//! EPUB 2 and 3, read straight out of the zip.
//!
//! Never extracted to a temp directory (hard invariant 5, and the same rule the image
//! reader follows for CBZ): a book is opened, the one entry that is wanted is read, and
//! the handle is dropped. Opening a book to list its chapters therefore costs the
//! container, the package document and the table of contents -- three small entries --
//! and not the megabyte of prose behind them.

use crate::{Document, Error, Result};
use quick_xml::events::Event;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// A book's shape: what it is called and what is in it, with no chapter text read.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Book {
    pub path: PathBuf,
    pub title: String,
    pub author: String,
    pub language: String,
    /// Spine order, which is reading order. The table of contents supplies names; it
    /// does not supply the order, because a nav document is allowed to skip entries and
    /// the spine is not.
    pub chapters: Vec<Spine>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Spine {
    /// The zip entry, resolved against the package document's own folder.
    pub href: String,
    pub title: String,
}

/// Read the container, the package document and the table of contents.
pub fn open(path: &Path) -> Result<Book> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?)?;

    let container = entry(&mut zip, "META-INF/container.xml")
        .ok_or_else(|| Error::NotAnEpub(path.to_owned(), "no META-INF/container.xml"))?;
    let opf_path = attribute(&container, "rootfile", "full-path")
        .ok_or_else(|| Error::NotAnEpub(path.to_owned(), "the container names no rootfile"))?;
    let opf = entry(&mut zip, &opf_path)
        .ok_or_else(|| Error::NotAnEpub(path.to_owned(), "the rootfile is missing"))?;

    // Every href in the package is relative to the package document, which is usually
    // in a subfolder. Resolving against the zip root instead is the classic way to
    // produce a book whose every chapter is missing.
    let base = folder(&opf_path);
    let package = parse_opf(&opf, &base);

    let mut titles = HashMap::new();
    if let Some(nav) = package.nav.as_ref().and_then(|h| entry(&mut zip, h)) {
        // The nav document is XHTML, so its hrefs resolve against *its* folder.
        let nav_base = folder(package.nav.as_deref().unwrap_or_default());
        titles.extend(parse_nav(&nav, &nav_base));
    }
    if titles.is_empty()
        && let Some(href) = package.ncx.as_ref()
        && let Some(ncx) = entry(&mut zip, href)
    {
        titles.extend(parse_ncx(&ncx, &folder(href)));
    }

    let chapters: Vec<Spine> = package
        .spine
        .iter()
        .enumerate()
        .map(|(n, href)| Spine {
            title: titles
                .get(href)
                .cloned()
                // No entry in the table of contents is normal -- a nav document lists
                // what the publisher chose to list. The first heading in the chapter is
                // the next best name, and it costs one entry read.
                .or_else(|| entry(&mut zip, href).and_then(|x| crate::from_html(&x).heading()))
                .unwrap_or_else(|| format!("Chapter {}", n + 1)),
            href: href.clone(),
        })
        .collect();

    if chapters.is_empty() {
        return Err(Error::Empty(path.to_owned()));
    }

    Ok(Book {
        title: if package.title.is_empty() {
            stem(path)
        } else {
            package.title
        },
        author: package.author,
        language: package.language,
        chapters,
        path: path.to_owned(),
    })
}

/// One chapter, normalized. This is the only call that reads prose.
pub fn chapter(path: &Path, href: &str) -> Result<Document> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?)?;
    let xhtml = entry(&mut zip, href).ok_or_else(|| Error::Empty(path.to_owned()))?;
    Ok(crate::from_html(&xhtml))
}

// ----------------------------------------------------------------------------- zip

/// One entry as text, or nothing.
///
/// Tolerant about the name: EPUBs in the wild percent-encode hrefs, and some writers
/// emit backslashes. Neither is worth failing a whole book over.
fn entry(zip: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<String> {
    let mut read = |name: &str| -> Option<String> {
        let mut file = zip.by_name(name).ok()?;
        let mut out = String::new();
        // Lossy, and on purpose: an EPUB declaring UTF-8 and containing one stray byte
        // is not a reason to refuse the chapter.
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        out.push_str(&String::from_utf8_lossy(&bytes));
        Some(out)
    };
    read(name)
        .or_else(|| read(&name.replace('\\', "/")))
        .or_else(|| read(&percent_decode(name)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ------------------------------------------------------------------------- package

#[derive(Default)]
struct Package {
    title: String,
    author: String,
    language: String,
    spine: Vec<String>,
    nav: Option<String>,
    ncx: Option<String>,
}

fn parse_opf(xml: &str, base: &str) -> Package {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().check_end_names = false;

    let mut package = Package::default();
    // id -> (href, properties), so the spine's idrefs can be resolved afterwards.
    let mut manifest: HashMap<String, (String, String)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut ncx_id = String::new();
    let mut field = String::new();
    let mut text = String::new();

    while let Ok(event) = reader.read_event() {
        match event {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let name = local(e.name().as_ref());
                match name.as_str() {
                    "item" => {
                        let id = attr(&e, "id").unwrap_or_default();
                        let href = attr(&e, "href").unwrap_or_default();
                        let props = attr(&e, "properties").unwrap_or_default();
                        manifest.insert(id, (join(base, &href), props));
                    }
                    "itemref" => {
                        // `linear="no"` is the publisher marking a page as out of the
                        // reading flow -- a cover image, an ad. Skipped, as intended.
                        if attr(&e, "linear").as_deref() != Some("no")
                            && let Some(idref) = attr(&e, "idref")
                        {
                            order.push(idref);
                        }
                    }
                    "spine" => ncx_id = attr(&e, "toc").unwrap_or_default(),
                    "title" | "creator" | "language" => {
                        field = name;
                        text.clear();
                    }
                    _ => {}
                }
            }
            Event::Text(t) => text.push_str(&t),
            Event::GeneralRef(r) => text.push_str(&pr_core::unescape(&format!("&{};", r.as_ref()))),
            Event::End(e) => {
                let name = local(e.name().as_ref());
                if name == field && !text.trim().is_empty() {
                    let value = text.trim().to_owned();
                    match name.as_str() {
                        // First wins: `dc:title` appears once, but a `<title>` inside
                        // some other element must not overwrite it.
                        "title" if package.title.is_empty() => package.title = value,
                        "creator" if package.author.is_empty() => package.author = value,
                        "language" if package.language.is_empty() => package.language = value,
                        _ => {}
                    }
                }
                text.clear();
                field.clear();
            }
            _ => {}
        }
    }

    package.spine = order
        .iter()
        .filter_map(|id| manifest.get(id).map(|(href, _)| href.clone()))
        .collect();
    package.nav = manifest
        .values()
        .find(|(_, props)| props.split_whitespace().any(|p| p == "nav"))
        .map(|(href, _)| href.clone());
    package.ncx = manifest.get(&ncx_id).map(|(href, _)| href.clone());
    package
}

/// EPUB 3: an XHTML `<nav epub:type="toc">` of nested lists.
///
/// Read as flat pairs of href and text. The nesting is the publisher's chapter
/// hierarchy and we present a flat list, so keeping it would be keeping something with
/// nowhere to go.
fn parse_nav(xml: &str, base: &str) -> HashMap<String, String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    reader.config_mut().allow_unmatched_ends = true;

    let mut out = HashMap::new();
    let mut href: Option<String> = None;
    let mut text = String::new();

    loop {
        match reader.read_event() {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) if local(e.name().as_ref()) == "a" => {
                href = attr(&e, "href").map(|h| join(base, strip_fragment(&h)));
                text.clear();
            }
            Ok(Event::Text(t)) if href.is_some() => text.push_str(&t),
            Ok(Event::GeneralRef(r)) if href.is_some() => {
                text.push_str(&pr_core::unescape(&format!("&{};", r.as_ref())))
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == "a" => {
                if let Some(href) = href.take()
                    && !text.trim().is_empty()
                {
                    out.entry(href).or_insert_with(|| squash(&text));
                }
            }
            _ => {}
        }
    }
    out
}

/// EPUB 2: `toc.ncx`, where the label and the target are siblings rather than nested.
fn parse_ncx(xml: &str, base: &str) -> HashMap<String, String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().check_end_names = false;

    let mut out = HashMap::new();
    let mut label = String::new();
    let mut text = String::new();
    let mut in_label = false;

    loop {
        match reader.read_event() {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => match local(e.name().as_ref()).as_str() {
                "navlabel" => {
                    in_label = true;
                    text.clear();
                }
                "content" => {
                    if let Some(src) = attr(&e, "src")
                        && !label.trim().is_empty()
                    {
                        out.entry(join(base, strip_fragment(&src)))
                            .or_insert_with(|| squash(&label));
                    }
                }
                _ => {}
            },
            // `<content src="..."/>` is normally self-closing.
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == "content" => {
                if let Some(src) = attr(&e, "src")
                    && !label.trim().is_empty()
                {
                    out.entry(join(base, strip_fragment(&src)))
                        .or_insert_with(|| squash(&label));
                }
            }
            Ok(Event::Text(t)) if in_label => text.push_str(&t),
            Ok(Event::GeneralRef(r)) if in_label => {
                text.push_str(&pr_core::unescape(&format!("&{};", r.as_ref())))
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == "navlabel" => {
                in_label = false;
                label = std::mem::take(&mut text);
            }
            _ => {}
        }
    }
    out
}

// ------------------------------------------------------------------------- helpers

fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_ascii_lowercase()
}

fn attr(e: &quick_xml::events::BytesStart<'_>, want: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find_map(|a| (local(a.key.as_ref()) == want).then(|| pr_core::unescape(&a.value)))
}

/// One attribute out of a document, without building a model of it. Used once, for the
/// container, where the whole file is four lines.
fn attribute(xml: &str, element: &str, want: &str) -> Option<String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    loop {
        match reader.read_event() {
            Err(_) | Ok(Event::Eof) => return None,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if local(e.name().as_ref()) == element => {
                if let Some(value) = attr(&e, want) {
                    return Some(value);
                }
            }
            _ => {}
        }
    }
}

fn strip_fragment(href: &str) -> &str {
    href.split('#').next().unwrap_or(href)
}

/// The folder part of a zip path, with its trailing slash.
fn folder(path: &str) -> String {
    match path.rfind('/') {
        Some(at) => path[..=at].to_owned(),
        None => String::new(),
    }
}

/// Resolve a relative href against a folder, collapsing `..` and `.`.
fn join(base: &str, href: &str) -> String {
    let href = strip_fragment(href);
    if href.starts_with('/') {
        return href.trim_start_matches('/').to_owned();
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in base.split('/').chain(href.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Whitespace in a title is layout, never content.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .to_owned()
}
