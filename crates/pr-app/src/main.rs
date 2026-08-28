//! Phase 0 spike. No library, no settings, no design: this exists only to answer
//! "can a 60,000px strip scroll smoothly through a webview". See ROADMAP.md.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod tiles;

use anyhow::Context;
use parking_lot::Mutex;
use pr_archive::PageSource;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::http::{header, Request, Response, StatusCode};
use tauri::{Manager, State};
use tiles::{Chapter, Layout, StatsSnapshot, TileKey};

#[derive(Default)]
struct App {
    chapters: Mutex<HashMap<String, Arc<Chapter>>>,
}

/// Hardcoded fixtures, per Phase 0. Override with PANREADER_CBZ / PANREADER_STRIP.
fn fixture(kind: &str) -> PathBuf {
    let (var, default) = match kind {
        "strip" => ("PANREADER_STRIP", "fixtures/strip"),
        _ => ("PANREADER_CBZ", "fixtures/spike.cbz"),
    };
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| default.into())
}

impl App {
    fn chapter(&self, kind: &str) -> anyhow::Result<Arc<Chapter>> {
        if let Some(c) = self.chapters.lock().get(kind) {
            return Ok(c.clone());
        }
        let path = fixture(kind);
        let src = PageSource::open(&path)
            .with_context(|| format!("opening {kind} fixture at {}", path.display()))?;
        let chapter = Arc::new(Chapter::open(kind, src)?);
        self.chapters
            .lock()
            .insert(kind.to_owned(), chapter.clone());
        Ok(chapter)
    }
}

#[tauri::command]
fn open_chapter(app: State<'_, App>, kind: String, display_w: u32) -> Result<Layout, String> {
    app.chapter(&kind)
        .map(|c| c.layout(display_w))
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn stats(app: State<'_, App>, kind: String) -> StatsSnapshot {
    app.chapter(&kind).map(|c| c.snapshot()).unwrap_or_default()
}

/// Custom-protocol origin differs per platform, so the frontend asks rather than guesses.
#[tauri::command]
fn tile_base() -> &'static str {
    if cfg!(any(windows, target_os = "android")) {
        "http://pan.localhost"
    } else {
        "pan://localhost"
    }
}

/// `/t/{kind}/{page}/{tile}/{display_w}` -> JPEG bytes.
///
/// Hard invariant 1: image bytes never cross Tauri IPC. They come through here.
fn serve(app: &App, req: &Request<Vec<u8>>) -> anyhow::Result<Vec<u8>> {
    let path = req.uri().path();
    let mut seg = path.trim_start_matches('/').split('/');
    anyhow::ensure!(seg.next() == Some("t"), "unknown route {path}");
    let kind = seg.next().context("missing kind")?;
    let mut num =
        || -> anyhow::Result<u32> { Ok(seg.next().context("missing path segment")?.parse()?) };
    let key = TileKey {
        page: num()? as usize,
        tile: num()?,
        w: num()?,
    };
    Ok(app.chapter(kind)?.tile(key)?.as_ref().clone())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("pr_app=info,pr_image=info")
        .init();

    // One global pool, one core left for the UI thread. CLAUDE.md, Parallelism.
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1));
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build_global()
        .expect("rayon global pool is built exactly once, here");

    tauri::Builder::default()
        .manage(App::default())
        .register_asynchronous_uri_scheme_protocol("pan", |ctx, req, responder| {
            let app = ctx.app_handle().clone();
            // Invariant 7: no blocking work on the webview's thread.
            //
            // ponytail: a plain thread per request, not rayon. Serving blocks on the
            // page-fill lock, and a rayon worker that blocks can be the same worker the
            // fill's own par_iter is waiting on, which deadlocks. Tile hits return in
            // microseconds so the thread count stays low; put a bounded pool here if a
            // flick ever spawns enough threads to show up in the HUD.
            std::thread::spawn(move || {
                let res = match serve(app.state::<App>().inner(), &req) {
                    Ok(bytes) => Response::builder()
                        .header(header::CONTENT_TYPE, "image/jpeg")
                        // Tiles are content-addressed by their URL, so they never change.
                        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                        .body(bytes),
                    Err(e) => {
                        tracing::warn!(uri = %req.uri(), "tile failed: {e:#}");
                        Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(format!("{e:#}").into_bytes())
                    }
                };
                if let Ok(res) = res {
                    responder.respond(res);
                }
            });
        })
        .invoke_handler(tauri::generate_handler![open_chapter, stats, tile_base])
        .run(tauri::generate_context!())
        .expect("tauri failed to start");
}
