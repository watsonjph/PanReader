//! Fetching OPDS catalogs.
//!
//! The split is deliberate and matches the rule extensions live under: this module
//! performs every request, `pr-opds` only parses what comes back. When C1 lands a
//! plugin host, sources reuse this rather than getting a network API of their own
//! (`CLAUDE.md`, Sources and plugins).

use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
use url::Url;

/// One page of a catalog, with every href already absolute.
///
/// Feeds mix absolute and relative hrefs freely, and a relative one is meaningless once
/// it has left the response it arrived in. Resolving here means nothing downstream --
/// the UI, the downloader -- has to carry the base URL around.
#[derive(Debug, serde::Serialize)]
pub struct Page {
    pub url: String,
    pub feed: pr_opds::Feed,
}

fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("PanReader/", env!("CARGO_PKG_VERSION")))
        // A catalog that hangs must not hang the browse.
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("could not build an http client")
}

/// Only ever http(s). A feed that hands back a `file://` href must not turn into a
/// local file read.
fn checked(base: &Url, href: &str) -> anyhow::Result<Url> {
    let joined = base
        .join(href)
        .with_context(|| format!("bad url: {href}"))?;
    if !matches!(joined.scheme(), "http" | "https") {
        bail!("refusing a {} url from a feed: {joined}", joined.scheme());
    }
    Ok(joined)
}

fn absolutise(feed: &mut pr_opds::Feed, base: &Url) {
    let fix = |href: &mut String| {
        if let Ok(url) = checked(base, href) {
            *href = url.to_string();
        }
    };
    if let Some(next) = feed.next.as_mut() {
        fix(next);
    }
    for entry in &mut feed.entries {
        if let Some(thumb) = entry.thumbnail.as_mut() {
            fix(thumb);
        }
        match &mut entry.kind {
            pr_opds::EntryKind::Navigation { href } => fix(href),
            pr_opds::EntryKind::Publication { downloads } => {
                for d in downloads {
                    fix(&mut d.href);
                }
            }
        }
    }
}

/// Fetch and parse one feed.
pub async fn browse(url: &str) -> anyhow::Result<Page> {
    let base = Url::parse(url).with_context(|| format!("bad catalog url: {url}"))?;
    if !matches!(base.scheme(), "http" | "https") {
        bail!("a catalog must be an http or https address");
    }

    let response = client()?
        .get(base.clone())
        .header(
            reqwest::header::ACCEPT,
            "application/atom+xml, application/opds+json, */*",
        )
        .send()
        .await
        .with_context(|| format!("could not reach {base}"))?;

    // Invariant 13: a server that wants credentials is reported, never negotiated with.
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("{base} requires a login, which PanReader does not do");
    }
    let response = response
        .error_for_status()
        .with_context(|| format!("{base} refused the request"))?;

    let bytes = response.bytes().await.context("the feed cut off")?;
    let mut feed = pr_opds::parse(&bytes).with_context(|| format!("{base} is not an OPDS feed"))?;
    absolutise(&mut feed, &base);

    Ok(Page {
        url: base.to_string(),
        feed,
    })
}

/// Strip anything that is not a filename.
///
/// The name comes from a remote feed, so it is untrusted: without this a title of
/// `../../autorun.inf` would write outside the library root.
fn safe_name(title: &str, mime: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " -_.,()!&".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    let stem = if trimmed.is_empty() {
        "download"
    } else {
        trimmed
    };

    let ext = match mime {
        m if m.contains("comicbook+zip") || m.contains("/zip") => "cbz",
        m if m.contains("epub") => "epub",
        m if m.contains("pdf") => "pdf",
        _ => "bin",
    };
    // Long names break on Windows well before the path limit does.
    let stem: String = stem.chars().take(120).collect();
    format!("{stem}.{ext}")
}

/// Download a publication into a library root.
///
/// Two properties matter more than speed here. The file lands under a `.part` name and
/// is renamed only once the body is complete, so a scan can never pick up a truncated
/// archive and record it as a chapter. And a retry resumes with a range request rather
/// than starting over, because these are tens of megabytes over connections that drop.
pub async fn download(href: &str, title: &str, mime: &str, root: &Path) -> anyhow::Result<PathBuf> {
    let url = Url::parse(href).with_context(|| format!("bad download url: {href}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("refusing to download from a {} url", url.scheme());
    }

    let final_path = root.join(safe_name(title, mime));
    if final_path.exists() {
        return Ok(final_path);
    }
    let part = final_path.with_extension("part");
    let client = client()?;

    let mut attempt = 0;
    loop {
        attempt += 1;
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

        let mut request = client.get(url.clone());
        if have > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }

        let result = async {
            let response = request.send().await?.error_for_status()?;
            // A server that ignores the range restarts the body at zero, so the partial
            // file has to go rather than be appended to.
            let resuming = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
            let body = response.bytes().await?;
            Ok::<_, reqwest::Error>((resuming, body))
        }
        .await;

        match result {
            Ok((resuming, body)) => {
                use std::io::Write;
                let appending = resuming && have > 0;
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(appending)
                    .write(!appending)
                    .truncate(!appending)
                    .open(&part)?;
                file.write_all(&body)?;
                file.sync_all()?;
                drop(file);
                std::fs::rename(&part, &final_path)?;
                tracing::info!(path = %final_path.display(), "downloaded");
                return Ok(final_path);
            }
            Err(e) if attempt < 3 => {
                // Linear backoff. Three tries against one server is politeness, not a
                // retry policy worth tuning.
                tracing::warn!("download attempt {attempt} failed, retrying: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(attempt)).await;
            }
            Err(e) => {
                // The partial stays behind on purpose: the next attempt resumes from it.
                return Err(anyhow::Error::new(e).context(format!("could not download {url}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_name_cannot_escape_the_library_root() {
        // Separators become underscores and leading dots are trimmed, so what is left
        // is a name inside the root rather than a route out of it.
        assert_eq!(
            safe_name("../../autorun", "application/vnd.comicbook+zip"),
            "_.._autorun.cbz"
        );
        assert_eq!(safe_name("C:\\evil", "application/zip"), "C__evil.cbz");

        for hostile in ["../../../etc/passwd", "..\\..\\windows", "/absolute", "a/b"] {
            let name = safe_name(hostile, "application/zip");
            let joined = Path::new("root").join(&name);
            assert_eq!(
                joined.parent(),
                Some(Path::new("root")),
                "{hostile} escaped as {name}"
            );
        }
        assert_eq!(safe_name("", "application/epub+zip"), "download.epub");
        assert_eq!(safe_name("...", "application/pdf"), "download.pdf");
        assert_eq!(
            safe_name("Yotsuba&! Vol. 1", "application/vnd.comicbook+zip"),
            "Yotsuba&! Vol. 1.cbz",
            "ordinary titles keep their punctuation"
        );
    }

    #[test]
    fn a_feed_cannot_point_us_at_the_local_filesystem() {
        let base = Url::parse("https://books.example/opds").unwrap();
        assert!(checked(&base, "file:///etc/passwd").is_err());
        assert!(checked(&base, "/opds/series?page=2").is_ok());
        assert_eq!(
            checked(&base, "/opds/series").unwrap().as_str(),
            "https://books.example/opds/series"
        );
    }
}
