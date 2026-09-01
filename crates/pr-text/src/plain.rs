//! Plain text and Markdown.
//!
//! ponytail: Markdown here is headings, emphasis and horizontal rules -- the subset a
//! novel actually uses. Tables, links, code fences and footnotes are not parsed, and a
//! file that leans on them reads as its own source, which is at least honest. Reach for
//! a real Markdown crate when someone brings a file this loses.

use crate::{Block, Document, Kind, Span};

/// A blank line separates paragraphs; a run of blank lines is still one separator.
pub fn from_plain(text: &str, markdown: bool) -> Document {
    let mut blocks = Vec::new();
    let mut para: Vec<&str> = Vec::new();

    let flush = |para: &mut Vec<&str>, blocks: &mut Vec<Block>| {
        if para.is_empty() {
            return;
        }
        // Lines inside one paragraph are joined with a space, not a newline: a text
        // file hard-wrapped at 72 columns is prose, not verse, and reflowing it is the
        // whole point of a text reader.
        let joined = para.join(" ");
        para.clear();
        let block = if markdown {
            markdown_block(&joined)
        } else {
            Block {
                kind: Kind::Para,
                spans: vec![Span {
                    text: joined,
                    em: false,
                    strong: false,
                }],
            }
        };
        if !block.is_empty() {
            blocks.push(block);
        }
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush(&mut para, &mut blocks);
        } else if markdown && matches!(trimmed, "---" | "***" | "___" | "* * *") {
            flush(&mut para, &mut blocks);
            blocks.push(Block {
                kind: Kind::Divider,
                spans: Vec::new(),
            });
        } else if markdown && trimmed.starts_with('#') {
            // A heading is its own block whether or not a blank line follows it.
            flush(&mut para, &mut blocks);
            para.push(trimmed);
            flush(&mut para, &mut blocks);
        } else {
            para.push(trimmed);
        }
    }
    flush(&mut para, &mut blocks);

    Document { blocks }
}

fn markdown_block(line: &str) -> Block {
    if let Some(rest) = line.strip_prefix('>') {
        return Block {
            kind: Kind::Quote,
            spans: emphasis(rest.trim()),
        };
    }
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) {
        return Block {
            kind: Kind::Heading(hashes as u8),
            spans: emphasis(line[hashes..].trim()),
        };
    }
    Block {
        kind: Kind::Para,
        spans: emphasis(line),
    }
}

/// `**strong**` and `*em*`, and the underscore spellings.
///
/// Deliberately not a parser: it scans for a marker, finds its partner, and treats
/// anything unpaired as ordinary text. An asterisk used as a scene break or a footnote
/// marker therefore stays an asterisk.
fn emphasis(line: &str) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut rest = line;

    while !rest.is_empty() {
        let Some(at) = rest.find(['*', '_']) else {
            break;
        };
        let marker = &rest[at..at + 1];
        let double = rest[at..].starts_with(&marker.repeat(2));
        let open = if double {
            marker.repeat(2)
        } else {
            marker.into()
        };

        let after = at + open.len();
        let Some(end) = rest[after..].find(&open).map(|i| i + after) else {
            // Unpaired. Take the marker as text and carry on past it.
            push(&mut spans, &rest[..after], false, false);
            rest = &rest[after..];
            continue;
        };

        push(&mut spans, &rest[..at], false, false);
        push(&mut spans, &rest[after..end], !double, double);
        rest = &rest[end + open.len()..];
    }
    push(&mut spans, rest, false, false);
    spans
}

fn push(spans: &mut Vec<Span>, text: &str, em: bool, strong: bool) {
    if text.is_empty() {
        return;
    }
    match spans.last_mut() {
        Some(last) if last.em == em && last.strong == strong => last.text.push_str(text),
        _ => spans.push(Span {
            text: text.to_owned(),
            em,
            strong,
        }),
    }
}
