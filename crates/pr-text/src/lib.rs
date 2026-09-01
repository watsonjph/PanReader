//! Novel documents: parse, sanitize, normalize. No rendering.
//!
//! The reader boundary in the build graph: this crate never depends on `pr-image` and
//! `pr-image` never depends on it. Tiling and scaled decode mean nothing to prose, and
//! pagination and font metrics mean nothing to a scanned page.
//!
//! The output is a normalized document, and it is parsed exactly once (hard invariant
//! 9). Changing font, size, measure or theme reflows that document in the browser; it
//! never comes back here. Re-parsing on a settings change is the text reader's version
//! of decoding at source size.

pub mod epub;
mod html;
mod plain;
pub mod scan;

pub use epub::Book;
pub use html::from_html;
pub use plain::from_plain;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a readable archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("malformed xml: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("{0} is not an EPUB: {1}")]
    NotAnEpub(std::path::PathBuf, &'static str),
    #[error("{0} has no readable chapters")]
    Empty(std::path::PathBuf),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A chapter, normalized.
///
/// Blocks rather than markup: an extension, a scraper or a publisher's XHTML never
/// reaches the screen as itself. What survives is text, three kinds of emphasis, and
/// the block's role -- which is everything a novel needs and nothing that can carry a
/// script tag.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Document {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Block {
    pub kind: Kind,
    pub spans: Vec<Span>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Para,
    /// 1 to 6, as written.
    Heading(u8),
    Quote,
    /// A scene break. Carries no text; the reader draws it.
    Divider,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub text: String,
    /// Two flags rather than an enum: they nest, and a `<em>` inside a `<strong>` is
    /// ordinary in typeset prose.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub em: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strong: bool,
}

impl Block {
    /// The block's text with no emphasis, which is what a character offset counts
    /// against. Position must not move when a `<em>` is added or a span is split.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.kind != Kind::Divider && self.spans.iter().all(|s| s.text.trim().is_empty())
    }
}

impl Document {
    /// Characters in the whole document. The reader shows it as a length and uses it to
    /// place a progress bar; nothing depends on it being a word count.
    pub fn chars(&self) -> usize {
        self.blocks.iter().map(|b| b.text().chars().count()).sum()
    }

    /// Content-derived identity, the same idea as a chapter of pages.
    ///
    /// Over the normalized text rather than the source bytes: two releases of the same
    /// translation that differ only in stylesheet are the same chapter to a reader, and
    /// treating them as different would lose their place for no reason.
    pub fn identity(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for block in &self.blocks {
            hasher.update(block.text().as_bytes());
            hasher.update(b"\n");
        }
        format!("blake3:{}", hasher.finalize().to_hex())
    }

    /// The first heading, for a chapter with no title in the table of contents.
    pub fn heading(&self) -> Option<String> {
        self.blocks
            .iter()
            .find(|b| matches!(b.kind, Kind::Heading(_)))
            .map(Block::text)
            .filter(|t| !t.trim().is_empty())
    }
}

#[cfg(test)]
mod tests;
