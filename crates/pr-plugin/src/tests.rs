use super::*;
use std::sync::Mutex;

/// A host that answers from a table and records what it was asked for.
///
/// The recording is the point of several tests below: the guarantee is not that a
/// plugin behaves, it is that a plugin *cannot* reach anything the host did not fetch
/// for it, and the only way to show that is to look at what the host was asked to do.
#[derive(Default)]
struct Stub {
    asked: Mutex<Vec<String>>,
    body: String,
}

impl Stub {
    fn new(body: &str) -> Arc<Self> {
        Arc::new(Self {
            asked: Mutex::new(Vec::new()),
            body: body.to_owned(),
        })
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl Fetcher for Stub {
    fn fetch(&self, request: Request) -> std::result::Result<String, String> {
        self.asked.lock().unwrap().push(request.url);
        Ok(self.body.clone())
    }
}

/// A host that is always down, for the paths where a plugin has to cope.
struct Dead;
impl Fetcher for Dead {
    fn fetch(&self, _: Request) -> std::result::Result<String, String> {
        Err("connection refused".into())
    }
}

const SOURCE: &str = r#"
export default {
  id: "example",
  name: "Example",
  version: "1.0.0",
  lang: "en",
  kind: "novel",
  nsfw: false,
  hosts: ["example.com"],

  async popular(page) {
    const html = await pan.fetch(`https://example.com/popular/${page}`);
    return { entries: [{ id: "a", title: "A" + page, cover: "https://cdn.example.com/a.jpg" }],
             hasNext: page < 3 };
  },
  async latest(page) { return { entries: [], hasNext: false }; },
  async search(page, query) {
    return { entries: [{ id: "q", title: query }], hasNext: false };
  },
  async details(entryId) {
    return { id: entryId, title: "A", author: "Someone", description: "..." };
  },
  async chapters(entryId) {
    return [{ id: "c1", title: "Chapter 1", number: 1 },
            { id: "c2", title: "Chapter 2", number: 2 }];
  },
  async content(chapterId) {
    const html = await pan.fetch(`https://example.com/read/${chapterId}`);
    return { html };
  },
};
"#;

fn load(bundle: &str, fetcher: Arc<dyn Fetcher>) -> Result<Source> {
    Source::load(bundle, fetcher, Limits::default())
}

#[test]
fn a_source_declares_itself_and_answers_the_six_methods() {
    let host = Stub::new("<p>the chapter</p>");
    let source = load(SOURCE, host.clone()).unwrap();

    let m = source.manifest();
    assert_eq!(m.id, "example");
    assert_eq!(m.kind, Kind::Novel);
    assert_eq!(m.hosts, ["example.com"]);

    let popular = source.popular(2).unwrap();
    assert_eq!(popular.entries[0].title, "A2");
    assert!(popular.has_next);
    assert!(!source.popular(3).unwrap().has_next, "page 3 is the last");

    assert_eq!(
        source.search(1, "needle").unwrap().entries[0].title,
        "needle"
    );
    assert_eq!(source.details("a").unwrap().author, "Someone");

    let chapters = source.chapters("a").unwrap();
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[1].number, Some(2.0));

    // The one place the readers diverge.
    assert_eq!(
        source.content("c1").unwrap(),
        Content::Html("<p>the chapter</p>".into())
    );

    // And every request went through the host, which is the whole design.
    assert_eq!(
        host.asked(),
        [
            "https://example.com/popular/2",
            "https://example.com/popular/3",
            "https://example.com/read/c1",
        ]
    );
}

/// The rule worth copying exactly, from Paperback: an extension cannot construct a
/// request. Not "is prevented from" -- was never given the means.
///
/// Asserted as a whole-surface snapshot rather than a list of names I thought to check.
/// Every global below is pure ECMAScript plus `atob`/`btoa`/`performance`: nothing
/// reaches the network, the filesystem or the process. `SharedArrayBuffer` and `Atomics`
/// are present and inert -- there is no Worker in this isolate to share a buffer with.
///
/// A new name here is not necessarily bad, but it must be a decision. This test turns
/// an rquickjs upgrade that quietly adds a capability into a failure rather than a
/// surprise, which is the only way a sandbox stays one.
#[test]
fn a_plugin_is_handed_a_language_and_no_capabilities() {
    const EXPECTED: &[&str] = &[
        "AggregateError",
        "Array",
        "ArrayBuffer",
        "AsyncDisposableStack",
        "Atomics",
        "BigInt",
        "BigInt64Array",
        "BigUint64Array",
        "Boolean",
        "DOMException",
        "DataView",
        "Date",
        "DisposableStack",
        "Error",
        "EvalError",
        "FinalizationRegistry",
        "Float16Array",
        "Float32Array",
        "Float64Array",
        "Function",
        "Infinity",
        "Int16Array",
        "Int32Array",
        "Int8Array",
        "InternalError",
        "Iterator",
        "JSON",
        "Map",
        "Math",
        "NaN",
        "Number",
        "Object",
        "Promise",
        "Proxy",
        "RangeError",
        "ReferenceError",
        "Reflect",
        "RegExp",
        "Set",
        "SharedArrayBuffer",
        "String",
        "SuppressedError",
        "Symbol",
        "SyntaxError",
        "TypeError",
        "URIError",
        "Uint16Array",
        "Uint32Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "WeakMap",
        "WeakRef",
        "WeakSet",
        "atob",
        "btoa",
        "decodeURI",
        "decodeURIComponent",
        "encodeURI",
        "encodeURIComponent",
        "escape",
        "eval",
        "globalThis",
        "isFinite",
        "isNaN",
        "parseFloat",
        "parseInt",
        "performance",
        "queueMicrotask",
        "undefined",
        "unescape",
        // Ours, and the entire outward surface of a plugin.
        "pan",
    ];

    let probe = r#"
export default {
  id: "probe", name: "Probe", lang: "en", kind: "novel", hosts: ["example.com"],
  async popular(page) {
    return { entries: Object.getOwnPropertyNames(globalThis).sort()
                        .map((n) => ({ id: n, title: n })), hasNext: false };
  },
  async latest(p) { return { entries: [], hasNext: false }; },
  async search(p, q) { return { entries: [], hasNext: false }; },
  async details(i) { return { id: i, title: "" }; },
  async chapters(i) { return []; },
  async content(i) { return { html: "" }; },
};
"#;
    let host = Stub::new("");
    let found = load(probe, host.clone()).unwrap().popular(1).unwrap();
    let names: Vec<String> = found.entries.into_iter().map(|e| e.id).collect();

    let mut expected: Vec<&str> = EXPECTED.to_vec();
    expected.sort_unstable();
    let unexpected: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|n| !expected.contains(n))
        .collect();
    assert!(
        unexpected.is_empty(),
        "the isolate grew capabilities nobody decided on: {unexpected:?}"
    );

    // Nothing was fetched to answer that, either.
    assert!(host.asked().is_empty());
}

/// The allowlist is enforced where the request is performed, not where it is described.
#[test]
fn a_host_outside_the_manifest_is_refused_and_the_plugin_can_see_why() {
    let host = Stub::new("ok");
    let sneaky = r#"
export default {
  id: "sneaky", name: "Sneaky", lang: "en", kind: "novel", hosts: ["example.com"],
  async popular(page) {
    const tried = [];
    for (const url of ["https://evil.test/steal",
                       "http://example.com.evil.test/steal",
                       "file:///etc/passwd",
                       "https://cdn.example.com/fine.jpg"]) {
      try { await pan.fetch(url); tried.push("allowed " + url); }
      catch (e) { tried.push("refused " + url); }
    }
    return { entries: tried.map((t, i) => ({ id: String(i), title: t })), hasNext: false };
  },
  async latest(p) { return { entries: [], hasNext: false }; },
  async search(p, q) { return { entries: [], hasNext: false }; },
  async details(i) { return { id: i, title: "" }; },
  async chapters(i) { return []; },
  async content(i) { return { html: "" }; },
};
"#;
    let out = load(sneaky, host.clone()).unwrap().popular(1).unwrap();
    let verdicts: Vec<&str> = out.entries.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(
        verdicts,
        [
            "refused https://evil.test/steal",
            // A host that merely *contains* the allowed one is a different host.
            "refused http://example.com.evil.test/steal",
            // Only http(s). A plugin cannot read the disk through a URL scheme.
            "refused file:///etc/passwd",
            // Subdomains are in, because covers and pages live on CDNs.
            "allowed https://cdn.example.com/fine.jpg",
        ]
    );
    assert_eq!(
        host.asked(),
        ["https://cdn.example.com/fine.jpg"],
        "the host performed only the allowed request"
    );
}

#[test]
fn the_allowlist_matches_hosts_not_substrings() {
    let m = Manifest {
        id: "m".into(),
        name: "m".into(),
        version: "1".into(),
        lang: "en".into(),
        kind: Kind::Novel,
        nsfw: false,
        hosts: vec!["example.com".into()],
    };
    assert!(m.allows("https://example.com/a"));
    assert!(m.allows("https://cdn.example.com/a"));
    assert!(m.allows("HTTPS://EXAMPLE.COM/a"));
    assert!(!m.allows("https://notexample.com/a"));
    assert!(!m.allows("https://example.com.evil.test/a"));
    assert!(!m.allows("file:///etc/passwd"));
    assert!(!m.allows("not a url"));
}

/// Invariant 11: a plugin that misbehaves is killed by the host, not noticed by the
/// user. An extension that can hang the reader is an extension that can hang the reader
/// whether or not it meant to.
#[test]
fn a_plugin_that_never_returns_is_stopped() {
    let spin = r#"
export default {
  id: "spin", name: "Spin", lang: "en", kind: "novel", hosts: ["example.com"],
  async popular(page) { while (true) {} },
  async latest(p) { return { entries: [], hasNext: false }; },
  async search(p, q) { return { entries: [], hasNext: false }; },
  async details(i) { return { id: i, title: "" }; },
  async chapters(i) { return []; },
  async content(i) { return { html: "" }; },
};
"#;
    let limits = Limits {
        deadline: std::time::Duration::from_millis(150),
        ..Default::default()
    };
    let source = Source::load(spin, Stub::new(""), limits).unwrap();

    let started = std::time::Instant::now();
    assert!(matches!(source.popular(1), Err(Error::Timeout(_))));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );

    // And the isolate is still usable afterwards: one bad call is not a dead source.
    assert_eq!(source.latest(1).unwrap().entries.len(), 0);
}

#[test]
fn a_plugin_that_allocates_without_end_is_stopped() {
    let greedy = r#"
export default {
  id: "greedy", name: "Greedy", lang: "en", kind: "novel", hosts: ["example.com"],
  async popular(page) { let s = []; while (true) { s.push("x".repeat(100000)); } },
  async latest(p) { return { entries: [], hasNext: false }; },
  async search(p, q) { return { entries: [], hasNext: false }; },
  async details(i) { return { id: i, title: "" }; },
  async chapters(i) { return []; },
  async content(i) { return { html: "" }; },
};
"#;
    let limits = Limits {
        memory_bytes: 4 * 1024 * 1024,
        ..Default::default()
    };
    let source = Source::load(greedy, Stub::new(""), limits).unwrap();
    assert!(source.popular(1).is_err());
}

/// A source's own top level is plugin code too, so it is bounded the same way.
#[test]
fn a_bundle_that_hangs_while_loading_is_stopped_rather_than_taking_the_thread() {
    let limits = Limits {
        deadline: std::time::Duration::from_millis(150),
        ..Default::default()
    };
    let hang = "while (true) {} export default {};";
    let started = std::time::Instant::now();
    assert!(Source::load(hang, Stub::new(""), limits).is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn a_dead_site_reaches_the_plugin_as_a_catchable_rejection() {
    let coping = r#"
export default {
  id: "coping", name: "Coping", lang: "en", kind: "novel", hosts: ["example.com"],
  async popular(page) {
    try { await pan.fetch("https://example.com/x"); return { entries: [], hasNext: false }; }
    catch (e) { return { entries: [{ id: "err", title: String(e) }], hasNext: false }; }
  },
  async latest(p) { return { entries: [], hasNext: false }; },
  async search(p, q) { return { entries: [], hasNext: false }; },
  async details(i) { return { id: i, title: "" }; },
  async chapters(i) { return []; },
  async content(i) { return { html: "" }; },
};
"#;
    let out = load(coping, Arc::new(Dead)).unwrap().popular(1).unwrap();
    assert!(
        out.entries[0].title.contains("connection refused"),
        "got {:?}",
        out.entries[0].title
    );
}

#[test]
fn a_bundle_that_is_not_a_source_is_refused_with_a_reason() {
    for (bundle, expected) in [
        ("export default { id: 'x' };", "no name"),
        ("export const nope = 1;", "no default export"),
        (
            "export default { id:'x', name:'x', lang:'en', kind:'video', hosts:['a.com'] };",
            "neither",
        ),
        (
            "export default { id:'x', name:'x', lang:'en', kind:'novel' };",
            "no hosts",
        ),
        (
            "export default { id:'x', name:'x', lang:'en', kind:'novel', hosts:[] };",
            "hosts is empty",
        ),
    ] {
        let err = load(bundle, Stub::new("")).unwrap_err().to_string();
        assert!(
            err.contains(expected),
            "{bundle:?} gave {err:?}, wanted something with {expected:?}"
        );
    }
}

/// A manga source and a novel source differ at `content` and nowhere else, which is
/// what lets one host serve both readers.
#[test]
fn a_manga_source_returns_pages_where_a_novel_returns_html() {
    let manga = SOURCE
        .replace(r#"kind: "novel""#, r#"kind: "manga""#)
        .replace(
            "return { html };",
            "return { pages: [html + '/1.jpg', html + '/2.jpg'] };",
        );
    let source = load(&manga, Stub::new("https://cdn.example.com/c1")).unwrap();
    assert_eq!(source.manifest().kind, Kind::Manga);
    assert_eq!(
        source.content("c1").unwrap(),
        Content::Pages(vec![
            "https://cdn.example.com/c1/1.jpg".into(),
            "https://cdn.example.com/c1/2.jpg".into(),
        ])
    );
}

#[test]
fn a_content_that_is_neither_shape_is_named_rather_than_guessed() {
    let confused = SOURCE.replace("return { html };", "return { pages: ['a'], html };");
    let err = load(&confused, Stub::new("x"))
        .unwrap()
        .content("c1")
        .unwrap_err();
    assert!(matches!(
        err,
        Error::BadShape {
            method: "content",
            ..
        }
    ));
}

/// Headers are the plugin's to set and the host's to send, which is what makes a
/// referer-checking site work without the plugin holding a socket.
#[test]
fn headers_the_plugin_sets_reach_the_host() {
    #[derive(Default)]
    struct Spy(Mutex<Vec<(String, String)>>);
    impl Fetcher for Spy {
        fn fetch(&self, request: Request) -> std::result::Result<String, String> {
            *self.0.lock().unwrap() = request.headers;
            Ok(String::new())
        }
    }
    let spy = Arc::new(Spy::default());
    let bundle = SOURCE.replace(
        r#"await pan.fetch(`https://example.com/popular/${page}`)"#,
        r#"await pan.fetch("https://example.com/p", { headers: { Referer: "https://example.com/" } })"#,
    );
    load(&bundle, spy.clone()).unwrap().popular(1).unwrap();
    assert_eq!(
        spy.0.lock().unwrap().clone(),
        [("Referer".to_owned(), "https://example.com/".to_owned())]
    );
}
