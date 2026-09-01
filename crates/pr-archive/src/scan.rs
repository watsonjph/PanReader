//! Walking a library root to find what is in it.
//!
//! Produces plain data. Nothing here knows about the database, and nothing here writes
//! anything: a library root is read-only to us (hard invariant 5).

use crate::{Error, PageSource, natural_cmp};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Bytes of the first page mixed into a chapter's identity. Enough to distinguish two
/// chapters with the same page count, cheap enough to read ten thousand times.
const IDENTITY_PREFIX: usize = 64 * 1024;

/// What a previous scan learned about a path.
///
/// A rescan that finds the same modification time and size reuses this instead of
/// opening the chapter and hashing it, which is the whole cost of a scan.
#[derive(Debug, Clone, PartialEq)]
pub struct Cached {
    pub mtime: i64,
    pub size: u64,
    pub identity: String,
    pub page_count: usize,
    /// The conclusions of the last scan, not just its identity. A chapter titled and
    /// numbered from its ComicInfo.xml must not fall back to its filename on a rescan
    /// that never opens it.
    pub title: String,
    pub number: Option<f64>,
}

/// Path to what the last scan saw there.
pub type Known = HashMap<PathBuf, Cached>;

/// Modification time and size, the pair that says "unchanged" cheaply.
///
/// Nanoseconds, not seconds. NTFS keeps 100 ns and ext4 keeps nanoseconds, and at
/// one-second resolution a chapter added in the same second as a scan is invisible
/// until something else touches the folder — which is exactly when someone drops a new
/// download in and hits rescan.
///
/// ponytail: for a folder chapter this is the directory's own stamp, so adding or
/// removing a page is caught but overwriting one in place is not. Identity only reads
/// page zero, so the miss is narrower still: it takes replacing the first page to slip
/// through. Hash every page when that stops being acceptable, and pay for it everywhere.
fn stamp(path: &Path) -> Option<(i64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as i64;
    Some((mtime, meta.len()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScannedChapter {
    pub path: PathBuf,
    /// Where inside the container. Empty for a folder or a CBZ, where the chapter is
    /// the file; the spine entry for a chapter of an EPUB.
    pub locator: String,
    pub title: String,
    /// Parsed from the name where one is there. Real, because 10.5 happens.
    pub number: Option<f64>,
    pub page_count: usize,
    /// Content-derived, so renaming the file keeps the reader's progress.
    pub identity: String,
    /// Stored so the next scan can skip this chapter entirely.
    pub mtime: i64,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScannedSeries {
    pub path: PathBuf,
    pub title: String,
    pub author: String,
    /// Which reader opens it: `image` or `text`. A plain string rather than an enum
    /// because it is a column value on its way to SQLite, and `pr-archive` and `pr-text`
    /// both produce these without either knowing about the other.
    pub kind: &'static str,
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

/// A chapter number out of a name. Lives in `pr-core` because both readers name their
/// chapters the same way; re-exported here so the scanner reads as one module.
pub use pr_core::chapter_number;

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

fn chapter_at(path: &Path, known: &Known) -> Option<ScannedChapter> {
    let title = if path.is_dir() {
        file_name(path)
    } else {
        stem(path)
    };
    let (mtime, size) = stamp(path)?;

    // Unchanged since the last scan: everything below this point is I/O we already did.
    if let Some(hit) = known.get(path)
        && hit.mtime == mtime
        && hit.size == size
    {
        return Some(ScannedChapter {
            number: hit.number,
            path: path.to_owned(),
            locator: String::new(),
            title: hit.title.clone(),
            page_count: hit.page_count,
            identity: hit.identity.clone(),
            mtime,
            size,
        });
    }

    let src = match PageSource::open(path) {
        Ok(src) => src,
        // One unreadable folder must not end the scan. This is the same call as a
        // corrupt page inside a chapter: skip it, say so, carry on.
        Err(e) => {
            tracing::debug!(path = %path.display(), "not a chapter: {e}");
            return None;
        }
    };
    let identity = identity(&src)
        .inspect_err(|e| tracing::warn!(path = %path.display(), "no identity: {e}"))
        .ok()?;

    // Real metadata beats guessing at a filename. Deliberately not the series name:
    // the folder is what the reader organised and what they expect on the shelf, and
    // chapters of one series routinely disagree about <Series>, leaving no principled
    // winner. Number and title are per-chapter and have no such conflict.
    let info = src
        .read_sidecar("ComicInfo.xml")
        .and_then(|x| String::from_utf8(x).ok())
        .map(|x| pr_core::parse_comic_info(&x))
        .unwrap_or_default();

    Some(ScannedChapter {
        number: info.number.or_else(|| chapter_number(&title)),
        path: path.to_owned(),
        // A CBZ or a folder is one chapter, so the file is the whole address.
        locator: String::new(),
        title: info.title.unwrap_or(title),
        page_count: src.len(),
        identity,
        mtime,
        size,
    })
}

/// Chapters directly inside a series directory: archives, or subdirectories of images.
fn chapters_in(dir: &Path, known: &Known) -> Vec<ScannedChapter> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| !is_hidden(p) && (p.is_dir() || is_archive(p)))
        .collect();
    paths.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    paths.iter().filter_map(|p| chapter_at(p, known)).collect()
}

/// Everything under one library root.
///
/// The layout is the one Mihon's local source uses and the one people already have:
/// a root holds series, and a series holds chapters. The exception worth handling is a
/// series directory containing images directly, which is one chapter and is exactly how
/// a single downloaded volume arrives.
pub fn scan_root(root: &Path, known: &Known) -> Vec<ScannedSeries> {
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
            if let Some(chapter) = chapter_at(&path, known) {
                out.push(ScannedSeries {
                    title: stem(&path),
                    author: String::new(),
                    kind: "image",
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
        if let Some(chapter) = chapter_at(&path, known) {
            out.push(ScannedSeries {
                title: file_name(&path),
                author: String::new(),
                kind: "image",
                path: path.clone(),
                chapters: vec![chapter],
            });
            continue;
        }

        let chapters = chapters_in(&path, known);
        if !chapters.is_empty() {
            out.push(ScannedSeries {
                title: file_name(&path),
                author: String::new(),
                kind: "image",
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

        let series = scan_root(&root, &Known::new());
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

        let series = scan_root(&root, &Known::new());
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
        let before = scan_root(&root, &Known::new())
            .remove(0)
            .chapters
            .remove(0)
            .identity;

        std::fs::rename(root.join("Before"), root.join("After")).unwrap();
        let after = scan_root(&root, &Known::new())
            .remove(0)
            .chapters
            .remove(0)
            .identity;
        assert_eq!(
            before, after,
            "renaming must not lose the reader's progress"
        );

        page(&root.join("After"), "p2.jpg");
        let grown = scan_root(&root, &Known::new())
            .remove(0)
            .chapters
            .remove(0)
            .identity;
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

        let series = scan_root(&root, &Known::new());
        assert_eq!(series.len(), 1, "only the real series survives");
        assert_eq!(series[0].title, "Real Series");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The point of the cache is that a rescan does no I/O per chapter. Proving that
    /// directly would need an I/O counter; proving it by consequence is enough — a
    /// deliberately wrong cache entry is returned verbatim, which can only happen if
    /// the chapter was never opened.
    #[test]
    fn a_rescan_trusts_an_unchanged_stamp_and_re_reads_a_changed_one() {
        let root = tmp("pr_scan_cache");
        page(&root.join("S").join("Chapter 1"), "p1.jpg");

        let first = scan_root(&root, &Known::new());
        let seen = &first[0].chapters[0];
        assert!(seen.mtime > 0, "a scan stamps what it saw");

        let mut known = Known::new();
        known.insert(
            seen.path.clone(),
            Cached {
                mtime: seen.mtime,
                size: seen.size,
                identity: "blake3:from-the-cache".into(),
                page_count: 99,
                title: "From The Cache".into(),
                number: Some(7.0),
            },
        );

        let again = scan_root(&root, &known);
        assert_eq!(again[0].chapters[0].identity, "blake3:from-the-cache");
        assert_eq!(
            again[0].chapters[0].title, "From The Cache",
            "a rescan keeps what the last scan concluded, not the filename"
        );
        assert_eq!(again[0].chapters[0].number, Some(7.0));
        assert_eq!(
            again[0].chapters[0].page_count, 99,
            "an unchanged chapter is never opened"
        );

        // Adding a page moves the directory's mtime, so the stamp misses and the real
        // chapter is read again.
        page(&root.join("S").join("Chapter 1"), "p2.jpg");
        let fresh = scan_root(&root, &known);
        assert_eq!(fresh[0].chapters[0].page_count, 2);
        assert_ne!(fresh[0].chapters[0].identity, "blake3:from-the-cache");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn comicinfo_names_and_numbers_a_chapter_where_the_filename_cannot() {
        let root = tmp("pr_scan_comicinfo");
        // The documented failure of filename parsing: the year wins.
        let dir = root.join("S").join("Yotsuba (2020)");
        page(&dir, "p1.jpg");

        let bare = scan_root(&root, &Known::new());
        assert_eq!(bare[0].chapters[0].number, Some(2020.0), "filename only");

        std::fs::write(
            dir.join("ComicInfo.xml"),
            "<ComicInfo><Title>Danbo Arrives</Title><Number>4</Number>             <Series>Ignored On Purpose</Series></ComicInfo>",
        )
        .unwrap();

        let tagged = scan_root(&root, &Known::new());
        assert_eq!(tagged[0].chapters[0].number, Some(4.0));
        assert_eq!(tagged[0].chapters[0].title, "Danbo Arrives");
        assert_eq!(
            tagged[0].title, "S",
            "the series name stays the folder the reader organised"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_root_yields_nothing_instead_of_panicking() {
        assert!(scan_root(Path::new("/definitely/not/a/real/path"), &Known::new()).is_empty());
    }
}
