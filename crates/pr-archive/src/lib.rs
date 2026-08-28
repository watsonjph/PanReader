//! Container -> ordered page bytes. Nothing here decodes an image, and nothing here
//! writes to the user's library (hard invariant 5: source files are read-only to us).

use std::cmp::Ordering;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("no pages found in {0}")]
    Empty(PathBuf),
    #[error("page {0} out of range")]
    OutOfRange(usize),
}

pub type Result<T> = std::result::Result<T, Error>;

const EXTS: [&str; 6] = ["jpg", "jpeg", "png", "webp", "gif", "bmp"];

fn is_page(name: &str) -> bool {
    if name.contains("__MACOSX") || name.rsplit('/').next().is_some_and(|f| f.starts_with('.')) {
        return false;
    }
    name.rsplit('.')
        .next()
        .is_some_and(|e| EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// `page10.jpg` must sort after `page2.jpg`. Compares digit runs numerically and
/// everything else case-insensitively, without allocating.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.bytes().peekable(), b.bytes().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let (mut na, mut nb) = (0u128, 0u128);
                    while x.peek().is_some_and(|c| c.is_ascii_digit()) {
                        na = na.saturating_mul(10) + (x.next().unwrap() - b'0') as u128;
                    }
                    while y.peek().is_some_and(|c| c.is_ascii_digit()) {
                        nb = nb.saturating_mul(10) + (y.next().unwrap() - b'0') as u128;
                    }
                    match na.cmp(&nb) {
                        Ordering::Equal => continue,
                        ord => return ord,
                    }
                }
                match ca.to_ascii_lowercase().cmp(&cb.to_ascii_lowercase()) {
                    Ordering::Equal => {
                        x.next();
                        y.next();
                    }
                    ord => return ord,
                }
            }
        }
    }
}

/// An ordered set of page images. Cheap to clone-free share across threads: every
/// read opens its own handle rather than sharing a cursor.
#[derive(Debug)]
pub enum PageSource {
    Dir(Vec<PathBuf>),
    Zip { path: PathBuf, names: Vec<String> },
}

impl PageSource {
    pub fn open(path: &Path) -> Result<Self> {
        let src = if path.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(path)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && p.to_str().is_some_and(is_page))
                .collect();
            files.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
            PageSource::Dir(files)
        } else {
            let mut names: Vec<String> = zip::ZipArchive::new(File::open(path)?)?
                .file_names()
                .filter(|n| is_page(n))
                .map(str::to_owned)
                .collect();
            names.sort_by(|a, b| natural_cmp(a, b));
            PageSource::Zip {
                path: path.to_owned(),
                names,
            }
        };
        if src.is_empty() {
            return Err(Error::Empty(path.to_owned()));
        }
        Ok(src)
    }

    pub fn len(&self) -> usize {
        match self {
            PageSource::Dir(f) => f.len(),
            PageSource::Zip { names, .. } => names.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn name(&self, i: usize) -> Option<String> {
        match self {
            PageSource::Dir(f) => f.get(i).map(|p| p.to_string_lossy().into_owned()),
            PageSource::Zip { names, .. } => names.get(i).cloned(),
        }
    }

    /// First `max` bytes of every page, in order, for header probing at open time.
    /// Opens the archive once instead of once per page: a 200-page CBZ pays one
    /// central-directory parse, not two hundred.
    pub fn read_prefixes(&self, max: usize) -> Result<Vec<Vec<u8>>> {
        match self {
            PageSource::Dir(files) => files
                .iter()
                .map(|p| {
                    let mut buf = Vec::new();
                    File::open(p)?.take(max as u64).read_to_end(&mut buf)?;
                    Ok(buf)
                })
                .collect(),
            PageSource::Zip { path, names } => {
                let mut zip = zip::ZipArchive::new(File::open(path)?)?;
                names
                    .iter()
                    .map(|name| {
                        let mut buf = Vec::new();
                        zip.by_name(name)?.take(max as u64).read_to_end(&mut buf)?;
                        Ok(buf)
                    })
                    .collect()
            }
        }
    }

    /// Streamed out of the archive, never extracted to disk.
    pub fn read(&self, i: usize) -> Result<Vec<u8>> {
        match self {
            PageSource::Dir(files) => Ok(std::fs::read(files.get(i).ok_or(Error::OutOfRange(i))?)?),
            PageSource::Zip { path, names } => {
                let name = names.get(i).ok_or(Error::OutOfRange(i))?;
                let mut zip = zip::ZipArchive::new(File::open(path)?)?;
                let mut entry = zip.by_name(name)?;
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut buf)?;
                Ok(buf)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_sort_numerically() {
        let mut v = vec![
            "page10.jpg",
            "page2.jpg",
            "page1.jpg",
            "page20.jpg",
            "page3.jpg",
        ];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            [
                "page1.jpg",
                "page2.jpg",
                "page3.jpg",
                "page10.jpg",
                "page20.jpg"
            ]
        );
    }

    #[test]
    fn handles_padding_prefixes_and_case() {
        let mut v = vec!["ch2/p9.png", "ch10/p1.png", "ch2/p10.png", "CH1/p1.png"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            ["CH1/p1.png", "ch2/p9.png", "ch2/p10.png", "ch10/p1.png"]
        );
        assert_eq!(natural_cmp("007", "7"), Ordering::Equal);
        assert_eq!(natural_cmp("a", "ab"), Ordering::Less);
    }

    #[test]
    fn junk_entries_are_not_pages() {
        assert!(is_page("x/page1.JPG"));
        assert!(!is_page("__MACOSX/._page1.jpg"));
        assert!(!is_page("x/.hidden.jpg"));
        assert!(!is_page("ComicInfo.xml"));
    }
}
