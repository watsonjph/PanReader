use super::*;

const BUNDLE: &str = r#"
export default {
  id: "example", name: "Example", version: "1.0.0", lang: "en",
  kind: "novel", nsfw: false, hosts: ["example.com"],
  async popular(page) { return { entries: [{ id: "a", title: "A" + page }], hasNext: false }; },
  async latest(page) { return { entries: [{ id: "l", title: "latest" }], hasNext: false }; },
  async search(page, query) { return { entries: [{ id: "s", title: query }], hasNext: false }; },
  async details(id) { return { id, title: "A" }; },
  async chapters(id) { return [{ id: "c1", title: "Chapter 1", number: 1 }]; },
  async content(id) { return { html: "<p>words</p>" }; },
};
"#;

#[test]
fn a_source_runs_on_its_own_thread_and_answers_over_the_channel() {
    let source = Loaded::start(
        "example",
        BUNDLE.to_owned(),
        Limits::default(),
        Arc::default(),
        None,
    )
    .unwrap();
    assert_eq!(source.manifest.id, "example");
    assert_eq!(source.manifest.kind, Kind::Novel);

    // Three listings behind one call, because to the reader they are one thing: a page
    // of entries. A query searches whichever tab they are on.
    assert_eq!(source.browse(2, "", false).unwrap().entries[0].title, "A2");
    assert_eq!(source.browse(1, "", true).unwrap().entries[0].id, "l");
    assert_eq!(
        source.browse(1, "needle", false).unwrap().entries[0].title,
        "needle"
    );
    assert_eq!(
        source.browse(1, "needle", true).unwrap().entries[0].title,
        "needle"
    );
    assert_eq!(source.details("a").unwrap().title, "A");
    assert_eq!(source.chapters("a").unwrap().len(), 1);
    assert_eq!(
        source.content("c1").unwrap(),
        Content::Html("<p>words</p>".into())
    );
}

/// The isolate is not `Send`, so it never leaves its thread -- but `Loaded` is what the
/// app holds, and app state is shared across Tauri's command threads.
#[test]
fn a_loaded_source_can_be_shared_across_threads_even_though_its_isolate_cannot() {
    fn assert_shareable<T: Send + Sync>() {}
    assert_shareable::<Loaded>();

    let source = Arc::new(
        Loaded::start(
            "example",
            BUNDLE.to_owned(),
            Limits::default(),
            Arc::default(),
            None,
        )
        .unwrap(),
    );
    let handles: Vec<_> = (0..4)
        .map(|n| {
            let source = source.clone();
            std::thread::spawn(move || {
                source.browse(n, "", false).unwrap().entries[0]
                    .title
                    .clone()
            })
        })
        .collect();
    let mut titles: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    titles.sort();
    assert_eq!(titles, ["A0", "A1", "A2", "A3"]);
}

/// Dropping the handle drops the only sender, the loop ends, and the isolate goes with
/// it. Removing a source is a `drop`, not a shutdown message someone forgets to send.
#[test]
fn dropping_a_source_stops_its_thread() {
    let source = Loaded::start(
        "example",
        BUNDLE.to_owned(),
        Limits::default(),
        Arc::default(),
        None,
    )
    .unwrap();
    assert!(source.is_running());
    let alive = source.alive.clone();
    drop(source);

    // The thread owns the only strong reference, so this goes to zero when it returns.
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive.strong_count() > 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(
        alive.strong_count(),
        0,
        "the source thread outlived its handle"
    );
}

#[test]
fn a_bundle_that_is_not_a_source_fails_at_load_rather_than_at_first_use() {
    let err = Loaded::start(
        "bad",
        "export default { id: 'bad' };".into(),
        Limits::default(),
        Arc::default(),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("no name"), "got {err}");
}

/// The rate limit is the host's, because a plugin has no reason to impose one on
/// itself and no way to be trusted with it.
#[test]
fn requests_from_one_source_are_spaced_out() {
    let fetcher = HostFetcher::new("example", Arc::default(), None).unwrap();
    let started = Instant::now();
    for _ in 0..3 {
        fetcher.wait_turn();
    }
    // The first goes immediately; the next two wait their turn.
    assert!(
        started.elapsed() >= MIN_GAP * 2,
        "three requests took {:?}, less than two gaps",
        started.elapsed()
    );
}

#[test]
fn a_repository_index_resolves_its_bundles_against_its_own_url() {
    let index = pr_plugin::repo::parse_index(
        r#"{ "schema": 1, "sources": [
             { "id": "a", "name": "A", "version": "1.0.0", "lang": "en",
               "kind": "novel", "bundle": "bundles/a-1.0.0.js", "sha256": "abc" },
             { "id": "b", "name": "B", "version": "2", "lang": "ja",
               "kind": "manga", "bundle": "https://cdn.test/b.js" },
             { "id": "broken", "kind": "video", "bundle": "x.js" }
           ] }"#,
        "https://repo.test/some/index.json",
    )
    .unwrap();

    assert_eq!(
        index.sources.len(),
        2,
        "the malformed row is skipped, not fatal"
    );
    assert_eq!(
        index.sources[0].bundle,
        "https://repo.test/some/bundles/a-1.0.0.js"
    );
    assert_eq!(index.sources[1].bundle, "https://cdn.test/b.js");
    assert_eq!(index.sources[1].kind, Kind::Manga);
    assert!(index.sources[1].sha256.is_none());
    assert!(!index.sources[0].foreign);
}

/// The shim, and the whole of it: their index is a bare array of novel plugins.
#[test]
fn an_lnreader_index_reads_as_ours() {
    let index = pr_plugin::repo::parse_index(
        r#"[ { "id": "novelupdates", "name": "Novel Updates", "site": "https://nu.test",
              "lang": "English", "version": "1.2.3", "url": "plugins/english/nu.js" } ]"#,
        "https://raw.test/lnreader/plugins.min.json",
    )
    .unwrap();

    let only = &index.sources[0];
    assert_eq!(only.id, "novelupdates");
    assert_eq!(
        only.kind,
        Kind::Novel,
        "every LNReader plugin is a novel source"
    );
    assert_eq!(
        only.bundle,
        "https://raw.test/lnreader/plugins/english/nu.js"
    );
    assert!(only.sha256.is_none(), "their index publishes no hashes");
    assert!(only.foreign, "so the reader can tell the ecosystems apart");
}

#[test]
fn a_bundle_that_is_not_what_the_index_listed_is_refused() {
    // sha256("x") -- anything else must not pass.
    let real = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881";
    assert!(pr_plugin::repo::verify(b"x", real).is_ok());
    assert!(pr_plugin::repo::verify(b"y", real).is_err());
    assert!(pr_plugin::repo::verify(b"x", "not a hash").is_err());
}

/// The index says what a reader agrees to install; the bundle says what runs. Where
/// they disagree about identity, the install is a lie and is refused.
#[test]
fn an_index_cannot_claim_an_identity_the_bundle_does_not_have() {
    let manifest = Manifest {
        id: "real".into(),
        name: "Real".into(),
        version: "1".into(),
        lang: "en".into(),
        kind: Kind::Novel,
        nsfw: false,
        hosts: vec!["example.com".into()],
    };
    let listed = |id: &str, kind| pr_plugin::repo::Listed {
        id: id.into(),
        name: "x".into(),
        version: "1".into(),
        lang: "en".into(),
        kind,
        nsfw: false,
        bundle: "https://repo.test/x.js".into(),
        sha256: None,
        foreign: false,
    };

    assert!(pr_plugin::repo::agrees(&listed("real", Kind::Novel), &manifest).is_ok());
    // Taking over another source's id would take over its library entries.
    assert!(pr_plugin::repo::agrees(&listed("other", Kind::Novel), &manifest).is_err());
    // Filing a novel under the image reader.
    assert!(pr_plugin::repo::agrees(&listed("real", Kind::Manga), &manifest).is_err());
}
