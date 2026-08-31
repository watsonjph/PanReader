//! OPDS catalog parsing, for Komga, Kavita, Calibre-Web and anything else that speaks
//! it.
//!
//! Parsing only. No network: the host fetches and hands the bytes here, which is the
//! same rule extensions live under (`CLAUDE.md`, Sources and plugins) and means the
//! fetch layer is written once rather than once per feature.
//!
//! Two wire formats for one idea. OPDS 1.2 is Atom XML and is what almost every server
//! actually serves; OPDS 2.0 is JSON and is what new ones serve. They differ in
//! spelling, not in meaning, so both parse into the same `Feed`.

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not valid xml: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("not valid json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not an OPDS feed")]
    NotAFeed,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Somewhere to download a publication from.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Download {
    pub href: String,
    pub mime: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum EntryKind {
    /// A sub-catalog. Following it yields another feed.
    Navigation { href: String },
    /// Something to fetch. More than one format is normal.
    Publication { downloads: Vec<Download> },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub thumbnail: Option<String>,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Feed {
    pub title: String,
    pub entries: Vec<Entry>,
    /// The next page, when the server paginates. Following it until it is `None` is the
    /// whole of OPDS pagination.
    pub next: Option<String>,
}

/// An acquisition relation, in any of the spellings servers use.
///
/// The bare `http://opds-spec.org/acquisition` and its `/open-access` variant are the
/// two that mean "you may have this". `/buy`, `/borrow` and `/subscribe` are
/// deliberately excluded: they lead to a purchase or a loan, which is a credential flow,
/// and `CLAUDE.md` invariant 13 keeps us out of those entirely.
fn is_acquisition(rel: &str) -> bool {
    matches!(
        rel,
        "http://opds-spec.org/acquisition" | "http://opds-spec.org/acquisition/open-access"
    )
}

fn is_thumbnail(rel: &str) -> bool {
    matches!(
        rel,
        "http://opds-spec.org/image/thumbnail" | "http://opds-spec.org/image"
    )
}

/// Whether a link's media type is a catalog rather than a file.
fn is_catalog(mime: &str) -> bool {
    mime.starts_with("application/atom+xml") || mime.starts_with("application/opds+json")
}

/// Parse a feed, sniffing which of the two formats it is.
///
/// Servers mislabel content types often enough that trusting the header is not worth
/// it; the first non-space byte settles it.
pub fn parse(bytes: &[u8]) -> Result<Feed> {
    let head = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map(|i| bytes[i]);
    match head {
        Some(b'{') => parse_json(bytes),
        Some(b'<') => parse_atom(bytes),
        _ => Err(Error::NotAFeed),
    }
}

// ---------------------------------------------------------------- OPDS 1.2, Atom XML

#[derive(Default)]
struct AtomLink {
    rel: String,
    href: String,
    mime: String,
}

fn read_link(e: &quick_xml::events::BytesStart) -> AtomLink {
    let mut link = AtomLink::default();
    for attr in e.attributes().flatten() {
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .unwrap_or_default()
            .into_owned();
        match local_name(attr.key.as_ref()).as_str() {
            "rel" => link.rel = value,
            "href" => link.href = value,
            "type" => link.mime = value,
            _ => {}
        }
    }
    link
}

/// Everything an entry accumulates while its element is open.
#[derive(Default)]
struct Building {
    id: String,
    title: String,
    author: Option<String>,
    summary: Option<String>,
    links: Vec<AtomLink>,
}

/// Parse an OPDS 1.2 Atom feed.
///
/// A real XML parser here, unlike `ComicInfo.xml`. Atom carries namespaces,
/// attribute-only links and nested author elements, and it arrives from servers we did
/// not write — so the shapes a substring scan cannot handle are exactly the ones that
/// turn up in the wild.
pub fn parse_atom(bytes: &[u8]) -> Result<Feed> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_reader(bytes);
    // Deliberately not trim_text: it trims each fragment, so `Komga &amp; Friends`
    // loses the spaces either side of the entity. The assembled value is trimmed once,
    // when the element closes.

    let mut feed = Feed::default();
    let mut buf = Vec::new();
    // Atom nests <author><name> inside <entry>, so the name of the innermost element is
    // not on its own enough to know what a run of text belongs to.
    let mut path: Vec<String> = Vec::new();
    let mut in_entry = false;
    let mut in_author = false;
    let mut saw_feed = false;
    let mut open = Building::default();
    // 0.42 emits an entity reference as its own event, so `Girl &amp; Boy` arrives as
    // three: text, ref, text. Assigning per event keeps only the last fragment, so
    // text accumulates and is read when the element closes.
    let mut text = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => return Err(Error::Xml(e)),
            Ok(Event::Eof) => break,

            // An Empty element carries attributes but has no End, so it must not go on
            // the path. `<link ... />` is the overwhelmingly common case of one.
            Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == "link" {
                    let link = read_link(&e);
                    if in_entry {
                        open.links.push(link);
                    } else if link.rel == "next" {
                        feed.next = Some(link.href);
                    }
                }
            }

            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "feed" => saw_feed = true,
                    "entry" => {
                        in_entry = true;
                        open = Building::default();
                    }
                    "author" => in_author = true,
                    "link" => {
                        let link = read_link(&e);
                        if in_entry {
                            open.links.push(link);
                        } else if link.rel == "next" {
                            feed.next = Some(link.href);
                        }
                    }
                    _ => {}
                }
                path.push(name);
                text.clear();
            }

            Ok(Event::Text(t)) => text.push_str(&t),

            // `&amp;` and `&#8212;` alike.
            Ok(Event::GeneralRef(r)) => match r.resolve_char_ref() {
                Ok(Some(c)) => text.push(c),
                // A named entity. The five predefined ones are all that Atom guarantees
                // without a DTD, and they are what servers actually emit.
                _ => text.push_str(match r.as_ref() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
                    other => other,
                }),
            },

            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                let depth = path.len();
                path.pop();
                let value = text.trim().to_owned();
                text.clear();

                if !value.is_empty() {
                    match name.as_str() {
                        "title" if in_entry => open.title = value,
                        // Depth 2 is a direct child of <feed>. An entry's title is
                        // deeper, and mistaking the two is the classic way to end up
                        // with the last book's name as the catalog name.
                        "title" if depth == 2 => feed.title = value,
                        "id" if in_entry => open.id = value,
                        "name" if in_author && in_entry => open.author = Some(value),
                        "summary" | "content" if in_entry => open.summary = Some(value),
                        _ => {}
                    }
                }

                match name.as_str() {
                    "author" => in_author = false,
                    "entry" => {
                        in_entry = false;
                        if let Some(entry) = build_entry(std::mem::take(&mut open)) {
                            feed.entries.push(entry);
                        }
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
        }
        buf.clear();
    }

    if !saw_feed {
        return Err(Error::NotAFeed);
    }
    Ok(feed)
}

/// `{http://www.w3.org/2005/Atom}title`, `atom:title` and `title` are one element.
/// Servers use all three spellings.
fn local_name(raw: &str) -> String {
    raw.rsplit(':').next().unwrap_or(raw).to_ascii_lowercase()
}

/// Decide what an entry is from the links it carries.
///
/// An entry with an acquisition link is something to read; failing that, one pointing
/// at another catalog is something to browse. An entry that is neither is dropped —
/// it leads nowhere we can follow, and showing it produces a row that does nothing.
fn build_entry(open: Building) -> Option<Entry> {
    let thumbnail = open
        .links
        .iter()
        .find(|l| is_thumbnail(&l.rel))
        .map(|l| l.href.clone());

    let downloads: Vec<Download> = open
        .links
        .iter()
        .filter(|l| is_acquisition(&l.rel))
        .map(|l| Download {
            href: l.href.clone(),
            mime: l.mime.clone(),
        })
        .collect();

    let kind = if downloads.is_empty() {
        let href = open
            .links
            .iter()
            .find(|l| is_catalog(&l.mime) || l.rel == "subsection")
            .map(|l| l.href.clone())?;
        EntryKind::Navigation { href }
    } else {
        EntryKind::Publication { downloads }
    };

    Some(Entry {
        id: open.id,
        title: open.title,
        author: open.author,
        summary: open.summary,
        thumbnail,
        kind,
    })
}

// ------------------------------------------------------------------ OPDS 2.0, JSON

#[derive(Deserialize)]
struct JsonFeed {
    #[serde(default)]
    metadata: JsonMeta,
    #[serde(default)]
    links: Vec<JsonLink>,
    #[serde(default)]
    navigation: Vec<JsonLink>,
    #[serde(default)]
    publications: Vec<JsonPublication>,
    /// Servers group shelves ("recently added", "on deck") rather than returning a flat
    /// list. The groups are presentation; the publications inside them are the feed.
    #[serde(default)]
    groups: Vec<JsonFeed>,
}

#[derive(Deserialize, Default)]
struct JsonMeta {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    author: Option<serde_json::Value>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    identifier: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
struct JsonLink {
    #[serde(default)]
    href: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "type")]
    mime: Option<String>,
    #[serde(default)]
    rel: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct JsonPublication {
    #[serde(default)]
    metadata: JsonMeta,
    #[serde(default)]
    links: Vec<JsonLink>,
    #[serde(default)]
    images: Vec<JsonLink>,
}

/// `rel` is a string in some feeds and an array in others. Both are legal.
fn rels(value: &Option<serde_json::Value>) -> Vec<String> {
    match value {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Likewise `author`: a bare string, an object with a name, or a list of either.
fn author_name(value: &Option<serde_json::Value>) -> Option<String> {
    match value.as_ref()? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(o) => o.get("name")?.as_str().map(str::to_owned),
        serde_json::Value::Array(a) => a.first().and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Object(o) => o.get("name")?.as_str().map(str::to_owned),
            _ => None,
        }),
        _ => None,
    }
}

/// Parse an OPDS 2.0 JSON feed.
pub fn parse_json(bytes: &[u8]) -> Result<Feed> {
    let raw: JsonFeed = serde_json::from_slice(bytes)?;
    let mut feed = Feed {
        title: raw.metadata.title.clone().unwrap_or_default(),
        entries: Vec::new(),
        next: raw
            .links
            .iter()
            .find(|l| rels(&l.rel).iter().any(|r| r == "next"))
            .map(|l| l.href.clone()),
    };
    collect_json(&raw, &mut feed.entries);
    Ok(feed)
}

fn collect_json(raw: &JsonFeed, out: &mut Vec<Entry>) {
    for link in &raw.navigation {
        out.push(Entry {
            id: link.href.clone(),
            title: link.title.clone().unwrap_or_else(|| link.href.clone()),
            author: None,
            summary: None,
            thumbnail: None,
            kind: EntryKind::Navigation {
                href: link.href.clone(),
            },
        });
    }

    for pub_ in &raw.publications {
        let downloads: Vec<Download> = pub_
            .links
            .iter()
            .filter(|l| rels(&l.rel).iter().any(|r| is_acquisition(r)))
            .map(|l| Download {
                href: l.href.clone(),
                mime: l.mime.clone().unwrap_or_default(),
            })
            .collect();
        if downloads.is_empty() {
            continue;
        }
        out.push(Entry {
            id: pub_
                .metadata
                .identifier
                .clone()
                .unwrap_or_else(|| downloads[0].href.clone()),
            title: pub_.metadata.title.clone().unwrap_or_default(),
            author: author_name(&pub_.metadata.author),
            summary: pub_.metadata.description.clone(),
            thumbnail: pub_.images.first().map(|i| i.href.clone()),
            kind: EntryKind::Publication { downloads },
        });
    }

    for group in &raw.groups {
        collect_json(group, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like what Komga and Calibre-Web serve: namespaced elements, a mix of
    /// navigation and acquisition entries, and a paginated feed.
    const ATOM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:opds="http://opds-spec.org/2010/catalog">
  <title>Komga &amp; Friends</title>
  <link rel="self" href="/opds/v1.2/catalog" type="application/atom+xml;profile=opds-catalog"/>
  <link rel="next" href="/opds/v1.2/series?page=2" type="application/atom+xml;profile=opds-catalog"/>
  <entry>
    <title>All Series</title>
    <id>urn:series</id>
    <link rel="subsection" href="/opds/v1.2/series"
          type="application/atom+xml;profile=opds-catalog;kind=navigation"/>
  </entry>
  <entry>
    <title>Yotsuba&amp;! Vol. 1</title>
    <id>urn:uuid:1234</id>
    <author><name>Kiyohiko Azuma</name></author>
    <summary>Danbo arrives.</summary>
    <link rel="http://opds-spec.org/image/thumbnail" href="/cover/1" type="image/jpeg"/>
    <link rel="http://opds-spec.org/acquisition" href="/download/1"
          type="application/vnd.comicbook+zip"/>
  </entry>
  <entry>
    <title>Paywalled</title>
    <id>urn:uuid:9999</id>
    <link rel="http://opds-spec.org/acquisition/buy" href="/buy/9" type="text/html"/>
  </entry>
</feed>"#;

    #[test]
    fn an_atom_catalog_separates_what_to_browse_from_what_to_read() {
        let feed = parse(ATOM.as_bytes()).unwrap();
        assert_eq!(feed.title, "Komga & Friends", "entities are resolved");
        assert_eq!(feed.next.as_deref(), Some("/opds/v1.2/series?page=2"));

        // The buy entry is dropped: it leads to a purchase, which invariant 13 keeps us
        // out of, and it carries no link we could follow instead.
        assert_eq!(feed.entries.len(), 2);

        assert_eq!(feed.entries[0].title, "All Series");
        assert_eq!(
            feed.entries[0].kind,
            EntryKind::Navigation {
                href: "/opds/v1.2/series".into()
            }
        );

        let book = &feed.entries[1];
        assert_eq!(book.title, "Yotsuba&! Vol. 1");
        assert_eq!(book.author.as_deref(), Some("Kiyohiko Azuma"));
        assert_eq!(book.summary.as_deref(), Some("Danbo arrives."));
        assert_eq!(book.thumbnail.as_deref(), Some("/cover/1"));
        assert_eq!(
            book.kind,
            EntryKind::Publication {
                downloads: vec![Download {
                    href: "/download/1".into(),
                    mime: "application/vnd.comicbook+zip".into(),
                }]
            }
        );
    }

    /// The feed title must not be taken from an entry's title, which is the classic way
    /// to get this wrong: both elements are called `title`.
    #[test]
    fn a_nested_title_does_not_become_the_feed_title() {
        let feed = parse_atom(ATOM.as_bytes()).unwrap();
        assert_eq!(feed.title, "Komga & Friends");
    }

    #[test]
    fn an_opds_2_feed_reads_the_same_as_an_atom_one() {
        let json = br#"{
          "metadata": { "title": "Kavita" },
          "links": [{ "rel": "next", "href": "/api/opds/2?page=2" }],
          "navigation": [
            { "title": "Libraries", "href": "/api/opds/libraries",
              "type": "application/opds+json" }
          ],
          "groups": [{
            "metadata": { "title": "On Deck" },
            "publications": [{
              "metadata": { "title": "Berserk", "author": { "name": "Kentaro Miura" },
                            "identifier": "urn:kavita:7", "description": "A long one." },
              "images": [{ "href": "/covers/7" }],
              "links": [
                { "rel": ["http://opds-spec.org/acquisition"], "href": "/dl/7",
                  "type": "application/vnd.comicbook+zip" }
              ]
            }]
          }]
        }"#;

        let feed = parse(json).unwrap();
        assert_eq!(feed.title, "Kavita");
        assert_eq!(feed.next.as_deref(), Some("/api/opds/2?page=2"));
        assert_eq!(feed.entries.len(), 2);
        assert_eq!(feed.entries[0].title, "Libraries");

        // Publications nested in a group are still publications. A parser that only
        // reads the top level finds an empty shelf on Kavita, which serves everything
        // in groups.
        let book = &feed.entries[1];
        assert_eq!(book.title, "Berserk");
        assert_eq!(book.author.as_deref(), Some("Kentaro Miura"));
        assert_eq!(book.thumbnail.as_deref(), Some("/covers/7"));
        assert!(matches!(book.kind, EntryKind::Publication { .. }));
    }

    #[test]
    fn rel_and_author_are_accepted_as_a_string_or_a_list() {
        let json = br#"{
          "metadata": { "title": "T" },
          "publications": [{
            "metadata": { "title": "B", "author": ["Solo Writer"] },
            "links": [{ "rel": "http://opds-spec.org/acquisition/open-access",
                        "href": "/dl/1", "type": "application/epub+zip" }]
          }]
        }"#;
        let feed = parse(json).unwrap();
        assert_eq!(feed.entries[0].author.as_deref(), Some("Solo Writer"));
    }

    #[test]
    fn something_that_is_not_a_feed_is_rejected_rather_than_returned_empty() {
        assert!(matches!(parse(b""), Err(Error::NotAFeed)));
        assert!(matches!(
            parse(b"<html><body>nope</body></html>"),
            Err(Error::NotAFeed)
        ));
        assert!(parse(b"{ not json").is_err());
    }
}
