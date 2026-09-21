//! The host side of the plugin runtime: one thread per loaded source, and the fetcher
//! that performs every request on its behalf.
//!
//! A QuickJS runtime is not `Send`, which is not an inconvenience to work around --
//! it is the isolation, stated by the type system. So a loaded source lives on its own
//! thread and the app talks to it over a channel. That also gives, for free, the two
//! things a source host needs anyway: calls into one plugin are serialised, and there
//! is exactly one place to put the rate limit.

use anyhow::{Context as _, bail};
use pr_plugin::{Content, Entry, Fetcher, Kind, Limits, Listing, Manifest, Request, Response};
use std::sync::Arc;
use std::sync::mpsc::{Sender, SyncSender, sync_channel};
use std::time::{Duration, Instant};

/// Minimum gap between two requests from one source.
///
/// Polite by default and not configurable per source, because the setting a source
/// author would reach for is "none". Sites that ask for slower say so with a 429, which
/// `backoff` then honours.
const MIN_GAP: Duration = Duration::from_millis(500);

/// How long a source's own server gets before we give up on it. Separate from the
/// plugin's CPU deadline: a slow site is not a misbehaving plugin.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

// ------------------------------------------------------------------------- fetching

/// Performs a plugin's requests, one source's worth.
///
/// `reqwest::blocking` rather than the async client plus a runtime handle. The actor
/// thread is a plain OS thread, so there is no runtime to block on and no class of
/// "cannot block inside a runtime" bug to reason about; the blocking client's internal
/// thread is a smaller cost than that reasoning.
struct HostFetcher {
    client: reqwest::blocking::Client,
    id: String,
    /// When the next request may go out. The rate limit lives here rather than in the
    /// plugin because a plugin cannot be trusted to rate-limit itself, and would have
    /// no reason to.
    next: parking_lot::Mutex<Instant>,
}

impl HostFetcher {
    fn new(id: &str) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                // Our own name and version. Invariant 13: we identify as what we are
                // rather than impersonating a browser.
                .user_agent(concat!("PanReader/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(10))
                .timeout(HTTP_TIMEOUT)
                .build()
                .context("could not build an http client for a source")?,
            id: id.to_owned(),
            next: parking_lot::Mutex::new(Instant::now()),
        })
    }

    /// Sleep until this source is allowed to speak again.
    fn wait_turn(&self) {
        let sleep = {
            let mut next = self.next.lock();
            let now = Instant::now();
            let wait = next.saturating_duration_since(now);
            *next = now.max(*next) + MIN_GAP;
            wait
        };
        if !sleep.is_zero() {
            std::thread::sleep(sleep);
        }
    }
}

impl Fetcher for HostFetcher {
    fn fetch(&self, request: Request) -> Result<Response, String> {
        self.wait_turn();

        let mut outgoing = self.client.get(&request.url);
        for (name, value) in &request.headers {
            outgoing = outgoing.header(name.as_str(), value.as_str());
        }

        let response = outgoing
            .send()
            .map_err(|e| format!("{}: {e}", request.url))?;
        let status = response.status();
        let final_url = response.url().to_string();

        // An interstitial is not a transport failure, and saying so plainly is the
        // whole of our challenge story until the webview path in invariant 13 exists.
        // A source that meets one is reported to the reader rather than worked around.
        if matches!(status.as_u16(), 403 | 503) {
            tracing::info!(source = %self.id, url = %request.url, %status, "challenged");
        }
        if status.as_u16() == 429 {
            // The one backoff a site explicitly asks for.
            let retry = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5)
                .min(60);
            *self.next.lock() = Instant::now() + Duration::from_secs(retry);
            return Err(format!("{} asked us to slow down", host_of(&request.url)));
        }
        if !status.is_success() {
            return Err(format!("{} returned {status}", host_of(&request.url)));
        }

        let body = response
            .text()
            .map_err(|e| format!("{} sent something unreadable: {e}", request.url))?;
        Ok(Response {
            status: status.as_u16(),
            body,
            final_url,
        })
    }
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
}

/// Fetch a repository index or a bundle. Not a plugin's request, so no allowlist and no
/// per-source rate limit -- the reader typed this URL themselves.
pub fn get_text(url: &str) -> anyhow::Result<String> {
    let parsed = url::Url::parse(url).with_context(|| format!("{url} is not a URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("refusing a {} url: {url}", parsed.scheme());
    }
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("PanReader/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(HTTP_TIMEOUT)
        .build()?;
    let response = client.get(parsed).send()?.error_for_status()?;
    Ok(response.text()?)
}

// ---------------------------------------------------------------------------- actor

/// What the app asks a loaded source to do. One variant per method on the interface,
/// plus the reply channel, because the isolate is on the other side of a thread.
#[derive(Debug)]
enum Job {
    Popular(u32, SyncSender<pr_plugin::Result<Listing>>),
    Latest(u32, SyncSender<pr_plugin::Result<Listing>>),
    Search(u32, String, SyncSender<pr_plugin::Result<Listing>>),
    Details(String, SyncSender<pr_plugin::Result<Entry>>),
    Chapters(
        String,
        SyncSender<pr_plugin::Result<Vec<pr_plugin::SourceChapter>>>,
    ),
    Content(String, SyncSender<pr_plugin::Result<Content>>),
}

/// A loaded source, as the rest of the app sees it.
///
/// The manifest is cloned out at load time so the library and the UI can read what a
/// source is without a round trip to its thread -- listing thirty installed sources
/// should not wake thirty isolates.
#[derive(Debug)]
pub struct Loaded {
    pub manifest: Manifest,
    jobs: Sender<Job>,
    /// Held by the source's thread and by nobody else, so its absence means the thread
    /// has ended -- either because this handle was dropped, or because the isolate died
    /// on its own. The UI needs to tell a removed source from a crashed one.
    alive: std::sync::Weak<()>,
}

impl Loaded {
    /// Start a source on its own thread.
    ///
    /// Blocks until the bundle has loaded or failed, because a source that does not
    /// load is not a source and the caller should hear about it now rather than at the
    /// first browse.
    pub fn start(id: &str, bundle: String, limits: Limits) -> anyhow::Result<Self> {
        let fetcher = Arc::new(HostFetcher::new(id)?);
        let (jobs, inbox) = std::sync::mpsc::channel::<Job>();
        let (ready, loaded) = sync_channel::<Result<Manifest, String>>(1);
        let name = id.to_owned();
        let heartbeat = Arc::new(());
        let alive = Arc::downgrade(&heartbeat);

        std::thread::Builder::new()
            .name(format!("source:{id}"))
            .spawn(move || {
                // Moved in so it lives exactly as long as this thread does.
                let _heartbeat = heartbeat;
                let source = match pr_plugin::Source::load(&bundle, fetcher, limits) {
                    Ok(source) => {
                        let _ = ready.send(Ok(source.manifest().clone()));
                        source
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e.to_string()));
                        return;
                    }
                };
                // The bundle text is a plugin's own source and can be large; nothing
                // needs it again once the isolate holds it.
                drop(bundle);

                // Ends when the last sender drops, which is how removing a source stops
                // its thread: no shutdown message to forget to send.
                for job in inbox {
                    match job {
                        Job::Popular(page, reply) => {
                            let _ = reply.send(source.popular(page));
                        }
                        Job::Latest(page, reply) => {
                            let _ = reply.send(source.latest(page));
                        }
                        Job::Search(page, query, reply) => {
                            let _ = reply.send(source.search(page, &query));
                        }
                        Job::Details(id, reply) => {
                            let _ = reply.send(source.details(&id));
                        }
                        Job::Chapters(id, reply) => {
                            let _ = reply.send(source.chapters(&id));
                        }
                        Job::Content(id, reply) => {
                            let _ = reply.send(source.content(&id));
                        }
                    }
                }
                tracing::debug!(source = %name, "source thread stopped");
            })
            .context("could not start a thread for a source")?;

        match loaded.recv() {
            Ok(Ok(manifest)) => Ok(Self {
                manifest,
                jobs,
                alive,
            }),
            Ok(Err(why)) => bail!("{why}"),
            Err(_) => bail!("the source thread died while loading"),
        }
    }

    /// Whether the isolate is still there. False after this handle is dropped, and
    /// false if the plugin thread ended on its own.
    pub fn is_running(&self) -> bool {
        self.alive.strong_count() > 0
    }

    fn ask<T>(
        &self,
        make: impl FnOnce(SyncSender<pr_plugin::Result<T>>) -> Job,
    ) -> anyhow::Result<T> {
        let (reply, answer) = sync_channel(1);
        self.jobs
            .send(make(reply))
            .map_err(|_| anyhow::anyhow!("{} is not running", self.manifest.id))?;
        // No timeout here on purpose: the plugin's own deadline bounds the call, and a
        // second timer racing it would only add a way to report the wrong reason.
        answer
            .recv()
            .map_err(|_| anyhow::anyhow!("{} stopped while answering", self.manifest.id))?
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// The three listings behind one call, because they are one thing to the reader:
    /// a page of entries. A query searches; without one, `latest` or `popular` is the
    /// tab they are on.
    pub fn browse(&self, page: u32, query: &str, latest: bool) -> anyhow::Result<Listing> {
        if !query.trim().is_empty() {
            self.ask(|reply| Job::Search(page, query.to_owned(), reply))
        } else if latest {
            self.ask(|reply| Job::Latest(page, reply))
        } else {
            self.ask(|reply| Job::Popular(page, reply))
        }
    }

    pub fn details(&self, entry_id: &str) -> anyhow::Result<Entry> {
        self.ask(|reply| Job::Details(entry_id.to_owned(), reply))
    }

    pub fn chapters(&self, entry_id: &str) -> anyhow::Result<Vec<pr_plugin::SourceChapter>> {
        self.ask(|reply| Job::Chapters(entry_id.to_owned(), reply))
    }

    pub fn content(&self, chapter_id: &str) -> anyhow::Result<Content> {
        self.ask(|reply| Job::Content(chapter_id.to_owned(), reply))
    }
}

// -------------------------------------------------------------------------- install

/// Fetch a bundle, check it against what the repository claimed, and load it once to
/// see that it is a source at all.
///
/// Loading before storing is the point: a bundle that is not a source, or that lies
/// about its id, should be refused while it is still a download rather than becoming a
/// row that fails every time the app starts.
pub fn install(listed: &pr_plugin::repo::Listed) -> anyhow::Result<(Manifest, String)> {
    let bundle =
        get_text(&listed.bundle).with_context(|| format!("could not fetch {}", listed.bundle))?;

    if let Some(expected) = &listed.sha256 {
        pr_plugin::repo::verify(bundle.as_bytes(), expected)?;
    }

    let loaded = Loaded::start(&listed.id, bundle.clone(), Limits::default())?;
    pr_plugin::repo::agrees(listed, &loaded.manifest)?;
    Ok((loaded.manifest.clone(), bundle))
}

/// Which reader a source feeds, as the database spells it.
pub fn kind_text(kind: Kind) -> &'static str {
    match kind {
        Kind::Manga => "image",
        Kind::Novel => "text",
    }
}

#[cfg(test)]
mod tests;
