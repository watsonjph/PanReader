//! Walking a library root to find what is in it.
//!
//! Produces plain data. Nothing here knows about the database, and nothing here writes
//! anything: a library root is read-only to us (hard invariant 5).

use crate::{Error, PageSource, natural_cmp};
use std::path::{Path, PathBuf};

/// Bytes of the first page mixed into a chapter's identity. Enough to distinguish two
/// chapters with the same page count, cheap enough to read ten thousand times.
const IDENTITY_PREFIX: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ScannedChapter {
    pub path: PathBuf,
    pub title: String,
    /// Parsed from the name where one is there. Real, because 10.5 happens.
    pub number: Option<f64>,
    pub page_count: usize,
    /// Content-derived, so renaming the file keeps the reader's progress.
    pub identity: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScannedSeries {
    pub path: PathBuf,
    pub title: String,
    pub chapters: Vec<ScannedChapter>,
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned()
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned()
}

fn is_archive(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "cbz" | "zip"))
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

/// A chapter number out of a name.
///
/// ponytail: the last number in the string, which handles `Chapter 12`, `c012.5`,
/// `Vol 1 Ch 3` and `Series 2 - 014` correctly because the chapter number is
/// conventionally last. It gets `2020` from `Series (2020)` wrong. Replace it with real
/// filename metadata parsing when Phase 2's ComicInfo work lands, not before.
pub fn chapter_number(name: &str) -> Option<f64> {
    let bytes = name.as_bytes();
    let mut last = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            // A single decimal point, and only when a digit follows it.
            if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            last = name[start..i].parse::<f64>().ok().or(last);
        } else {
            i += 1;
        }
    }
    last
}

/// Identity of a chapter, from its content rather than its path.
///
/// Page count plus the head of the first page. Hashing every page would be exact and
/// unaffordable — a ten thousand chapter library would read gigabytes to answer a
/// question that a few kilobytes settles.
fn identity(src: &PageSource) -> Result<String, Error> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(src.len() as u64).to_le_bytes());
    hasher.update(&src.read_page_prefix(0, IDENTITY_PREFIX)?);
    Ok(format!("blake3:{}", &hasher.finalize().to_hex()[..32]))
}

fn chapter_at(path: &Path) -> Option<ScannedChapter> {
    let src = match PageSource::open(path) {
        Ok(src) => src,
        // One unreadable folder must not end the scan. This is the same call as a
        // corrupt page inside a chapter: skip it, say so, carry on.
        Err(e) => {
            tracing::debug!(path = %path.display(), "not a chapter: {e}");
            return None;
        }
    };
    let title = if path.is_dir() {
        file_name(path)
    } else {
        stem(path)
    };
    let identity = identity(&src)
        .inspect_err(|e| tracing::warn!(path = %path.display(), "no identity: {e}"))
        .ok()?;

    Some(ScannedChapter {
        number: chapter_number(&title),
        path: path.to_owned(),
        title,
        page_count: src.len(),
        identity,
    })
}

/// Chapters directly inside a series directory: archives, or subdirectories of images.
fn chapters_in(dir: &Path) -> Vec<ScannedChapter> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| !is_hidden(p) && (p.is_dir() || is_archive(p)))
        .collect();
    paths.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    paths.iter().filter_map(|p| chapter_at(p)).collect()
}

/// Everything under one library root.
///
/// The layout is the one Mihon's local source uses and the one people already have:
/// a root holds series, and a series holds chapters. The exception worth handling is a
/// series directory containing images directly, which is one chapter and is exactly how
/// a single downloaded volume arrives.
pub fn scan_root(root: &Path) -> Vec<ScannedSeries> {
    let Ok(entries) = std::fs::read_dir(root) else {
        tracing::warn!(root = %root.display(), "library root could not be read");
        return Vec::new();
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| !is_hidden(p))
        .collect();
    paths.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));

    let mut out = Vec::new();
    for path in paths {
        if is_archive(&path) {
            // A loose archive in the root is a series of one.
            if let Some(chapter) = chapter_at(&path) {
                out.push(ScannedSeries {
                    title: stem(&path),
                    path: path.clone(),
                    chapters: vec![chapter],
                });
            }
            continue;
        }
        if !path.is_dir() {
            continue;
        }

        // Images directly inside: the directory is itself a single chapter.
        if let Some(chapter) = chapter_at(&path) {
            out.push(ScannedSeries {
                title: file_name(&path),
                path: path.clone(),
                chapters: vec![chapter],
            });
            continue;
        }

        let chapters = chapters_in(&path);
        if !chapters.is_empty() {
            out.push(ScannedSeries {
                title: file_name(&path),
                path,
                chapters,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        // A minimal valid JPEG header is enough: the scanner counts and hashes, it does
        // not decode.
        std::fs::write(dir.join(name), [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]).unwrap();
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn chapter_numbers_come_from_the_last_number_in_the_name() {
        assert_eq!(chapter_number("Chapter 12"), Some(12.0));
        assert_eq!(chapter_number("c012.5"), Some(12.5));
        assert_eq!(chapter_number("Vol 1 Ch 3"), Some(3.0));
        assert_eq!(chapter_number("Series 2 - 014"), Some(14.0));
        assert_eq!(chapter_number("no digits here"), None);
        // Documented wrong answer: the year wins. Real metadata fixes this, not a
        // cleverer regex.
        assert_eq!(chapter_number("Series (2020)"), Some(2020.0));
    }

    #[test]
    fn a_series_of_chapter_folders_is_found_in_order() {
        let root = tmp("pr_scan_folders");
        for ch in ["Chapter 1", "Chapter 2", "Chapter 10"] {
            page(&root.join("My Series").join(ch), "p1.jpg");
            page(&root.join("My Series").join(ch), "p2.jpg");
        }

        let series = scan_root(&root);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].title, "My Series");
        let titles: Vec<&str> = series[0]
            .chapters
            .iter()
            .map(|c| c.title.as_str())
            .collect();
        assert_eq!(
            titles,
            ["Chapter 1", "Chapter 2", "Chapter 10"],
            "natural order"
        );
        assert_eq!(series[0].chapters[2].number, Some(10.0));
        assert_eq!(series[0].chapters[0].page_count, 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// How a downloaded volume actually arrives, and how the Yotsuba fixture is laid out.
    #[test]
    fn a_directory_of_images_is_a_series_with_one_chapter() {
        let root = tmp("pr_scan_flat");
        page(&root.join("Yotsubato Vol 1"), "p001.jpg");
        page(&root.join("Yotsubato Vol 1"), "p002.jpg");

        let series = scan_root(&root);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].title, "Yotsubato Vol 1");
        assert_eq!(series[0].chapters.len(), 1);
        assert_eq!(series[0].chapters[0].page_count, 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn identity_survives_a_rename_and_changes_with_content() {
        let root = tmp("pr_scan_identity");
        page(&root.join("Before"), "p1.jpg");
        let before = scan_root(&root).remove(0).chapters.remove(0).identity;

        std::fs::rename(root.join("Before"), root.join("After")).unwrap();
        let after = scan_root(&root).remove(0).chapters.remove(0).identity;
        assert_eq!(
            before, after,
            "renaming must not lose the reader's progress"
        );

        page(&root.join("After"), "p2.jpg");
        let grown = scan_root(&root).remove(0).chapters.remove(0).identity;
        assert_ne!(
            before, grown,
            "a chapter that gained a page is not the same chapter"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn junk_and_unreadable_entries_are_skipped_rather_than_fatal() {
        let root = tmp("pr_scan_junk");
        page(&root.join("Real Series"), "p1.jpg");
        std::fs::create_dir_all(root.join("Empty Folder")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        page(&root.join(".hidden"), "p1.jpg");
        std::fs::write(root.join("notes.txt"), b"hello").unwrap();

        let series = scan_root(&root);
        assert_eq!(series.len(), 1, "only the real series survives");
        assert_eq!(series[0].title, "Real Series");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_root_yields_nothing_instead_of_panicking() {
        assert!(scan_root(Path::new("/definitely/not/a/real/path")).is_empty());
    }
}
