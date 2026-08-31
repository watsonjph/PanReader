//! Tauri commands and app state. See ROADMAP.md for where this sits.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod tiles;

use anyhow::Context;
use parking_lot::Mutex;
use pr_archive::PageSource;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::http::{Request, Response, StatusCode, header};
use tauri::{Manager, State};
use tiles::{Chapter, Layout, StatsSnapshot, TileKey};

struct App {
    /// Keyed by chapter id, so two chapters of one series are distinct entries and
    /// reopening one is free.
    chapters: Mutex<HashMap<i64, Arc<Chapter>>>,
    scanning: std::sync::atomic::AtomicBool,
    db: Mutex<pr_db::Db>,
    /// Cached so the tile path never touches SQLite. Settings change rarely and are
    /// read on every chapter open.
    settings: Mutex<pr_core::Settings>,
}

impl App {
    fn open() -> anyhow::Result<Self> {
        let path = pr_db::default_path()?;
        let db = pr_db::Db::open(&path)?;
        let settings = db.settings()?;
        tracing::info!(db = %path.display(), ?settings, "opened library");
        Ok(Self {
            chapters: Mutex::new(HashMap::new()),
            scanning: std::sync::atomic::AtomicBool::new(false),
            db: Mutex::new(db),
            settings: Mutex::new(settings),
        })
    }
}

impl App {
    /// Open a chapter by id, or hand back the one already open.
    fn chapter(&self, chapter_id: i64) -> anyhow::Result<Arc<Chapter>> {
        if let Some(open) = self.chapters.lock().get(&chapter_id) {
            return Ok(open.clone());
        }

        let row = self
            .db
            .lock()
            .chapter(chapter_id)?
            .with_context(|| format!("chapter {chapter_id} is not in the library"))?;
        let path = PathBuf::from(&row.path);
        let src = PageSource::open(&path)
            .with_context(|| format!("Could not open {}", path.display()))?;

        let default_mode = self.settings.lock().default_reading_mode;
        let chapter = Arc::new(Chapter::open(&row.title, src, default_mode)?);
        self.chapters.lock().insert(chapter_id, chapter.clone());
        Ok(chapter)
    }

    /// Walk every root and fold the result in.
    ///
    /// Runs on the caller's thread; the command that triggers it spawns, because a scan
    /// of a real library takes seconds and invariant 7 says a command returns at once.
    fn scan(&self) -> anyhow::Result<pr_db::ScanSummary> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.scanning.swap(true, SeqCst) {
            anyhow::bail!("a scan is already running");
        }
        let result = (|| {
            let roots = self.db.lock().roots()?;
            let mut total = pr_db::ScanSummary::default();
            for root in roots {
                let found = pr_archive::scan::scan_root(&root);
                let summary = self.db.lock().sync(&found)?;
                total.series += summary.series;
                total.chapters_added += summary.chapters_added;
                total.chapters_kept += summary.chapters_kept;
            }
            tracing::info!(?total, "scan complete");
            Ok(total)
        })();
        self.scanning.store(false, SeqCst);
        result
    }
}

#[tauri::command]
fn library(app: State<App>) -> Result<Vec<pr_db::SeriesRow>, String> {
    app.db.lock().library().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn chapters(app: State<App>, series_id: i64) -> Result<Vec<pr_db::ChapterRow>, String> {
    app.db
        .lock()
        .chapters(series_id)
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn roots(app: State<App>) -> Result<Vec<String>, String> {
    app.db
        .lock()
        .roots()
        .map(|rs| {
            rs.iter()
                .map(|r| r.to_string_lossy().into_owned())
                .collect()
        })
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn add_root(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let dir = PathBuf::from(&path);
    if !dir.is_dir() {
        return Err(format!("{path} is not a folder"));
    }
    app.state::<App>()
        .db
        .lock()
        .add_root(&dir)
        .map_err(|e| format!("{e:#}"))?;
    rescan(app)
}

#[tauri::command]
fn remove_root(app: State<App>, path: String) -> Result<(), String> {
    app.db
        .lock()
        .remove_root(Path::new(&path))
        .map_err(|e| format!("{e:#}"))
}

/// Returns immediately; the scan runs behind it. The frontend watches `scanning`.
#[tauri::command]
fn rescan(app: tauri::AppHandle) -> Result<(), String> {
    std::thread::spawn(move || {
        if let Err(e) = app.state::<App>().scan() {
            tracing::warn!("scan failed: {e:#}");
        }
    });
    Ok(())
}

#[tauri::command]
fn scanning(app: State<App>) -> bool {
    app.scanning.load(std::sync::atomic::Ordering::SeqCst)
}

/// Written on every page turn, which is why it is a one-row upsert rather than part of
/// the settings blob.
#[tauri::command]
fn save_position(
    app: State<App>,
    chapter_id: i64,
    page: i64,
    completed: bool,
) -> Result<(), String> {
    app.db
        .lock()
        .save_position(chapter_id, page, completed)
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn open_chapter(app: State<App>, chapter_id: i64, display_w: u32) -> Result<Layout, String> {
    let chapter = app.chapter(chapter_id).map_err(|e| format!("{e:#}"))?;
    let layout = chapter.layout(display_w);
    // Start filling the chapter behind the reader straight away. The first page is
    // already on its way over pan:// by the time this returns.
    chapter.warm(0, display_w);
    Ok(layout)
}

/// Re-aim the background fill at where the reader actually is.
#[tauri::command]
fn warm(app: State<App>, chapter_id: i64, page: usize, display_w: u32) {
    if let Some(chapter) = app.chapters.lock().get(&chapter_id) {
        chapter.warm(page, display_w);
    }
}

#[tauri::command]
fn settings(app: State<App>) -> pr_core::Settings {
    app.settings.lock().clone()
}

/// Persist and adopt. Written whole, because settings change rarely and a blob rewrite
/// costs nothing at this rate -- unlike reading position, which gets its own table.
#[tauri::command]
fn save_settings(app: State<App>, settings: pr_core::Settings) -> Result<(), String> {
    app.db
        .lock()
        .save_settings(&settings)
        .map_err(|e| format!("{e:#}"))?;
    *app.settings.lock() = settings;
    Ok(())
}

#[tauri::command]
fn stats(app: State<App>, chapter_id: i64) -> StatsSnapshot {
    // Deliberately does not open the chapter. It used to, which meant the HUD's own
    // 250ms poll paid the header probe before you ever pressed a key, and first-paint
    // then measured a warm open while looking like a cold one.
    app.chapters
        .lock()
        .get(&chapter_id)
        .map(|c| c.snapshot())
        .unwrap_or_default()
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

/// `/t/{chapter_id}/{page}/{tile}/{display_w}` -> JPEG bytes.
///
/// Hard invariant 1: image bytes never cross Tauri IPC. They come through here.
fn serve(app: &App, req: &Request<Vec<u8>>) -> anyhow::Result<tiles::Served> {
    let path = req.uri().path();
    let mut seg = path.trim_start_matches('/').split('/');
    anyhow::ensure!(seg.next() == Some("t"), "unknown route {path}");
    let mut num =
        || -> anyhow::Result<i64> { Ok(seg.next().context("missing path segment")?.parse()?) };
    let chapter_id = num()?;
    let key = TileKey {
        page: num()? as usize,
        tile: num()? as u32,
        w: num()? as u32,
    };
    app.chapter(chapter_id)?.tile(key)
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

    let app = match App::open() {
        Ok(app) => app,
        // A library we cannot open is worth failing loudly for: every setting and every
        // reading position lives there, and starting without it would silently discard
        // both.
        Err(e) => {
            tracing::error!("could not open the library: {e:#}");
            std::process::exit(1);
        }
    };

    tauri::Builder::default()
        .manage(app)
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
                    Ok(served) => Response::builder()
                        .header(header::CONTENT_TYPE, served.mime)
                        // Tiles are content-addressed by their URL, so they never change.
                        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                        .body(served.data.as_ref().clone()),
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
        .invoke_handler(tauri::generate_handler![
            open_chapter,
            warm,
            stats,
            tile_base,
            settings,
            save_settings,
            library,
            chapters,
            roots,
            add_root,
            remove_root,
            rescan,
            scanning,
            save_position
        ])
        .run(tauri::generate_context!())
        .expect("tauri failed to start");
}
