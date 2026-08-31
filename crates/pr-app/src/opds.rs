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

        // Streamed to disk as it arrives, not buffered and then written. Buffering
        // whole loses everything on a dropped connection, which would leave nothing to
        // resume from and make the range request above dead code.
        let outcome: anyhow::Result<()> = async {
            use std::io::Write;
            let mut response = request.send().await?.error_for_status()?;
            // A server that ignores the range restarts the body at zero, so the partial
            // has to go rather than be appended to.
            let appending = response.status() == reqwest::StatusCode::PARTIAL_CONTENT && have > 0;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(appending)
                .write(!appending)
                .truncate(!appending)
                .open(&part)?;

            while let Some(chunk) = response.chunk().await? {
                file.write_all(&chunk)?;
            }
            file.sync_all()?;
            Ok(())
        }
        .await;

        match outcome {
            Ok(()) => {
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
                return Err(e.context(format!("could not download {url}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    /// What a scripted connection sends back.
    enum Reply {
        Full(&'static str),
        /// Headers claiming the whole length, then only the first `sent` bytes before
        /// the connection drops. What a real interrupted download looks like.
        Truncated(&'static str, usize),
        Status(u16),
    }

    /// A throwaway HTTP server on a loopback port.
    ///
    /// ponytail: stdlib TcpListener and a hand-written response, not a test-server
    /// dependency. What is under test is our client -- ranges, retries, the .part
    /// rename -- and that needs a server that misbehaves on cue, which a well-behaved
    /// one would not do.
    fn serve(script: Vec<Reply>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();

        std::thread::spawn(move || {
            for (reply, stream) in script.into_iter().zip(listener.incoming()) {
                let Ok(mut stream) = stream else { continue };

                // Read the request head. Every request here is a GET, so there is no
                // body to worry about.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while stream.read(&mut byte).unwrap_or(0) == 1 {
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head_text = String::from_utf8_lossy(&head).into_owned();
                recorded.lock().unwrap().push(head_text.clone());

                let _ = match reply {
                    Reply::Status(code) => stream.write_all(
                        format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\n\r\n").as_bytes(),
                    ),
                    Reply::Full(body) => {
                        // Honour a Range the way a real server does, so a resume gets
                        // the missing half rather than the whole file again.
                        let from = head_text.lines().find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("range: bytes=")
                                .and_then(|v| v.split('-').next()?.trim().parse::<usize>().ok())
                        });
                        match from {
                            Some(at) if at < body.len() => {
                                let rest = &body[at..];
                                stream
                                    .write_all(
                                        format!(
                                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\r\n",
                                            rest.len()
                                        )
                                        .as_bytes(),
                                    )
                                    .and_then(|()| stream.write_all(rest.as_bytes()))
                            }
                            _ => stream
                                .write_all(
                                    format!(
                                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                                        body.len()
                                    )
                                    .as_bytes(),
                                )
                                .and_then(|()| stream.write_all(body.as_bytes())),
                        }
                    }
                    Reply::Truncated(body, sent) => stream
                        .write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                                .as_bytes(),
                        )
                        .and_then(|()| stream.write_all(&body.as_bytes()[..sent])),
                };
                let _ = stream.flush();
            }
        });

        (format!("http://127.0.0.1:{port}"), seen)
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const FEED: &str = r##"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom"><title>Local</title>
  <entry>
    <title>Book</title><id>1</id>
    <link rel="http://opds-spec.org/image/thumbnail" href="/cover/1"/>
    <link rel="http://opds-spec.org/acquisition" href="/dl/1"
          type="application/vnd.comicbook+zip"/>
  </entry>
</feed>"##;

    #[test]
    fn browsing_a_real_server_resolves_relative_hrefs_against_the_feed() {
        let (base, _) = serve(vec![Reply::Full(FEED)]);
        let page = block_on(browse(&format!("{base}/opds"))).unwrap();

        assert_eq!(page.feed.title, "Local");
        let entry = &page.feed.entries[0];
        assert_eq!(
            entry.thumbnail.as_deref(),
            Some(format!("{base}/cover/1").as_str()),
            "a relative href is useless once it has left the response it arrived in"
        );
        match &entry.kind {
            pr_opds::EntryKind::Publication { downloads } => {
                assert_eq!(downloads[0].href, format!("{base}/dl/1"));
            }
            other => panic!("expected a publication, got {other:?}"),
        }
    }

    /// Invariant 13: a server that wants credentials is reported, never negotiated with.
    #[test]
    fn a_catalog_that_demands_a_login_says_so_plainly() {
        let (base, _) = serve(vec![Reply::Status(401)]);
        let err = block_on(browse(&format!("{base}/opds"))).unwrap_err();
        assert!(
            format!("{err:#}").contains("requires a login"),
            "got: {err:#}"
        );
    }

    #[test]
    fn a_download_is_renamed_into_place_only_once_it_is_complete() {
        let root = tmp("pr_opds_download");
        let (base, _) = serve(vec![Reply::Full("CBZBODY")]);

        let path = block_on(download(
            &format!("{base}/dl/1"),
            "Book One",
            "application/vnd.comicbook+zip",
            &root,
        ))
        .unwrap();

        assert_eq!(path, root.join("Book One.cbz"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "CBZBODY");
        assert!(
            !root.join("Book One.part").exists(),
            "the partial must not be left behind on success"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The property that matters: a scan must never find a truncated archive and record
    /// it as a chapter.
    #[test]
    fn an_interrupted_download_resumes_with_a_range_and_never_lands_truncated() {
        let root = tmp("pr_opds_resume");
        // The first connection dies after three bytes; the retry asks for the rest.
        let (base, seen) = serve(vec![Reply::Truncated("CBZBODY", 3), Reply::Full("CBZBODY")]);

        let path = block_on(download(
            &format!("{base}/dl/1"),
            "Book Two",
            "application/vnd.comicbook+zip",
            &root,
        ))
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "CBZBODY",
            "the resumed half must join the first, not replace or double it"
        );

        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2, "one failure, one resume");
        assert!(
            !requests[0].to_ascii_lowercase().contains("range:"),
            "the first attempt has nothing to resume from"
        );
        assert!(
            requests[1].to_ascii_lowercase().contains("range: bytes=3-"),
            "the retry must ask only for what is missing: {}",
            requests[1]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

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
