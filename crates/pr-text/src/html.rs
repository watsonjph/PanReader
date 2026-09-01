//! XHTML in, normalized blocks out.
//!
//! Not a rendering path and not a sanitizer bolted onto one: markup never reaches the
//! screen, so there is nothing to strip dangerous attributes *from*. What comes out is
//! text, two emphasis flags and a block role. A `<script>` cannot survive that, and
//! neither can a stylesheet, an inline event handler or a tracking pixel -- not because
//! each is filtered, but because there is nowhere for any of them to go.

use crate::{Block, Document, Kind, Span};
use quick_xml::events::Event;

/// Elements whose entire contents are dropped.
///
/// `rt` and `rp` are the ruby annotation and its fallback parentheses: Japanese text
/// puts the reading above the base characters, and flattening them into the paragraph
/// would interleave the reading with the sentence. The base text stays.
const SILENT: &[&str] = &["head", "script", "style", "title", "rt", "rp"];

/// Elements that end the block they are in.
fn block_kind(name: &str) -> Option<Kind> {
    Some(match name {
        "p" | "div" | "section" | "article" | "li" | "dd" | "dt" | "td" | "figcaption" => {
            Kind::Para
        }
        "h1" => Kind::Heading(1),
        "h2" => Kind::Heading(2),
        "h3" => Kind::Heading(3),
        "h4" => Kind::Heading(4),
        "h5" => Kind::Heading(5),
        "h6" => Kind::Heading(6),
        _ => return None,
    })
}

/// The last path segment of a namespaced name: `xhtml:p` is `p`.
fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_ascii_lowercase()
}

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    spans: Vec<Span>,
    kind: Kind,
    quote: u32,
    em: u32,
    strong: u32,
    /// Whether the last character pushed was a space, so runs of whitespace across span
    /// and element boundaries collapse to one.
    spaced: bool,
}

impl Builder {
    fn push(&mut self, raw: &str) {
        for c in raw.chars() {
            // A no-break space is content, not layout: it is there precisely because
            // the publisher did not want it collapsed.
            if c.is_whitespace() && c != '\u{a0}' {
                // Nothing yet in this block means leading whitespace, which goes.
                if self.spaced || self.spans.iter().all(|s| s.text.is_empty()) {
                    continue;
                }
                self.spaced = true;
                self.append(' ');
                continue;
            }
            self.spaced = false;
            self.append(c);
        }
    }

    /// A hard line break inside a paragraph -- verse, an address, a letter. Kept as a
    /// newline in the text rather than as a block, because it is one paragraph.
    fn newline(&mut self) {
        if self.spans.iter().any(|s| !s.text.is_empty()) {
            self.append('\n');
            self.spaced = true;
        }
    }

    fn append(&mut self, c: char) {
        let em = self.em > 0;
        let strong = self.strong > 0;
        match self.spans.last_mut() {
            Some(span) if span.em == em && span.strong == strong => span.text.push(c),
            _ => self.spans.push(Span {
                text: c.to_string(),
                em,
                strong,
            }),
        }
    }

    fn flush(&mut self) {
        let mut spans = std::mem::take(&mut self.spans);
        let kind = std::mem::take(&mut self.kind);
        self.spaced = false;
        // Leading whitespace never got in; trailing whitespace only becomes trailing
        // once the block ends, so it is trimmed here. A character offset counted
        // against a block that ends in a stray space is off by one for no reason.
        while let Some(last) = spans.last_mut() {
            let trimmed = last.text.trim_end_matches([' ', '\n']);
            if trimmed.len() < last.text.len() {
                last.text.truncate(trimmed.len());
            }
            if !last.text.is_empty() {
                break;
            }
            spans.pop();
        }
        // A quote's paragraphs are still quoted: `<blockquote><p>` opens a Para inside
        // it, and the reader should see the indent rather than the inner tag.
        let kind = if self.quote > 0 && kind == Kind::Para {
            Kind::Quote
        } else {
            kind
        };
        let block = Block { kind, spans };
        if !block.is_empty() {
            self.blocks.push(block);
        }
    }
}

/// Parse one chapter of XHTML.
///
/// Lenient by design. EPUBs claim to be XHTML and a good number are not: unclosed
/// `<br>`, mismatched tags, an entity from the XHTML DTD that no XML parser declares.
/// A reader that refuses those is a reader that cannot open half of what people own, so
/// the parser is configured to carry on and the result is whatever text was recoverable.
pub fn from_html(xhtml: &str) -> Document {
    let mut reader = quick_xml::Reader::from_str(xhtml);
    let config = reader.config_mut();
    config.check_end_names = false;
    config.allow_unmatched_ends = true;

    let mut b = Builder::default();
    let mut silent = 0u32;

    loop {
        match reader.read_event() {
            Err(e) => {
                // Whatever was readable is better than nothing, and "nothing" is what a
                // reader gets from every other novel app when a file is slightly wrong.
                tracing::debug!("stopped parsing a chapter early: {e}");
                break;
            }
            Ok(Event::Eof) => break,

            Ok(Event::Start(e)) => {
                let name = local(e.name().as_ref());
                if SILENT.contains(&name.as_str()) {
                    silent += 1;
                    continue;
                }
                if silent > 0 {
                    continue;
                }
                open(&mut b, &name);
            }

            // Self-closing, so there is no End to match. `<br/>` and `<hr/>` are the
            // two that matter, and both are the reason this arm exists at all.
            Ok(Event::Empty(e)) => {
                if silent > 0 {
                    continue;
                }
                match local(e.name().as_ref()).as_str() {
                    "br" => b.newline(),
                    "hr" => {
                        b.flush();
                        b.blocks.push(Block {
                            kind: Kind::Divider,
                            spans: Vec::new(),
                        });
                    }
                    _ => {}
                }
            }

            Ok(Event::End(e)) => {
                let name = local(e.name().as_ref());
                if SILENT.contains(&name.as_str()) {
                    silent = silent.saturating_sub(1);
                    continue;
                }
                if silent > 0 {
                    continue;
                }
                close(&mut b, &name);
            }

            Ok(Event::Text(t)) if silent == 0 => b.push(&t),

            // 0.42 emits an entity reference as its own event, so `he said &mdash; no`
            // arrives as three. An unknown one is left as written rather than eaten.
            Ok(Event::GeneralRef(r)) if silent == 0 => match r.resolve_char_ref() {
                Ok(Some(c)) => b.push(&c.to_string()),
                _ => match pr_core::entity(r.as_ref()) {
                    Some(c) => b.push(&c.to_string()),
                    None => b.push(&format!("&{};", r.as_ref())),
                },
            },

            _ => {}
        }
    }

    b.flush();
    Document { blocks: b.blocks }
}

fn open(b: &mut Builder, name: &str) {
    if let Some(kind) = block_kind(name) {
        b.flush();
        b.kind = kind;
        return;
    }
    match name {
        "blockquote" => {
            b.flush();
            b.quote += 1;
        }
        "br" => b.newline(),
        "hr" => {
            b.flush();
            b.blocks.push(Block {
                kind: Kind::Divider,
                spans: Vec::new(),
            });
        }
        "em" | "i" | "cite" | "dfn" | "var" => b.em += 1,
        "strong" | "b" => b.strong += 1,
        _ => {}
    }
}

fn close(b: &mut Builder, name: &str) {
    if block_kind(name).is_some() {
        b.flush();
        return;
    }
    match name {
        "blockquote" => {
            b.flush();
            b.quote = b.quote.saturating_sub(1);
        }
        "em" | "i" | "cite" | "dfn" | "var" => b.em = b.em.saturating_sub(1),
        "strong" | "b" => b.strong = b.strong.saturating_sub(1),
        _ => {}
    }
}
