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
    #[error("{0} archives are not supported in this build, see the RAR note in CLAUDE.md")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;

const EXTS: [&str; 6] = ["jpg", "jpeg", "png", "webp", "gif", "bmp"];

/// Takes a bare file name, never a path.
///
/// It used to take either and split on `/` to find the file name, which is wrong on
/// Windows twice over: a `\`-separated path yields no split at all (so `.DS_Store`
/// slipped through), and a path with mixed separators — `pr-app/../..\data\p.jpg`,
/// which is exactly what `Path::join("../..")` builds — yields a "file name" starting
/// with `..`, so every page in the directory was discarded as a dotfile.
fn is_page(name: &str) -> bool {
    !name.starts_with('.')
        && name
            .rsplit('.')
            .next()
            .is_some_and(|e| EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Zip entry names are `/`-separated by specification, whatever the host OS, so the
/// file name can be taken directly. `__MACOSX` is a directory component, so it has to
/// be matched against the whole entry rather than the file name.
fn is_zip_page(entry: &str) -> bool {
    !entry.contains("__MACOSX") && entry.rsplit('/').next().is_some_and(is_page)
}

/// `page10.jpg` must sort after `page2.jpg`. Compares digit runs numerically and
/// everything else case-insensitively, without allocating.
fn natural_cmp(a: &str, b: &str) -> Ordering {
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
        // A CBR is a RAR, which the default build cannot read for licence reasons. Say
        // so by name: opening it as a zip fails with a corrupt-archive error that sends
        // the reader looking for a damaged download instead.
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && matches!(ext.to_ascii_lowercase().as_str(), "cbr" | "rar")
        {
            return Err(Error::Unsupported(ext.to_ascii_uppercase()));
        }

        let src = if path.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(path)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    // The file name, not the path: see `is_page`.
                    p.is_file() && p.file_name().and_then(|n| n.to_str()).is_some_and(is_page)
                })
                .collect();
            files.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
            PageSource::Dir(files)
        } else {
            let mut names: Vec<String> = zip::ZipArchive::new(File::open(path)?)?
                .file_names()
                .filter(|n| is_zip_page(n))
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

    /// A non-image file sitting alongside the pages, by name, case-insensitively.
    ///
    /// Used for ComicInfo.xml, which is the only widely-written source of reading
    /// direction. Returns None when it is absent, which is the common case.
    pub fn read_sidecar(&self, file_name: &str) -> Option<Vec<u8>> {
        match self {
            PageSource::Dir(files) => {
                let dir = files.first()?.parent()?;
                let entry = std::fs::read_dir(dir)
                    .ok()?
                    .filter_map(|e| e.ok())
                    .find(|e| {
                        e.file_name()
                            .to_str()
                            .is_some_and(|n| n.eq_ignore_ascii_case(file_name))
                    })?;
                std::fs::read(entry.path()).ok()
            }
            PageSource::Zip { path, .. } => {
                let mut zip = zip::ZipArchive::new(File::open(path).ok()?).ok()?;
                let name = zip
                    .file_names()
                    .find(|n| {
                        n.rsplit('/')
                            .next()
                            .is_some_and(|f| f.eq_ignore_ascii_case(file_name))
                    })?
                    .to_owned();
                let mut buf = Vec::new();
                zip.by_name(&name).ok()?.read_to_end(&mut buf).ok()?;
                Some(buf)
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
    fn a_rar_is_refused_by_name_rather_than_as_a_broken_zip() {
        let err = PageSource::open(Path::new("chapter.cbr")).unwrap_err();
        assert!(
            matches!(&err, Error::Unsupported(f) if f == "CBR"),
            "expected an unsupported-format error, got {err:?}"
        );
        assert!(
            err.to_string().contains("CLAUDE.md"),
            "the error should say where to look"
        );
        assert!(matches!(
            PageSource::open(Path::new("x.RAR")).unwrap_err(),
            Error::Unsupported(_)
        ));
    }

    #[test]
    fn junk_entries_are_not_pages() {
        assert!(is_page("page1.JPG"));
        assert!(!is_page(".hidden.jpg"));
        assert!(!is_page("ComicInfo.xml"));
        assert!(!is_page("cover"));

        assert!(is_zip_page("chapter/page1.jpg"));
        assert!(!is_zip_page("__MACOSX/._page1.jpg"));
        assert!(!is_zip_page("chapter/.DS_Store"));
    }

    /// Regression: `Path::join("../..")` builds a path whose separators are mixed on
    /// Windows, and the old filename split turned every page in the directory into a
    /// "dotfile".
    ///
    /// The path is built the way the app builds it rather than written as a literal.
    /// A hardcoded `C:\...` string is a Windows path only on Windows: elsewhere `\` is
    /// an ordinary filename character, `file_name()` keeps the whole thing, and the test
    /// fails for a reason that says nothing about the code. Joining reproduces whatever
    /// shape the platform actually produces, which is the shape that broke.
    #[test]
    fn joined_relative_paths_still_find_their_pages() {
        let joined = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("library")
            .join("vol 1")
            .join("p001.jpg");
        let name = joined
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        assert!(
            is_page(name),
            "{} was rejected via file name {name:?}",
            joined.display()
        );
    }
}
