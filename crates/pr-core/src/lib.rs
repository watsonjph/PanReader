//! Domain types shared by every layer. Depends on nothing in the workspace.

use serde::{Deserialize, Serialize};

/// How a chapter is read.
///
/// Mihon splits this six ways (`LEFT_TO_RIGHT`, `RIGHT_TO_LEFT`, `VERTICAL`, `WEBTOON`,
/// `CONTINUOUS_VERTICAL`, `DEFAULT`). Two of those are refinements most people never
/// touch -- `VERTICAL` is paged but advances downward, and `CONTINUOUS_VERTICAL` is
/// `WEBTOON` with gaps between pages -- and the distinction between the last two is a
/// known source of confusion. Start with the three that map onto how comics are
/// actually published; add the refinements when someone wants them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadingMode {
    /// Paged, advancing right to left. Japanese manga.
    Rtl,
    /// Paged, advancing left to right. Western comics, Chinese manhua.
    Ltr,
    /// One continuous vertical strip with no seams. Korean manhwa, webtoons.
    Webtoon,
}

/// How a page is scaled into the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    #[default]
    Page,
    Width,
    Height,
    Original,
}

/// Everything the reader can change that should outlive the process.
///
/// Persisted as one JSON blob (see `pr-db`), so **every field carries a default**.
/// Adding a field must never stop an older config from loading, and removing one must
/// never stop a newer config from loading either.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Used when nothing better is known: no override, no metadata, no strip shape.
    pub default_reading_mode: ReadingMode,
    pub fit: Fit,
    /// Fraction of the natural display width to decode. Below 1 this also takes pages
    /// out of the passthrough path, so it costs CPU to save memory.
    pub downsample: f32,
    /// Gap between pages in CSS px. Zero is a seamless webtoon.
    pub page_padding: u32,
    pub rotation: u32,
    /// Suppresses the automatic quarter turn of a wide page.
    pub rotation_lock: bool,
    pub double_page: bool,
    /// Hold the cover back so later pairs land on the right leaf.
    pub cover_alone: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // Right to left is the safest default for a manga reader: the reader who
            // is wrong about it notices immediately, whereas a silently reversed
            // Japanese volume reads as nonsense for a while first.
            default_reading_mode: ReadingMode::Rtl,
            fit: Fit::Page,
            downsample: 1.0,
            page_padding: 0,
            rotation: 0,
            rotation_lock: false,
            double_page: false,
            cover_alone: true,
        }
    }
}

/// The `Manga` field of a ComicInfo.xml, which is the only widely-written metadata that
/// states reading direction. Komga, Kavita and most tagging tools emit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MangaFlag {
    Unknown,
    No,
    Yes,
    YesAndRightToLeft,
}

/// Where a chapter's reading mode came from, in precedence order. Surfaced in the UI so
/// a mode the reader did not expect can be explained rather than just suffered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeSource {
    /// The reader set it for this series. Always wins.
    SeriesOverride,
    /// ComicInfo.xml stated the direction.
    Metadata,
    /// Inferred from the shape of the pages.
    PageShape,
    /// Nothing to go on, so the reader's default applies.
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detected {
    pub mode: ReadingMode,
    pub source: ModeSource,
}

/// Below this width-to-height ratio a page is a strip segment, not a comic page.
///
/// Real numbers either side: a Yotsuba page is 978x1400, ratio 0.70; a webtoon segment
/// is commonly 800x8000, ratio 0.10. Print pages cluster tightly around 0.65-0.75
/// because paper does, so the gap is wide and 0.5 sits comfortably in it.
const WEBTOON_ASPECT: f32 = 0.5;

/// Median width-to-height ratio, which ignores a wide cover or a spread among portrait
/// pages the way a mean would not.
fn median_aspect(pages: &[(u32, u32)]) -> Option<f32> {
    let mut ratios: Vec<f32> = pages
        .iter()
        .filter(|&&(w, h)| w > 0 && h > 0)
        .map(|&(w, h)| w as f32 / h as f32)
        .collect();
    if ratios.is_empty() {
        return None;
    }
    ratios.sort_by(|a, b| {
        a.partial_cmp(b)
            .expect("ratios of positive ints are finite")
    });
    Some(ratios[ratios.len() / 2])
}

/// Decide how to read a chapter.
///
/// Precedence is override, then metadata, then page shape, then the reader's default.
///
/// What this can and cannot know is worth being plain about. Page shape identifies a
/// vertical strip reliably, because nothing printed on paper is eight times taller than
/// it is wide. **Direction is not recoverable from the images** -- no property of a JPEG
/// says "read me right to left" -- so it comes from ComicInfo.xml when present and from
/// the reader's default otherwise. Guessing it from filenames or resolution would
/// silently reverse someone's library, which is worse than asking once.
///
/// Phase 4's OCR closes this gap properly: detected script implies direction.
pub fn detect(
    pages: &[(u32, u32)],
    meta: Option<MangaFlag>,
    series_override: Option<ReadingMode>,
    default: ReadingMode,
) -> Detected {
    if let Some(mode) = series_override {
        return Detected {
            mode,
            source: ModeSource::SeriesOverride,
        };
    }

    // Layout beats metadata: if the pages are a strip, direction is meaningless, and a
    // file tagged as right-to-left manga can still be published as a vertical strip.
    if median_aspect(pages).is_some_and(|r| r < WEBTOON_ASPECT) {
        return Detected {
            mode: ReadingMode::Webtoon,
            source: ModeSource::PageShape,
        };
    }

    match meta {
        Some(MangaFlag::YesAndRightToLeft) => Detected {
            mode: ReadingMode::Rtl,
            source: ModeSource::Metadata,
        },
        // Explicitly not manga, so left to right is a safe read.
        Some(MangaFlag::No) => Detected {
            mode: ReadingMode::Ltr,
            source: ModeSource::Metadata,
        },
        // `Yes` says it is manga but deliberately does not say which direction, and
        // plenty of Chinese and Korean titles are tagged that way while reading left to
        // right. Treat it as no answer rather than assuming Japanese.
        _ => Detected {
            mode: default,
            source: ModeSource::Default,
        },
    }
}

/// What a ComicInfo.xml says about a chapter.
///
/// The ComicRack schema as Mylar, Komga, Kavita and every CBZ tagger write it. Absent
/// fields are `None`; a present but empty field is also `None`, because taggers emit
/// `<Series />` for "not set" and an empty title is worse than no title.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ComicInfo {
    pub series: Option<String>,
    pub title: Option<String>,
    pub number: Option<f64>,
    pub volume: Option<i64>,
    /// Chapters in the series, when the tagger knew.
    pub count: Option<i64>,
    pub summary: Option<String>,
    pub writer: Option<String>,
    pub penciller: Option<String>,
    pub publisher: Option<String>,
    pub genre: Option<String>,
    pub tags: Option<String>,
    pub characters: Option<String>,
    pub age_rating: Option<String>,
    pub web: Option<String>,
    /// ISO code. The source language for translation, eventually.
    pub language: Option<String>,
    pub year: Option<i64>,
    pub month: Option<i64>,
    pub day: Option<i64>,
    /// What the tagger claimed. We count the real pages and trust those instead; this
    /// is here because a mismatch is a useful signal that a file was modified.
    pub page_count: Option<i64>,
    pub manga: Option<MangaFlag>,
}

/// The five predefined entities and numeric references.
///
/// Taggers write `&amp;` in series titles constantly, and a title reading
/// `Fullmetal Alchemist &amp; Co` is a visible bug on the shelf.
fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail.find(';').filter(|i| *i <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let body = &tail[1..semi];
        let resolved = match body {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => body
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        match resolved {
            Some(c) => out.push(c),
            // Not an entity we know. Leave it exactly as written rather than eating it.
            None => out.push_str(&tail[..=semi]),
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// The text of one top-level element.
///
/// ponytail: a substring scan, not an XML parser. ComicInfo is flat, has no namespaces
/// and is machine-written, so the shapes a real parser buys — nesting, attributes on
/// these elements, mixed content — do not occur in it. Reach for `quick-xml` if we ever
/// parse XML we did not expect the shape of; do not add it for this.
fn field<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let rest = &xml[start..];
    Some(&rest[..rest.find(&format!("</{tag}>"))?])
}

fn text(xml: &str, tag: &str) -> Option<String> {
    let value = unescape(field(xml, tag)?.trim());
    (!value.is_empty()).then_some(value)
}

fn int(xml: &str, tag: &str) -> Option<i64> {
    field(xml, tag)?.trim().parse().ok()
}

/// Parse a ComicInfo.xml.
///
/// Never fails: a malformed or truncated document yields whatever fields survived. A
/// chapter with unreadable metadata still reads, it just falls back to its filename.
pub fn parse_comic_info(xml: &str) -> ComicInfo {
    ComicInfo {
        series: text(xml, "Series"),
        title: text(xml, "Title"),
        // Real, because 10.5 happens, and ComicInfo writes it as a string.
        number: field(xml, "Number").and_then(|n| n.trim().parse().ok()),
        volume: int(xml, "Volume"),
        count: int(xml, "Count"),
        summary: text(xml, "Summary"),
        writer: text(xml, "Writer"),
        penciller: text(xml, "Penciller"),
        publisher: text(xml, "Publisher"),
        genre: text(xml, "Genre"),
        tags: text(xml, "Tags"),
        characters: text(xml, "Characters"),
        age_rating: text(xml, "AgeRating"),
        web: text(xml, "Web"),
        language: text(xml, "LanguageISO"),
        year: int(xml, "Year"),
        month: int(xml, "Month"),
        day: int(xml, "Day"),
        page_count: int(xml, "PageCount"),
        manga: field(xml, "Manga").map(|v| match v.trim() {
            v if v.eq_ignore_ascii_case("YesAndRightToLeft") => MangaFlag::YesAndRightToLeft,
            v if v.eq_ignore_ascii_case("Yes") => MangaFlag::Yes,
            v if v.eq_ignore_ascii_case("No") => MangaFlag::No,
            _ => MangaFlag::Unknown,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real dimensions: Yotsuba&! vol 1 against the Phase 0 strip fixture.
    const MANGA: [(u32, u32); 4] = [(978, 1400), (978, 1400), (1456, 1400), (978, 1400)];
    const STRIP: [(u32, u32); 3] = [(800, 8000), (800, 8000), (800, 6400)];

    #[test]
    fn a_vertical_strip_is_recognised_by_its_shape() {
        let got = detect(&STRIP, None, None, ReadingMode::Rtl);
        assert_eq!(got.mode, ReadingMode::Webtoon);
        assert_eq!(got.source, ModeSource::PageShape);

        // Even tagged as right-to-left manga, a strip is still a strip.
        let tagged = detect(
            &STRIP,
            Some(MangaFlag::YesAndRightToLeft),
            None,
            ReadingMode::Ltr,
        );
        assert_eq!(tagged.mode, ReadingMode::Webtoon);
    }

    #[test]
    fn a_wide_spread_does_not_drag_a_paged_chapter_off_course() {
        // The median ignores the 1456px spread sitting among portrait pages.
        let got = detect(&MANGA, None, None, ReadingMode::Rtl);
        assert_eq!(got.mode, ReadingMode::Rtl);
        assert_eq!(
            got.source,
            ModeSource::Default,
            "no metadata means no evidence"
        );
    }

    #[test]
    fn metadata_decides_direction_and_only_when_it_actually_says() {
        let rtl = detect(
            &MANGA,
            Some(MangaFlag::YesAndRightToLeft),
            None,
            ReadingMode::Ltr,
        );
        assert_eq!(
            (rtl.mode, rtl.source),
            (ReadingMode::Rtl, ModeSource::Metadata)
        );

        let western = detect(&MANGA, Some(MangaFlag::No), None, ReadingMode::Rtl);
        assert_eq!(
            (western.mode, western.source),
            (ReadingMode::Ltr, ModeSource::Metadata)
        );

        // `Yes` is not a direction. Korean and Chinese titles carry it too.
        let bare = detect(&MANGA, Some(MangaFlag::Yes), None, ReadingMode::Ltr);
        assert_eq!(
            (bare.mode, bare.source),
            (ReadingMode::Ltr, ModeSource::Default)
        );
    }

    #[test]
    fn an_override_beats_every_other_signal() {
        let got = detect(
            &STRIP,
            Some(MangaFlag::YesAndRightToLeft),
            Some(ReadingMode::Ltr),
            ReadingMode::Rtl,
        );
        assert_eq!(
            (got.mode, got.source),
            (ReadingMode::Ltr, ModeSource::SeriesOverride)
        );
    }

    #[test]
    fn an_empty_chapter_falls_back_instead_of_dividing_by_zero() {
        assert_eq!(
            detect(&[], None, None, ReadingMode::Rtl).mode,
            ReadingMode::Rtl
        );
        assert_eq!(
            detect(&[(0, 0)], None, None, ReadingMode::Ltr).mode,
            ReadingMode::Ltr
        );
    }

    #[test]
    fn comicinfo_manga_field_is_read_in_the_shapes_tools_actually_write() {
        let xml = r#"<?xml version="1.0"?><ComicInfo><Series>X</Series><Manga>YesAndRightToLeft</Manga></ComicInfo>"#;
        assert_eq!(
            parse_comic_info(xml).manga,
            Some(MangaFlag::YesAndRightToLeft)
        );

        assert_eq!(
            parse_comic_info("<ComicInfo>\n  <Manga> Yes </Manga>\n</ComicInfo>").manga,
            Some(MangaFlag::Yes)
        );
        assert_eq!(
            parse_comic_info("<ComicInfo><Manga>No</Manga></ComicInfo>").manga,
            Some(MangaFlag::No)
        );
        assert_eq!(
            parse_comic_info("<ComicInfo><Manga>nonsense</Manga></ComicInfo>").manga,
            Some(MangaFlag::Unknown)
        );
        assert_eq!(
            parse_comic_info("<ComicInfo><Series>X</Series></ComicInfo>").manga,
            None
        );
        assert_eq!(parse_comic_info("<ComicInfo><Manga>truncated").manga, None);
    }

    #[test]
    fn a_full_comicinfo_is_read_and_entities_are_resolved() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<ComicInfo>
  <Series>Fullmetal Alchemist &amp; Co</Series>
  <Title>The Two Alchemists</Title>
  <Number>10.5</Number>
  <Volume>2</Volume>
  <Count>27</Count>
  <Summary>Ed &amp; Al say &quot;hello&quot;</Summary>
  <Writer>Hiromu Arakawa</Writer>
  <Publisher>Square Enix</Publisher>
  <LanguageISO>ja</LanguageISO>
  <Year>2002</Year>
  <PageCount>186</PageCount>
  <Manga>YesAndRightToLeft</Manga>
  <Genre />
</ComicInfo>"#;
        let info = parse_comic_info(xml);
        assert_eq!(info.series.as_deref(), Some("Fullmetal Alchemist & Co"));
        assert_eq!(info.title.as_deref(), Some("The Two Alchemists"));
        assert_eq!(info.number, Some(10.5), "10.5 happens");
        assert_eq!(info.volume, Some(2));
        assert_eq!(info.count, Some(27));
        assert_eq!(info.summary.as_deref(), Some(r#"Ed & Al say "hello""#));
        assert_eq!(info.language.as_deref(), Some("ja"));
        assert_eq!(info.year, Some(2002));
        assert_eq!(info.page_count, Some(186));
        assert_eq!(info.manga, Some(MangaFlag::YesAndRightToLeft));
        assert_eq!(info.genre, None, "a self-closed field is not a value");
        assert_eq!(info.characters, None);
    }

    #[test]
    fn numeric_entities_and_lone_ampersands_survive() {
        let info =
            parse_comic_info("<ComicInfo><Series>A&#38;B&#x2764;C &amp; D</Series></ComicInfo>");
        assert_eq!(info.series.as_deref(), Some("A&B\u{2764}C & D"));

        // A bare & is not an entity. Taggers emit them and the title must not lose text.
        let bare = parse_comic_info("<ComicInfo><Series>Q &  A</Series></ComicInfo>");
        assert_eq!(bare.series.as_deref(), Some("Q &  A"));

        // Something entity-shaped that we do not know stays verbatim.
        let unknown = parse_comic_info("<ComicInfo><Series>x&nbsp;y</Series></ComicInfo>");
        assert_eq!(unknown.series.as_deref(), Some("x&nbsp;y"));
    }

    #[test]
    fn a_document_that_is_not_comicinfo_yields_nothing_rather_than_failing() {
        assert_eq!(
            parse_comic_info("<html><body>404 not found</body></html>"),
            ComicInfo::default()
        );
    }
}
