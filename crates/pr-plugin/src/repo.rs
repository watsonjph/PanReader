//! Repository indexes: what a repository offers, and whether a bundle is what it said.
//!
//! No I/O. The host fetches the index and the bundles; this decides what they mean and
//! whether to trust them, which keeps the whole thing testable against a string.
//!
//! **We host no repository and bundle no extension.** A repository is a URL the reader
//! chose, and everything here is about reading someone else's file carefully.

use crate::{Error, Kind, Result};
use sha2::{Digest, Sha256};

/// One source a repository offers, before it is installed.
///
/// Deliberately not a `Manifest`: this is what the index *claims*, and the manifest is
/// what the bundle actually declares. They are checked against each other at install
/// time, because an index that could widen a plugin's reach without changing its code
/// would be a hole rather than a convenience.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Listed {
    pub id: String,
    pub name: String,
    pub version: String,
    pub lang: String,
    pub kind: Kind,
    pub nsfw: bool,
    /// Absolute, already resolved against the index's own URL.
    pub bundle: String,
    /// Absent in LNReader's index, which is why installing cannot require one.
    pub sha256: Option<String>,
    /// Shown next to the name so a reader can tell the two ecosystems apart.
    pub foreign: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Index {
    pub sources: Vec<Listed>,
}

/// Read an index, ours or LNReader's.
///
/// Sniffed on the first non-space byte rather than on the URL: ours is an object with a
/// schema, theirs is a bare array. A repository that renamed its file would otherwise
/// stop working for no reason the reader could see.
pub fn parse_index(json: &str, index_url: &str) -> Result<Index> {
    let base = url::Url::parse(index_url)
        .map_err(|_| Error::Manifest(format!("{index_url} is not a URL")))?;

    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| Error::Manifest(format!("this is not a repository index: {e}")))?;

    let sources = match &value {
        serde_json::Value::Array(items) => {
            // A Mihon or Aniyomi repository is also a bare JSON array, so it parses
            // this far and then yields nothing -- which reads as "the repository is
            // empty" when the truth is that its extensions are Android APKs. Saying so
            // is the difference between a dead end and an answer.
            if items
                .iter()
                .any(|item| item.get("apk").is_some() || item.get("pkg").is_some())
            {
                return Err(Error::Manifest(
                    "this is a Mihon or Aniyomi repository. Those extensions are                      compiled Android apps and cannot run here. Run Suwayomi and add                      it as a catalog instead, or use an LNReader repository for novels"
                        .into(),
                ));
            }
            items
                .iter()
                .filter_map(|item| lnreader(item, &base))
                .collect()
        }
        serde_json::Value::Object(_) => {
            let listed = value
                .get("sources")
                .and_then(|s| s.as_array())
                .ok_or_else(|| {
                    Error::Manifest(
                        "this JSON has no \"sources\" array. A PanReader repository is                          { \"schema\": 1, \"sources\": [ ... ] }"
                            .into(),
                    )
                })?;
            listed.iter().filter_map(|item| ours(item, &base)).collect()
        }
        _ => {
            return Err(Error::Manifest(
                "the index is neither an object nor a list".into(),
            ));
        }
    };

    Ok(Index { sources })
}

/// Our own index entry. A malformed one is skipped rather than failing the repository:
/// one bad row should not hide the forty good ones next to it.
fn ours(item: &serde_json::Value, base: &url::Url) -> Option<Listed> {
    let kind = match item.get("kind").and_then(|k| k.as_str())? {
        "manga" => Kind::Manga,
        "novel" => Kind::Novel,
        _ => return None,
    };
    Some(Listed {
        id: text(item, "id")?,
        name: text(item, "name").unwrap_or_else(|| text(item, "id").unwrap_or_default()),
        version: text(item, "version").unwrap_or_else(|| "0".into()),
        lang: text(item, "lang").unwrap_or_else(|| "en".into()),
        kind,
        nsfw: item.get("nsfw").and_then(|n| n.as_bool()).unwrap_or(false),
        bundle: base.join(&text(item, "bundle")?).ok()?.to_string(),
        sha256: text(item, "sha256"),
        foreign: false,
    })
}

/// An LNReader plugin, read as one of ours.
///
/// A manifest shim rather than a second host, because there is nothing to host
/// differently: their plugins are JavaScript published to an index at a repository URL,
/// which is the same distribution shape and nearly the same interface. Their entries
/// carry `url` rather than `bundle`, no `kind` because every one of them is a novel
/// source, and no hash.
fn lnreader(item: &serde_json::Value, base: &url::Url) -> Option<Listed> {
    let bundle = text(item, "url")?;
    Some(Listed {
        id: text(item, "id")?,
        name: text(item, "name").unwrap_or_else(|| text(item, "site").unwrap_or_default()),
        version: text(item, "version").unwrap_or_else(|| "0".into()),
        lang: text(item, "lang").unwrap_or_else(|| "en".into()),
        kind: Kind::Novel,
        // Their index does not say, and guessing "safe" is the wrong direction to be
        // wrong in.
        nsfw: true,
        bundle: base.join(&bundle).ok()?.to_string(),
        sha256: None,
        foreign: true,
    })
}

fn text(item: &serde_json::Value, key: &str) -> Option<String> {
    item.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// Whether a bundle is the one the index described.
///
/// Only checked when the index offered a hash. LNReader's does not, and refusing every
/// plugin from the ecosystem this shim exists to support would be a strange way to
/// support it -- the sandbox is what makes an unverified bundle survivable, and the
/// reader is told which repositories publish hashes.
pub fn verify(bundle: &[u8], expected: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bundle));
    let expected = expected.trim().to_ascii_lowercase();
    if actual == expected {
        return Ok(());
    }
    Err(Error::Manifest(format!(
        "this bundle is not the one the repository listed \
         (expected {expected}, got {actual})"
    )))
}

/// What the index claimed against what the bundle declares.
///
/// The bundle wins everywhere it disagrees, because the bundle is what runs. This only
/// refuses the disagreements that would make the install a lie: a different id would
/// take over another source's library entries, and a different kind would file a novel
/// under the image reader.
pub fn agrees(listed: &Listed, manifest: &crate::Manifest) -> Result<()> {
    if listed.id != manifest.id {
        return Err(Error::Manifest(format!(
            "the repository lists this as {} but the bundle calls itself {}",
            listed.id, manifest.id
        )));
    }
    if listed.kind != manifest.kind {
        return Err(Error::Manifest(format!(
            "the repository lists {} as a {:?} source but the bundle is {:?}",
            listed.id, listed.kind, manifest.kind
        )));
    }
    Ok(())
}
