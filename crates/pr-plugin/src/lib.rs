//! The sandboxed extension host. A source is a plugin with the `source` capability.
//!
//! One runtime, not one per reader. Building a separate extension system for manga and
//! for novels would mean two sandboxes, two manifests and two sets of security bugs for
//! one idea; the readers diverge at exactly one method, `content`, and nowhere else.
//!
//! **Extensions get no network API.** This is Paperback's central decision and the
//! reason a sandbox is meaningful rather than decorative: the host performs every
//! request and the extension only parses what comes back. It cannot open a socket,
//! cannot reach a host outside its manifest allowlist, and cannot exfiltrate anything,
//! because it was never handed the means.
//!
//! That is cheaper to guarantee here than it sounds. QuickJS is not a browser and not
//! Node: it ships with no `fetch`, no `XMLHttpRequest`, no `WebSocket`, no `require`
//! and no `process`. There is nothing to strip. The host *adds* one function, and that
//! function is the entire outward surface of a plugin.
//!
//! No I/O happens in this crate. `Fetcher` is a trait the host implements, which keeps
//! the runtime synchronous and testable (workspace convention: tokio lives in `pr-app`,
//! `pr-server` and `pr-engine`, and everything else is called via `spawn_blocking`).

pub mod repo;

use rquickjs::{
    CatchResultExt, Context, Function, Module, Object, Persistent, Promise, Runtime, Value,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Js(String),
    #[error("this is not a source: {0}")]
    Manifest(String),
    #[error("{id} tried to reach {host}, which is not in its manifest")]
    Blocked { id: String, host: String },
    #[error("{0} took too long and was stopped")]
    Timeout(String),
    #[error("{method} returned something this reader cannot use: {why}")]
    BadShape { method: &'static str, why: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Which reader a source feeds. The one place the two diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Manga,
    Novel,
}

/// What a source says about itself. Read from the bundle rather than from a repository
/// index: the index is what someone reads before installing, but the code is what
/// actually runs, and an index that could widen a plugin's network reach without
/// changing its code would be a hole rather than a convenience.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub lang: String,
    pub kind: Kind,
    pub nsfw: bool,
    /// Every host this plugin may reach, and nothing else is reachable.
    pub hosts: Vec<String>,
}

impl Manifest {
    /// Whether a URL is inside the allowlist.
    ///
    /// An exact host, or a subdomain of one. Sources serve covers and pages from CDNs
    /// on subdomains of the site, and an allowlist that forced every one of those to be
    /// listed would be an allowlist people copy a wildcard into.
    pub fn allows(&self, url: &str) -> bool {
        let Ok(parsed) = url::Url::parse(url) else {
            return false;
        };
        if !matches!(parsed.scheme(), "http" | "https") {
            return false;
        }
        let Some(host) = parsed.host_str() else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        self.hosts.iter().any(|allowed| {
            let allowed = allowed.trim().to_ascii_lowercase();
            !allowed.is_empty() && (host == allowed || host.ends_with(&format!(".{allowed}")))
        })
    }
}

/// A request the host is asked to perform on the plugin's behalf.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: String,
    /// After redirects, which is what a relative link in the body resolves against.
    pub final_url: String,
}

/// The host's side of the one function a plugin can call outward.
///
/// Synchronous on purpose. A plugin call already runs on a background job thread, so
/// blocking there costs nothing the reader can feel, and it keeps the host out of the
/// business of pumping a JavaScript microtask queue around its own I/O.
pub trait Fetcher: Send + Sync {
    fn fetch(&self, request: Request) -> std::result::Result<Response, String>;
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub memory_bytes: usize,
    /// Wall clock for one method call, enforced by the interrupt handler.
    pub deadline: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // Generous for parsing a page of HTML, nowhere near enough to be a problem.
            memory_bytes: 64 * 1024 * 1024,
            // A source that cannot answer in ten seconds is a source that is broken or
            // a site that is down, and either way the reader should be told.
            deadline: Duration::from_secs(10),
        }
    }
}

// ------------------------------------------------------------------- what comes back

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover: Option<String>,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub has_next: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SourceChapter {
    pub id: String,
    pub title: String,
    pub number: Option<f64>,
}

/// The one place the two readers diverge, and deliberately rather than accidentally.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    /// Manga: URLs the host fetches and decodes through `pr-image`.
    Pages(Vec<String>),
    /// Novel: markup the host sanitizes and normalizes through `pr-text`. Never
    /// rendered as it arrives -- that normalization is what makes it safe, and it
    /// happens in the app layer, not here.
    Html(String),
}

// --------------------------------------------------------------------------- the host

/// A loaded plugin, with its own isolate.
///
/// Not `Sync`: one isolate belongs to one thread. That is the isolation, not an
/// oversight -- two threads sharing a runtime would be two plugins sharing a heap.
pub struct Source {
    // Debug prints the manifest and nothing from inside the isolate: a plugin's heap is
    // not ours to put in a log.
    // Field order is drop order, and it is load-bearing here. A Persistent holds a
    // reference into the runtime's heap, so it has to be released before the runtime
    // is; the other way round, QuickJS asserts that its object list is non-empty at
    // teardown and takes the process with it.
    /// The bundle's default export, held on this side rather than parked on a global.
    /// A plugin's global namespace is the language plus `pan`, and keeping the source
    /// object out of it means a plugin cannot reach -- or reassign -- its own methods
    /// between calls.
    source: Persistent<Object<'static>>,
    context: Context,
    runtime: Runtime,
    manifest: Manifest,
    limits: Limits,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

impl Source {
    /// Load a bundle into a fresh isolate.
    ///
    /// The bundle is an ES module whose default export is the source object, which is
    /// the shape Mihon uses and the shape LNReader publishes.
    pub fn load(bundle: &str, fetcher: Arc<dyn Fetcher>, limits: Limits) -> Result<Self> {
        let runtime = Runtime::new().map_err(|e| Error::Js(e.to_string()))?;
        runtime.set_memory_limit(limits.memory_bytes);
        let context = Context::full(&runtime).map_err(|e| Error::Js(e.to_string()))?;

        // Loading is itself plugin code running, so it is bounded too: a bundle whose
        // top level loops forever must not take the thread with it.
        arm(&runtime, limits.deadline);

        let (manifest, source) =
            context.with(|ctx| -> Result<(Manifest, Persistent<Object<'static>>)> {
                let (module, done) = Module::declare(ctx.clone(), "source", bundle)
                    .catch(&ctx)
                    .map_err(|e| Error::Js(e.to_string()))?
                    .eval()
                    .catch(&ctx)
                    .map_err(|e| Error::Js(e.to_string()))?;
                done.finish::<()>()
                    .catch(&ctx)
                    .map_err(|e| Error::Js(e.to_string()))?;

                let default: Object = module
                    .get("default")
                    .map_err(|_| Error::Manifest("the bundle has no default export".into()))?;
                let manifest = read_manifest(&default)?;
                Ok((manifest, Persistent::save(&ctx, default)))
            })?;

        install_fetch(&context, &manifest, fetcher)?;

        Ok(Self {
            source,
            context,
            runtime,
            manifest,
            limits,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn popular(&self, page: u32) -> Result<Listing> {
        self.listing("popular", |ctx| Ok(vec![page.into_js(ctx)?]))
    }

    pub fn latest(&self, page: u32) -> Result<Listing> {
        self.listing("latest", |ctx| Ok(vec![page.into_js(ctx)?]))
    }

    pub fn search(&self, page: u32, query: &str) -> Result<Listing> {
        self.listing("search", |ctx| {
            Ok(vec![page.into_js(ctx)?, query.into_js(ctx)?])
        })
    }

    pub fn details(&self, entry_id: &str) -> Result<Entry> {
        self.call(
            "details",
            |ctx| Ok(vec![entry_id.into_js(ctx)?]),
            |v| read_entry(&object(v, "details")?),
        )
    }

    pub fn chapters(&self, entry_id: &str) -> Result<Vec<SourceChapter>> {
        self.call(
            "chapters",
            |ctx| Ok(vec![entry_id.into_js(ctx)?]),
            |v| {
                let list: Vec<Object> = v
                    .clone()
                    .into_array()
                    .and_then(|a| a.iter().collect::<rquickjs::Result<Vec<_>>>().ok())
                    .ok_or_else(|| Error::BadShape {
                        method: "chapters",
                        why: "expected an array of chapters".into(),
                    })?;
                list.iter().map(read_chapter).collect()
            },
        )
    }

    pub fn content(&self, chapter_id: &str) -> Result<Content> {
        self.call(
            "content",
            |ctx| Ok(vec![chapter_id.into_js(ctx)?]),
            |v| {
                let obj = object(v, "content")?;
                // Manga returns pages, novels return html. A source that returns both
                // or neither is a source with a bug, and saying so beats guessing.
                let pages: Option<Vec<String>> = obj.get("pages").ok();
                let html: Option<String> = obj.get("html").ok();
                match (pages, html) {
                    (Some(pages), None) => Ok(Content::Pages(pages)),
                    (None, Some(html)) => Ok(Content::Html(html)),
                    _ => Err(Error::BadShape {
                        method: "content",
                        why: "expected exactly one of { pages } or { html }".into(),
                    }),
                }
            },
        )
    }

    fn listing<A>(&self, method: &'static str, args: A) -> Result<Listing>
    where
        A: for<'js> FnOnce(&rquickjs::Ctx<'js>) -> rquickjs::Result<Vec<Value<'js>>>,
    {
        self.call(method, args, |v| {
            let obj = object(v, method)?;
            let entries: Vec<Object> = obj.get("entries").map_err(|_| Error::BadShape {
                method,
                why: "no entries array".into(),
            })?;
            Ok(Listing {
                entries: entries.iter().map(read_entry).collect::<Result<Vec<_>>>()?,
                has_next: obj.get("hasNext").unwrap_or(false),
            })
        })
    }

    /// Call one method, await its promise, and read the result.
    ///
    /// The deadline is re-armed here rather than once at load: an interrupt handler
    /// holding a deadline that has already passed would kill every later call, so each
    /// call gets its own budget.
    fn call<A, R, T>(&self, method: &'static str, args: A, read: R) -> Result<T>
    where
        A: for<'js> FnOnce(&rquickjs::Ctx<'js>) -> rquickjs::Result<Vec<Value<'js>>>,
        R: for<'js> FnOnce(&Value<'js>) -> Result<T>,
    {
        let started = Instant::now();
        arm(&self.runtime, self.limits.deadline);

        self.context.with(|ctx| {
            let source: Object = self
                .source
                .clone()
                .restore(&ctx)
                .map_err(|e| Error::Js(e.to_string()))?;
            let function: Function = source.get(method).map_err(|_| {
                Error::Manifest(format!(
                    "this source has no {method}() -- every source needs popular, latest, \
                 search, details, chapters and content"
                ))
            })?;

            let args = args(&ctx).map_err(|e| Error::Js(e.to_string()))?;
            let mut call = rquickjs::function::Args::new(ctx.clone(), args.len() + 1);
            call.this(source.clone())
                .map_err(|e| Error::Js(e.to_string()))?;
            call.push_args(args).map_err(|e| Error::Js(e.to_string()))?;

            let returned: Value = function
                .call_arg(call)
                .catch(&ctx)
                .map_err(|e| js_error(method, started, self.limits.deadline, &e.to_string()))?;

            // Every method is declared async, so the return is a promise -- but a source
            // written without `async` returns the value directly, and refusing that
            // would be pedantry rather than safety.
            let settled = match returned.as_promise() {
                Some(promise) => {
                    let promise = promise.clone();
                    // Drain the microtask queue. There is nothing to wait for: the
                    // host's fetch is synchronous, so by the time the outer promise
                    // stops making progress it has settled.
                    while ctx.execute_pending_job() {}
                    Promise::finish::<Value>(&promise)
                        .catch(&ctx)
                        .map_err(|e| {
                            js_error(method, started, self.limits.deadline, &e.to_string())
                        })?
                }
                None => returned,
            };
            read(&settled)
        })
    }
}

/// A QuickJS interrupt fires between operations, which is what makes an infinite loop
/// in plugin code survivable: the runtime stops it rather than the reader force-quitting.
fn arm(runtime: &Runtime, deadline: Duration) {
    let until = Instant::now() + deadline;
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() > until)));
}

/// QuickJS reports an interrupt as an ordinary exception, so the deadline is what tells
/// a plugin that threw from a plugin that ran away.
fn js_error(method: &'static str, started: Instant, deadline: Duration, message: &str) -> Error {
    if started.elapsed() >= deadline {
        Error::Timeout(method.to_owned())
    } else {
        Error::Js(format!("{method}: {message}"))
    }
}

/// Give the isolate its one outward function, and the shim that presents it as the
/// documented `await pan.fetch(url, { headers })`.
fn install_fetch(context: &Context, manifest: &Manifest, fetcher: Arc<dyn Fetcher>) -> Result<()> {
    let allow = manifest.clone();
    context.with(|ctx| -> Result<()> {
        let pan = Object::new(ctx.clone()).map_err(|e| Error::Js(e.to_string()))?;

        let raw = Function::new(
            ctx.clone(),
            move |ctx: rquickjs::Ctx,
                  url: String,
                  options: Option<Object>|
                  -> rquickjs::Result<String> {
                if !allow.allows(&url) {
                    // The refusal names the host, because the reader who sees this
                    // needs to know which site the plugin reached for.
                    let host = url::Url::parse(&url)
                        .ok()
                        .and_then(|u| u.host_str().map(str::to_owned))
                        .unwrap_or_else(|| url.clone());
                    return Err(throw(
                        &ctx,
                        &format!("{} is not in this source's manifest", host),
                    ));
                }

                let headers = options
                    .and_then(|o| o.get::<_, Object>("headers").ok())
                    .map(|h| {
                        h.props::<String, String>()
                            .filter_map(|p| p.ok())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                match fetcher.fetch(Request { url, headers }) {
                    Ok(response) => Ok(response.body),
                    Err(why) => Err(throw(&ctx, &why)),
                }
            },
        )
        .map_err(|e| Error::Js(e.to_string()))?;

        pan.set("__fetch", raw)
            .map_err(|e| Error::Js(e.to_string()))?;
        ctx.globals()
            .set("pan", pan)
            .map_err(|e| Error::Js(e.to_string()))?;

        // A thrown host error becomes a rejected promise, so a plugin can try/catch a
        // dead site the same way it would in a browser.
        ctx.eval::<(), _>(
            r#"pan.fetch = (url, options) => {
                 try { return Promise.resolve(pan.__fetch(url, options ?? {})); }
                 catch (e) { return Promise.reject(e); }
               };"#,
        )
        .catch(&ctx)
        .map_err(|e| Error::Js(e.to_string()))?;
        Ok(())
    })
}

fn throw(ctx: &rquickjs::Ctx<'_>, message: &str) -> rquickjs::Error {
    match rquickjs::String::from_str(ctx.clone(), message) {
        Ok(s) => ctx.throw(s.into_value()),
        Err(e) => e,
    }
}

// -------------------------------------------------------------------------- shape

fn object<'js>(value: &Value<'js>, method: &'static str) -> Result<Object<'js>> {
    value.as_object().cloned().ok_or_else(|| Error::BadShape {
        method,
        why: "expected an object".into(),
    })
}

fn read_manifest(source: &Object<'_>) -> Result<Manifest> {
    let required = |name: &str| -> Result<String> {
        source
            .get::<_, String>(name)
            .map_err(|_| Error::Manifest(format!("no {name}")))
    };
    // Read in the order someone reads a manifest, so the first complaint is about the
    // first thing missing rather than whichever field this function happened to touch.
    let id = required("id")?;
    let name = required("name")?;
    let kind = match required("kind")?.as_str() {
        "manga" => Kind::Manga,
        "novel" => Kind::Novel,
        other => {
            return Err(Error::Manifest(format!(
                "kind is {other:?}, which is neither \"manga\" nor \"novel\""
            )));
        }
    };
    let hosts: Vec<String> = source.get("hosts").map_err(|_| {
        Error::Manifest(
            "no hosts -- a source with no allowlist can reach nothing, so say which sites it needs"
                .into(),
        )
    })?;
    if hosts.is_empty() {
        return Err(Error::Manifest("hosts is empty".into()));
    }

    Ok(Manifest {
        id,
        name,
        version: source.get("version").unwrap_or_else(|_| "0".to_owned()),
        lang: source.get("lang").unwrap_or_else(|_| "en".to_owned()),
        kind,
        nsfw: source.get("nsfw").unwrap_or(false),
        hosts,
    })
}

fn read_entry(entry: &Object<'_>) -> Result<Entry> {
    Ok(Entry {
        id: entry.get("id").map_err(|_| Error::BadShape {
            method: "entry",
            why: "an entry with no id cannot be opened again".into(),
        })?,
        title: entry.get("title").unwrap_or_default(),
        cover: entry.get("cover").ok(),
        author: entry.get("author").unwrap_or_default(),
        description: entry.get("description").unwrap_or_default(),
    })
}

fn read_chapter(chapter: &Object<'_>) -> Result<SourceChapter> {
    Ok(SourceChapter {
        id: chapter.get("id").map_err(|_| Error::BadShape {
            method: "chapters",
            why: "a chapter with no id cannot be fetched".into(),
        })?,
        title: chapter.get("title").unwrap_or_default(),
        number: chapter.get("number").ok(),
    })
}

use rquickjs::IntoJs;

#[cfg(test)]
mod tests;
