//! Walking a library root for books.
//!
//! The text half of what `pr-archive::scan` does for pages, and deliberately not the
//! same function: an EPUB is one file holding a whole series, where a CBZ is one file
//! holding one chapter. Sharing a walker would mean a walker that knows both, and the
//! reader boundary is worth more than the twenty lines it would save.
//!
//! Produces plain data. Nothing here writes anything: a library root is read-only to us
//! (hard invariant 5).

use crate::epub;
use std::path::{Path, PathBuf};

/// One book, as a scan sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Scanned {
    pub path: PathBuf,
    pub title: String,
    pub author: String,
    pub chapters: Vec<Chapter>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    /// The container. For an EPUB every chapter shares one; for loose text files it is
    /// the file itself.
    pub path: PathBuf,
    /// Where inside the container, empty when the file *is* the chapter.
    pub locator: String,
    pub title: String,
    pub number: Option<f64>,
    pub identity: String,
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_owned()
}

pub fn is_book(path: &Path) -> bool {
    extension(path) == "epub"
}

pub fn is_prose(path: &Path) -> bool {
    matches!(extension(path).as_str(), "txt" | "md" | "markdown")
}

/// Every book under a root.
///
/// ponytail: no stamp cache, unlike the image scanner. An EPUB is opened and its spine
/// entries hashed on every scan, which is a few milliseconds a book -- fine for a shelf
/// of novels, not fine for five thousand. Cache on the book's mtime and size when
/// someone has that many; the shape is already there in `pr_archive::scan::Known`.
pub fn scan_root(root: &Path) -> Vec<Scanned> {
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out.sort_by(|a, b| a.title.cmp(&b.title));
    out
}

fn walk(dir: &Path, depth: u32, out: &mut Vec<Scanned>) {
    // Deep enough for root/author/series, and shallow enough that pointing the app at a
    // home directory does not walk the whole disk.
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();

    let mut prose: Vec<PathBuf> = Vec::new();
    for path in paths {
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        if path.is_dir() {
            walk(&path, depth + 1, out);
        } else if is_book(&path) {
            match book(&path) {
                Ok(scanned) => out.push(scanned),
                Err(e) => tracing::warn!("skipping {}: {e}", path.display()),
            }
        } else if is_prose(&path) {
            prose.push(path);
        }
    }

    // A folder of text files is one series of chapters, the same way a folder of CBZs
    // is. A single loose file is a series of one, which is what someone dropping one
    // chapter in expects to see.
    if !prose.is_empty() {
        let title = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Text")
            .to_owned();
        let mut chapters: Vec<Chapter> = prose.iter().filter_map(|p| loose(p)).collect();
        chapters.sort_by(|a, b| {
            a.number
                .partial_cmp(&b.number)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.title.cmp(&b.title))
        });
        if !chapters.is_empty() {
            out.push(Scanned {
                path: dir.to_owned(),
                title,
                author: String::new(),
                chapters,
            });
        }
    }
}

fn book(path: &Path) -> crate::Result<Scanned> {
    let book = epub::open(path)?;
    Ok(Scanned {
        chapters: book
            .chapters
            .iter()
            .enumerate()
            .map(|(n, spine)| Chapter {
                path: path.to_owned(),
                locator: spine.href.clone(),
                title: spine.title.clone(),
                // Spine position, not a number parsed out of the title: a book's
                // chapters are ordered by the spine and nothing else, and "Chapter
                // Twenty" has no digits to find.
                number: Some(n as f64 + 1.0),
                identity: spine.identity.clone(),
            })
            .collect(),
        title: book.title,
        author: book.author,
        path: path.to_owned(),
    })
}

fn loose(path: &Path) -> Option<Chapter> {
    let bytes = std::fs::read(path).ok()?;
    let title = stem(path);
    Some(Chapter {
        number: pr_core::chapter_number(&title),
        identity: format!("blake3:{}", blake3::hash(&bytes).to_hex()),
        path: path.to_owned(),
        locator: String::new(),
        title,
    })
}

/// Read one chapter, whatever kind of container it came from.
pub fn read(path: &Path, locator: &str) -> crate::Result<crate::Document> {
    if !locator.is_empty() {
        return epub::chapter(path, locator);
    }
    let raw = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&raw);
    Ok(crate::from_plain(&text, extension(path) != "txt"))
}
