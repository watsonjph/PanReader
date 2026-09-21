//! Fetching a chapter so it can be read with the network off.
//!
//! The shape that keeps this small: a download does not create anything new. It gives
//! the chapter row that already exists a `path`, and from then on every other part of
//! the app opens it the way it opens a scanned file -- same identity, same position
//! row, same reader. Nothing downstream learns that downloads exist.
//!
//! That is also why **read state follows the chapter, not the copy**. Deleting a
//! download clears the path and the entry falls back to streaming, with progress
//! untouched: someone who freed disk space has not said they want to forget what they
//! read.
//!
//! Downloads live in the app data directory rather than in a library root. A library
//! root is read-only to us (hard invariant 5), and a CBZ written into one would be
//! found by the next scan and become a second, duplicate series.

use anyhow::{Context as _, bail};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where downloads go. Beside the database, like the cover cache.
pub fn dir() -> anyhow::Result<PathBuf> {
    let db = pr_db::default_path()?;
    Ok(db
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("downloads"))
}

/// Filesystem-safe, and short enough for Windows to accept the whole path.
fn safe(part: &str) -> String {
    let cleaned: String = part
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    let short: String = trimmed.chars().take(60).collect();
    if short.is_empty() {
        "untitled".to_owned()
    } else {
        short
    }
}

/// Fetch one page of a manga chapter.
///
/// The allowlist still applies. The URL came out of a plugin, and the fact that the
/// host is the one dereferencing it does not make it the plugin's business where it
/// points -- otherwise `content` would be a way to make the host fetch anything.
fn page_bytes(
    client: &reqwest::blocking::Client,
    manifest: &pr_plugin::Manifest,
    url: &str,
) -> anyhow::Result<Vec<u8>> {
    if !manifest.allows(url) {
        bail!("{} is not in {}'s manifest", url, manifest.id);
    }
    let response = client.get(url).send()?.error_for_status()?;
    Ok(response.bytes()?.to_vec())
}

/// Download a chapter and hand back the file it landed in.
///
/// Written to `.part` and renamed once complete, the same rule the OPDS download
/// follows: a truncated archive must never be presented as a chapter.
pub fn fetch(
    source: &crate::sources::Loaded,
    series_title: &str,
    chapter_title: &str,
    chapter_id: &str,
) -> anyhow::Result<PathBuf> {
    let content = source.content(chapter_id)?;
    let folder = dir()?
        .join(safe(&source.manifest.id))
        .join(safe(series_title));
    std::fs::create_dir_all(&folder)?;

    let (final_path, part) = match &content {
        pr_plugin::Content::Pages(_) => {
            let name = format!("{}.cbz", safe(chapter_title));
            (folder.join(&name), folder.join(format!("{name}.part")))
        }
        pr_plugin::Content::Html(_) => {
            let name = format!("{}.html", safe(chapter_title));
            (folder.join(&name), folder.join(format!("{name}.part")))
        }
    };

    match content {
        // Prose is one small file. Stored as the markup the source returned rather than
        // as our normalized document: normalization is cheap, versioned and improving,
        // and freezing last month's version of it into someone's disk would mean a
        // parser fix never reaching the chapters they already have.
        pr_plugin::Content::Html(html) => {
            std::fs::write(&part, html.as_bytes())?;
        }

        pr_plugin::Content::Pages(urls) => {
            if urls.is_empty() {
                bail!("{chapter_title} has no pages");
            }
            let client = reqwest::blocking::Client::builder()
                .user_agent(concat!("PanReader/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(60))
                .build()?;

            let file = std::fs::File::create(&part)?;
            let mut zip = zip::ZipWriter::new(file);
            // Stored, not deflated. These are already-compressed JPEGs and PNGs;
            // deflating them again costs CPU to save nothing.
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);

            for (index, url) in urls.iter().enumerate() {
                let bytes = page_bytes(&client, &source.manifest, url)
                    .with_context(|| format!("page {} of {chapter_title}", index + 1))?;
                let extension = url
                    .rsplit('.')
                    .next()
                    .filter(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
                    .unwrap_or("jpg");
                // Zero-padded so the natural sort in `pr-archive` puts page 10 after
                // page 9 rather than after page 1.
                zip.start_file(format!("{:04}.{extension}", index + 1), options)?;
                zip.write_all(&bytes)?;
            }
            zip.finish()?;
        }
    }

    std::fs::rename(&part, &final_path)?;
    tracing::info!(path = %final_path.display(), "downloaded a chapter");
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::safe;

    #[test]
    fn a_title_becomes_a_filename_without_escaping_its_folder() {
        assert_eq!(safe("Chapter 1"), "Chapter 1");
        // Separators go first, then the leading dots, so nothing is left that could
        // climb out of the folder this gets joined onto.
        assert_eq!(safe("../../etc/passwd"), "_.._etc_passwd");
        assert!(!safe("../../etc/passwd").contains(['/', '\\']));
        assert_eq!(safe("C:\\Windows\\System32"), "C__Windows_System32");
        assert_eq!(safe("what? yes!"), "what_ yes!");
        // Trailing dots and spaces are what Windows silently strips, which would make
        // two different chapters land on one path.
        assert_eq!(safe("Chapter 1.  "), "Chapter 1");
        assert_eq!(safe("   "), "untitled");
        assert!(safe(&"x".repeat(500)).len() <= 60);
    }
}
