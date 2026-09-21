//! Interstitial challenges, handled by being a browser rather than by pretending.
//!
//! Invariant 13, made mechanical. When a request comes back as an interstitial, the
//! same URL is opened in a real webview -- the one this app already embeds -- and the
//! site's own JavaScript runs in it. Whatever cookie the site then issues goes into a
//! per-host jar, and later requests for that host carry it.
//!
//! Nothing here is faked, because nothing needs to be. Bot detection exists to separate
//! browsers from scripts, and at the moment the challenge runs, this *is* a browser
//! loading the real page. That is the whole argument, and it is why there is no
//! User-Agent written anywhere in this file, no TLS fingerprint work, and no
//! reimplementation of a challenge algorithm.
//!
//! **Two things are deliberately not done here, and both are load-bearing.**
//!
//! We never inject our IPC into a challenged page. Tauri's `initialization_script` would
//! make reading anything out of the page easy and would also hand a hostile site our
//! command surface, which is a far worse hole than the one it would close.
//!
//! We never send a User-Agent we did not earn. Where a site binds its clearance cookie
//! to the engine that solved the challenge, our follow-up request presents PanReader and
//! is refused -- and we report that plainly instead of borrowing the webview's identity
//! for a plain HTTP client. A reader who needs that site can open it themselves.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::Manager;

/// How long a non-interactive challenge gets before the window is shown.
///
/// Cloudflare's own managed challenge usually settles in two to five seconds. Past ten
/// it is waiting for a person, and hiding that from them is just a hang.
const QUIET: Duration = Duration::from_secs(10);

/// How long a visible challenge waits for a click before giving up.
const PATIENT: Duration = Duration::from_secs(120);

const POLL: Duration = Duration::from_millis(400);

/// Cookies we have been given, per host.
///
/// Per host because that is the boundary a cookie already has: one issued by a.test is
/// never sent to b.test, and clearing one site's jar must not log anyone out of
/// another. An extension never sees any of this -- it is on the host's side of the
/// line, which is what lets a challenged site work without handing a plugin something
/// it could exfiltrate.
#[derive(Default)]
pub struct Jars {
    hosts: parking_lot::Mutex<HashMap<String, Vec<(String, String)>>>,
}

impl Jars {
    /// The `Cookie` header value for a host, or nothing.
    pub fn header_for(&self, host: &str) -> Option<String> {
        let hosts = self.hosts.lock();
        let jar = hosts.get(host)?;
        if jar.is_empty() {
            return None;
        }
        Some(
            jar.iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    fn put(&self, host: &str, cookies: Vec<(String, String)>) {
        if cookies.is_empty() {
            return;
        }
        self.hosts.lock().insert(host.to_owned(), cookies);
    }

    /// What Settings shows: which hosts we hold something for, and how much.
    pub fn listed(&self) -> Vec<Jar> {
        let mut out: Vec<Jar> = self
            .hosts
            .lock()
            .iter()
            .map(|(host, cookies)| Jar {
                host: host.clone(),
                cookies: cookies.len() as i64,
            })
            .collect();
        out.sort_by(|a, b| a.host.cmp(&b.host));
        out
    }

    pub fn clear(&self, host: &str) {
        self.hosts.lock().remove(host);
    }

    pub fn clear_all(&self) {
        self.hosts.lock().clear();
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Jar {
    pub host: String,
    pub cookies: i64,
}

/// Whether a response is an interstitial rather than an answer.
///
/// Status alone is not enough: a 403 is also what a site says when a chapter is gone,
/// and treating that as a challenge would pop a window at someone for a dead link. So
/// the status has to be one an interstitial uses *and* the body has to look like one.
pub fn is_challenge(status: u16, body: &str) -> bool {
    if !matches!(status, 403 | 503) {
        return false;
    }
    // An interstitial is a small HTML page; a real 403 from an API is smaller still and
    // a real page of content is much larger.
    if body.len() > 200_000 {
        return false;
    }
    let haystack = body.to_ascii_lowercase();
    [
        "just a moment",
        "cf-browser-verification",
        "cf_chl_opt",
        "challenge-platform",
        "checking your browser",
        "enable javascript and cookies to continue",
        "ddos-guard",
        "__ddg",
        "attention required",
    ]
    .iter()
    .any(|marker| haystack.contains(marker))
}

/// Open the real page in the real engine and keep whatever cookie it issues.
///
/// Blocks, and must be called from a worker thread: reading cookies from the main
/// thread or a synchronous command deadlocks WebView2 on Windows. A source's own actor
/// thread is exactly the separate thread this wants.
pub fn solve(app: &tauri::AppHandle, jars: &Arc<Jars>, url: &str) -> anyhow::Result<bool> {
    let parsed = url::Url::parse(url)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("{url} has no host"))?
        .to_owned();

    // One window per host. Two chapters of the same series hitting the same
    // interstitial should wait on one challenge, not race to open two windows.
    let label = format!("challenge-{}", host.replace('.', "-"));
    if app.get_webview_window(&label).is_some() {
        anyhow::bail!("already solving a challenge for {host}");
    }

    tracing::info!(%host, "challenged; opening the page in a webview");
    let built = build_window(app, &label, parsed.clone())?;
    if !built {
        anyhow::bail!("could not open a window for {host}");
    }

    let started = Instant::now();
    let mut shown = false;
    let mut found: Vec<(String, String)> = Vec::new();

    loop {
        std::thread::sleep(POLL);

        let Some(window) = app.get_webview_window(&label) else {
            // The reader closed it. That is an answer.
            break;
        };

        match window.cookies_for_url(parsed.clone()) {
            Ok(cookies) if !cookies.is_empty() => {
                found = cookies
                    .into_iter()
                    .map(|c| (c.name().to_owned(), c.value().to_owned()))
                    .collect();
                break;
            }
            Ok(_) => {}
            Err(e) => tracing::debug!(%host, "could not read cookies yet: {e}"),
        }

        // Nothing yet, so this one wants a person. Invariant 13: an interactive
        // challenge gets a visible window and a click, and is never answered for them.
        if !shown && started.elapsed() > QUIET {
            tracing::info!(%host, "this challenge needs a click");
            let _ = window.show();
            let _ = window.set_focus();
            shown = true;
        }
        if started.elapsed() > if shown { PATIENT } else { QUIET } + PATIENT {
            break;
        }
    }

    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.close();
    }

    let got = !found.is_empty();
    jars.put(&host, found);
    if got {
        tracing::info!(%host, "challenge cleared");
    } else {
        tracing::info!(%host, "challenge not cleared");
    }
    Ok(got)
}

/// Windows are created on the main thread; this is called from a worker.
fn build_window(app: &tauri::AppHandle, label: &str, url: url::Url) -> anyhow::Result<bool> {
    let (done, wait) = std::sync::mpsc::sync_channel(1);
    let handle = app.clone();
    let label = label.to_owned();
    let title = format!("{} — one moment", url.host_str().unwrap_or("this site"));

    app.run_on_main_thread(move || {
        let built =
            tauri::WebviewWindowBuilder::new(&handle, &label, tauri::WebviewUrl::External(url))
                .title(title)
                .inner_size(760.0, 640.0)
                // Hidden until it turns out a person is needed. A challenge that settles on its
                // own should not flash a window at someone mid-page-turn.
                .visible(false)
                .build();
        let _ = done.send(match built {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!("could not build a challenge window: {e}");
                false
            }
        });
    })?;

    Ok(wait.recv_timeout(Duration::from_secs(10)).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interstitial_is_told_apart_from_an_ordinary_refusal() {
        let cloudflare = "<html><head><title>Just a moment...</title></head>\
                          <body><div id='cf-browser-verification'></div></body></html>";
        assert!(is_challenge(503, cloudflare));
        assert!(is_challenge(403, cloudflare));
        assert!(is_challenge(
            403,
            "<h1>Attention Required! | Cloudflare</h1>"
        ));
        assert!(is_challenge(403, "<div class='ddos-guard'>checking</div>"));

        // A dead link is a 403 too, and popping a window at someone for one would be a
        // worse bug than missing a challenge.
        assert!(!is_challenge(403, r#"{"error":"not found"}"#));
        assert!(!is_challenge(404, cloudflare), "wrong status");
        assert!(!is_challenge(200, cloudflare), "a page that mentions it");
        // A real chapter that happens to contain the words.
        let long = format!("{}{}", "x".repeat(200_001), cloudflare);
        assert!(!is_challenge(503, &long), "too big to be an interstitial");
    }

    #[test]
    fn a_jar_is_per_host_and_clearable() {
        let jars = Jars::default();
        assert!(jars.header_for("a.test").is_none());

        jars.put(
            "a.test",
            vec![
                ("cf_clearance".into(), "abc".into()),
                ("session".into(), "xyz".into()),
            ],
        );
        jars.put("b.test", vec![("other".into(), "1".into())]);

        assert_eq!(
            jars.header_for("a.test").unwrap(),
            "cf_clearance=abc; session=xyz"
        );
        // A cookie from one host is never sent to another; that is the boundary a
        // cookie already has and there is no reason to widen it.
        assert_eq!(jars.header_for("b.test").unwrap(), "other=1");

        assert_eq!(jars.listed().len(), 2);
        jars.clear("a.test");
        assert!(jars.header_for("a.test").is_none());
        assert!(jars.header_for("b.test").is_some(), "one host at a time");

        jars.clear_all();
        assert!(jars.listed().is_empty());
    }

    /// Putting nothing must not create an empty jar, or Settings shows a host we hold
    /// nothing for and clearing it does nothing visible.
    #[test]
    fn a_challenge_that_yielded_nothing_leaves_no_trace() {
        let jars = Jars::default();
        jars.put("a.test", Vec::new());
        assert!(jars.listed().is_empty());
    }
}
