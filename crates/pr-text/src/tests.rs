use crate::{Kind, from_html, from_plain};

/// The text of every block, in order, which is what a reader actually sees.
fn lines(doc: &crate::Document) -> Vec<String> {
    doc.blocks.iter().map(crate::Block::text).collect()
}

#[test]
fn markup_becomes_blocks_and_nothing_else_survives() {
    let doc = from_html(
        r#"<html><head><title>Ignored</title>
             <style>p { color: red }</style></head>
           <body onload="steal()">
             <h2>Chapter One</h2>
             <p>She said <em>no</em>, and <strong>meant</strong> it.</p>
             <script>fetch("http://example.invalid")</script>
             <p>   Whitespace
                   across   lines collapses.   </p>
           </body></html>"#,
    );

    assert_eq!(
        lines(&doc),
        [
            "Chapter One",
            "She said no, and meant it.",
            "Whitespace across lines collapses.",
        ]
    );
    assert_eq!(doc.blocks[0].kind, Kind::Heading(2));
    // The script and the stylesheet are not filtered out; there is nowhere for them to
    // go. The same is true of the onload attribute.
    assert!(!doc.blocks.iter().any(|b| b.text().contains("steal")));
    assert!(!doc.blocks.iter().any(|b| b.text().contains("example")));
}

#[test]
fn emphasis_nests_and_splits_spans_without_moving_the_text() {
    let doc = from_html("<p>a <strong>b <em>c</em> d</strong> e</p>");
    let block = &doc.blocks[0];
    assert_eq!(
        block.text(),
        "a b c d e",
        "the offset of 'e' is 8 either way"
    );

    let marked: Vec<(&str, bool, bool)> = block
        .spans
        .iter()
        .map(|s| (s.text.as_str(), s.em, s.strong))
        .collect();
    assert_eq!(
        marked,
        [
            ("a ", false, false),
            ("b ", false, true),
            ("c", true, true),
            (" d", false, true),
            (" e", false, false),
        ]
    );
}

#[test]
fn a_blockquote_keeps_its_kind_through_the_paragraphs_inside_it() {
    let doc = from_html("<blockquote><p>first</p><p>second</p></blockquote><p>after</p>");
    let kinds: Vec<Kind> = doc.blocks.iter().map(|b| b.kind).collect();
    assert_eq!(kinds, [Kind::Quote, Kind::Quote, Kind::Para]);
}

#[test]
fn a_line_break_stays_inside_its_paragraph_and_a_rule_becomes_a_divider() {
    let doc = from_html("<p>Dear sir,<br/>I write to you<br />in haste.</p><hr/><p>Later.</p>");
    assert_eq!(doc.blocks[0].text(), "Dear sir,\nI write to you\nin haste.");
    assert_eq!(doc.blocks[1].kind, Kind::Divider);
    assert_eq!(doc.blocks[2].text(), "Later.");
}

/// Japanese ruby: the reading sits above the base text, and flattening it into the
/// paragraph would interleave a pronunciation guide with the sentence.
#[test]
fn ruby_annotations_are_dropped_and_the_base_text_is_kept() {
    let doc = from_html("<p><ruby>漢字<rp>(</rp><rt>かんじ</rt><rp>)</rp></ruby>を読む</p>");
    assert_eq!(doc.blocks[0].text(), "漢字を読む");
}

#[test]
fn html_entities_resolve_including_the_ones_xml_does_not_declare() {
    let doc = from_html("<p>He paused&mdash;then&nbsp;left. Tom &amp; Jerry &#8230; &bogus;</p>");
    assert_eq!(
        doc.blocks[0].text(),
        "He paused\u{2014}then\u{a0}left. Tom & Jerry \u{2026} &bogus;"
    );
}

/// The reader must open what people own, not what the specification describes.
#[test]
fn a_malformed_chapter_yields_the_text_that_was_readable() {
    let doc = from_html("<body><p>first<p>second<p>third</body>");
    assert_eq!(lines(&doc), ["first", "second", "third"]);
}

#[test]
fn a_hard_wrapped_text_file_reflows_into_paragraphs() {
    let doc = from_plain(
        "The quick brown fox\njumped over the lazy dog.\n\n\nA second paragraph.\n",
        false,
    );
    assert_eq!(
        lines(&doc),
        [
            "The quick brown fox jumped over the lazy dog.",
            "A second paragraph.",
        ]
    );
}

#[test]
fn markdown_gets_headings_dividers_and_paired_emphasis_only() {
    let doc = from_plain(
        "# Title\nSome *emphasis* and **weight**.\n\n---\n\nAn * unpaired marker.\n",
        true,
    );
    assert_eq!(doc.blocks[0].kind, Kind::Heading(1));
    assert_eq!(doc.blocks[0].text(), "Title");
    assert_eq!(doc.blocks[1].text(), "Some emphasis and weight.");
    assert!(doc.blocks[1].spans.iter().any(|s| s.em && !s.strong));
    assert!(doc.blocks[1].spans.iter().any(|s| s.strong && !s.em));
    assert_eq!(doc.blocks[2].kind, Kind::Divider);
    assert_eq!(doc.blocks[3].text(), "An * unpaired marker.");
}

// ------------------------------------------------------------------------------ epub

/// A minimal but real EPUB: container, package in a subfolder, an NCX, two spine items
/// and one marked `linear="no"`.
fn build_epub(path: &std::path::Path, nav3: bool) {
    use std::io::Write;
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();

    let mut put = |name: &str, body: &str| {
        zip.start_file(name, opts).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    };

    put(
        "META-INF/container.xml",
        r#"<container><rootfiles>
             <rootfile full-path="OEBPS/book.opf" media-type="application/oebps-package+xml"/>
           </rootfiles></container>"#,
    );

    let toc_item = if nav3 {
        r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>"#
    } else {
        r#"<item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>"#
    };
    put(
        "OEBPS/book.opf",
        &format!(
            r#"<package><metadata>
                 <dc:title>Bocchi &amp; the Rock</dc:title>
                 <dc:creator>Hamaji Aki</dc:creator>
                 <dc:language>ja</dc:language>
               </metadata>
               <manifest>
                 {toc_item}
                 <item id="c1" href="text/one.xhtml" media-type="application/xhtml+xml"/>
                 <item id="c2" href="text/two.xhtml" media-type="application/xhtml+xml"/>
                 <item id="cover" href="text/cover.xhtml" media-type="application/xhtml+xml"/>
               </manifest>
               <spine toc="ncx">
                 <itemref idref="cover" linear="no"/>
                 <itemref idref="c1"/>
                 <itemref idref="c2"/>
               </spine></package>"#
        ),
    );

    if nav3 {
        put(
            "OEBPS/nav.xhtml",
            r#"<html><body><nav epub:type="toc"><ol>
                 <li><a href="text/one.xhtml">The First Day</a></li>
                 <li><a href="text/two.xhtml#top">The Second   Day</a></li>
               </ol></nav></body></html>"#,
        );
    } else {
        put(
            "OEBPS/toc.ncx",
            r#"<ncx><navMap>
                 <navPoint><navLabel><text>The First Day</text></navLabel>
                   <content src="text/one.xhtml"/></navPoint>
                 <navPoint><navLabel><text>The Second   Day</text></navLabel>
                   <content src="text/two.xhtml#top"/></navPoint>
               </navMap></ncx>"#,
        );
    }

    put(
        "OEBPS/text/cover.xhtml",
        "<html><body><p>cover</p></body></html>",
    );
    put(
        "OEBPS/text/one.xhtml",
        "<html><body><h1>The First Day</h1><p>It began badly.</p></body></html>",
    );
    put(
        "OEBPS/text/two.xhtml",
        "<html><body><p>And it got worse.</p></body></html>",
    );
    zip.finish().unwrap();
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("pr_text_tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn an_epub3_opens_with_its_metadata_spine_order_and_nav_titles() {
    let path = scratch("nav3.epub");
    build_epub(&path, true);

    let book = crate::epub::open(&path).unwrap();
    assert_eq!(book.title, "Bocchi & the Rock", "the entity resolved");
    assert_eq!(book.author, "Hamaji Aki");
    assert_eq!(book.language, "ja");

    // The cover is `linear="no"`, so it is out of the reading flow.
    assert_eq!(book.chapters.len(), 2, "linear=no is skipped");
    // Hrefs resolve against the package document's folder, not the zip root.
    assert_eq!(book.chapters[0].href, "OEBPS/text/one.xhtml");
    assert_eq!(book.chapters[0].title, "The First Day");
    // A fragment is not part of the entry name, and title whitespace is squashed.
    assert_eq!(book.chapters[1].title, "The Second Day");

    let doc = crate::epub::chapter(&path, &book.chapters[1].href).unwrap();
    assert_eq!(doc.blocks[0].text(), "And it got worse.");
}

#[test]
fn an_epub2_falls_back_to_the_ncx_for_the_same_titles() {
    let path = scratch("ncx2.epub");
    build_epub(&path, false);

    let book = crate::epub::open(&path).unwrap();
    let titles: Vec<&str> = book.chapters.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["The First Day", "The Second Day"]);
}

#[test]
fn a_chapter_with_no_toc_entry_is_named_by_its_first_heading() {
    let path = scratch("notoc.epub");
    {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        let mut put = |name: &str, body: &str| {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        };
        put(
            "META-INF/container.xml",
            r#"<container><rootfiles><rootfile full-path="book.opf"/></rootfiles></container>"#,
        );
        put(
            "book.opf",
            r#"<package><manifest>
                 <item id="a" href="a.xhtml"/><item id="b" href="b.xhtml"/>
               </manifest>
               <spine><itemref idref="a"/><itemref idref="b"/></spine></package>"#,
        );
        put(
            "a.xhtml",
            "<body><h2>A Quiet Morning</h2><p>text</p></body>",
        );
        put("b.xhtml", "<body><p>no heading here</p></body>");
        zip.finish().unwrap();
    }

    let book = crate::epub::open(&path).unwrap();
    assert_eq!(book.chapters[0].title, "A Quiet Morning");
    assert_eq!(book.chapters[1].title, "Chapter 2", "and then its position");
    // No dc:title either, so the filename stands in.
    assert_eq!(book.title, "notoc");
}

#[test]
fn something_that_is_not_an_epub_says_so_by_name() {
    let path = scratch("plain.zip");
    {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        zip.start_file("hello.txt", zip::write::FileOptions::<'_, ()>::default())
            .unwrap();
        zip.write_all(b"not a book").unwrap();
        zip.finish().unwrap();
    }
    assert!(matches!(
        crate::epub::open(&path),
        Err(crate::Error::NotAnEpub(_, _))
    ));
}

#[test]
fn a_root_yields_books_and_folders_of_prose_as_two_different_shapes() {
    let root = std::env::temp_dir().join("pr_text_scan_root");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("Notes")).unwrap();
    build_epub(&root.join("bocchi.epub"), true);
    std::fs::write(
        root.join("Notes/Chapter 2.txt"),
        "second
",
    )
    .unwrap();
    std::fs::write(
        root.join("Notes/Chapter 10.txt"),
        "tenth
",
    )
    .unwrap();
    std::fs::write(root.join("Notes/cover.jpg"), b"not text").unwrap();

    let found = crate::scan::scan_root(&root);
    assert_eq!(found.len(), 2, "one book, one folder of prose");

    let book = found
        .iter()
        .find(|s| s.title.starts_with("Bocchi"))
        .unwrap();
    assert_eq!(book.chapters.len(), 2);
    // Every chapter shares the container; the locator is what separates them.
    assert!(
        book.chapters
            .iter()
            .all(|c| c.path == root.join("bocchi.epub"))
    );
    assert_eq!(book.chapters[0].locator, "OEBPS/text/one.xhtml");
    // Spine position, because "Chapter Twenty" has no digits to parse.
    assert_eq!(book.chapters[1].number, Some(2.0));

    let notes = found.iter().find(|s| s.title == "Notes").unwrap();
    assert_eq!(notes.chapters.len(), 2, "the jpeg is not prose");
    // Numbered from the filename and sorted by it, so 10 follows 2 rather than sorting
    // before it the way a plain string compare would.
    assert_eq!(
        notes.chapters.iter().map(|c| c.number).collect::<Vec<_>>(),
        [Some(2.0), Some(10.0)]
    );

    // And reading one goes through the same call whichever container it came from.
    let from_book = crate::scan::read(&book.chapters[0].path, &book.chapters[0].locator).unwrap();
    assert_eq!(from_book.blocks[1].text(), "It began badly.");
    let from_file = crate::scan::read(&notes.chapters[0].path, "").unwrap();
    assert_eq!(from_file.blocks[0].text(), "second");

    let _ = std::fs::remove_dir_all(&root);
}
