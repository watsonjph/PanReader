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
    /// Decoded covers, so a cold start paints the shelf without opening every archive
    /// in the library.
    covers: PathBuf,
}

impl App {
    fn open() -> anyhow::Result<Self> {
        let path = pr_db::default_path()?;
        let db = pr_db::Db::open(&path)?;
        let settings = db.settings()?;
        tracing::info!(db = %path.display(), ?settings, "opened library");
        // Derived data, so it belongs beside the database rather than in the library
        // (hard invariant 5). Losing it costs one re-decode.
        let covers = path.with_file_name("covers");
        if let Err(e) = std::fs::create_dir_all(&covers) {
            tracing::warn!(dir = %covers.display(), "no cover cache: {e}");
        }
        Ok(Self {
            chapters: Mutex::new(HashMap::new()),
            scanning: std::sync::atomic::AtomicBool::new(false),
            db: Mutex::new(db),
            settings: Mutex::new(settings),
            covers,
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

        // Precedence: a series override beats detection, a category default only
        // applies where detection had nothing to go on, and the global setting is
        // the last resort. `pr_core::detect` enforces the first half of that.
        let (series_override, category_mode) = self.db.lock().modes_for_chapter(chapter_id)?;
        let fallback = category_mode.unwrap_or(self.settings.lock().default_reading_mode);
        let chapter = Arc::new(Chapter::open(&row.title, src, series_override, fallback)?);
        self.chapters.lock().insert(chapter_id, chapter.clone());
        Ok(chapter)
    }

    /// A series cover: the first page of its first chapter, scaled.
    ///
    /// Deliberately does not go through `Chapter`, which probes every page's header at
    /// open. A cover needs exactly one page, and a library screen asks for hundreds of
    /// them at once. Not cached in memory either: the response is immutable and the
    /// webview keeps it, so a second look costs nothing.
    /// A series cover, decoded once ever.
    ///
    /// The webview caches these for the session, so this is about the cold start: a
    /// five hundred series shelf otherwise opens five hundred archives before it can
    /// paint. A chapter row is matched by content identity, so an id implies fixed
    /// bytes and the file never goes stale — changed content lands on a new row.
    ///
    /// ponytail: no eviction. One cover per series at ~15 KB is single-digit megabytes
    /// for a large library. Add an LRU when a real library makes that untrue.
    fn cover(&self, chapter_id: i64, width: u32) -> anyhow::Result<Vec<u8>> {
        // chapter_id and width are already parsed as numbers, so this cannot escape
        // the directory.
        let cached = self.covers.join(format!("{chapter_id}-{width}.jpg"));
        if let Ok(bytes) = std::fs::read(&cached) {
            return Ok(bytes);
        }

        let row = self
            .db
            .lock()
            .chapter(chapter_id)?
            .with_context(|| format!("chapter {chapter_id} is not in the library"))?;
        let src = PageSource::open(Path::new(&row.path))?;
        let img = pr_image::decode_scaled(&src.read(0)?, width)?;
        let bytes = pr_image::encode_jpeg(&img, 78)?;

        // Best effort. A cover we cannot write is a slow cover, not a failure.
        if let Err(e) = std::fs::write(&cached, &bytes) {
            tracing::debug!(path = %cached.display(), "cover not cached: {e}");
        }
        Ok(bytes)
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
            // What the last scan saw. A rescan of an unchanged library then costs a
            // directory walk and nothing else.
            let known = self.db.lock().known()?;
            let mut total = pr_db::ScanSummary::default();
            for root in roots {
                let found = pr_archive::scan::scan_root(&root, &known);
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
fn search(
    app: State<App>,
    query: String,
    category: Option<i64>,
) -> Result<Vec<pr_db::SeriesRow>, String> {
    app.db
        .lock()
        .search(query.trim(), category)
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn categories(app: State<App>) -> Result<Vec<pr_db::CategoryRow>, String> {
    app.db.lock().categories().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn create_category(app: State<App>, name: String) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a category needs a name".into());
    }
    app.db
        .lock()
        .create_category(name)
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn delete_category(app: State<App>, id: i64) -> Result<(), String> {
    app.db
        .lock()
        .delete_category(id)
        .map_err(|e| format!("{e:#}"))
}

/// Changing a category's mode changes how its series open, so anything already open is
/// dropped: the mode is resolved when a chapter is opened, not on every page.
#[tauri::command]
fn set_category_mode(
    app: State<App>,
    id: i64,
    mode: Option<pr_core::ReadingMode>,
) -> Result<(), String> {
    app.db
        .lock()
        .set_category_mode(id, mode)
        .map_err(|e| format!("{e:#}"))?;
    app.chapters.lock().clear();
    Ok(())
}

#[tauri::command]
fn set_series_category(
    app: State<App>,
    series_id: i64,
    category_id: i64,
    member: bool,
) -> Result<(), String> {
    app.db
        .lock()
        .set_series_category(series_id, category_id, member)
        .map_err(|e| format!("{e:#}"))?;
    app.chapters.lock().clear();
    Ok(())
}

#[tauri::command]
fn categories_of(app: State<App>, series_id: i64) -> Result<Vec<i64>, String> {
    app.db
        .lock()
        .categories_of(series_id)
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn set_series_mode(
    app: State<App>,
    series_id: i64,
    mode: Option<pr_core::ReadingMode>,
) -> Result<(), String> {
    app.db
        .lock()
        .set_series_mode(series_id, mode)
        .map_err(|e| format!("{e:#}"))?;
    app.chapters.lock().clear();
    Ok(())
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

/// `/t/{chapter_id}/{page}/{tile}/{display_w}` -> tile bytes.
/// `/c/{chapter_id}/{width}` -> a cover.
///
/// Hard invariant 1: image bytes never cross Tauri IPC. They come through here.
fn serve(app: &App, req: &Request<Vec<u8>>) -> anyhow::Result<tiles::Served> {
    let path = req.uri().path();
    let mut seg = path.trim_start_matches('/').split('/');
    let route = seg.next().unwrap_or_default();
    let mut num =
        || -> anyhow::Result<i64> { Ok(seg.next().context("missing path segment")?.parse()?) };
    let chapter_id = num()?;

    if route == "c" {
        return Ok(tiles::Served {
            data: Arc::new(app.cover(chapter_id, num()? as u32)?),
            mime: "image/jpeg",
        });
    }
    anyhow::ensure!(route == "t", "unknown route {path}");

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
        .plugin(tauri_plugin_dialog::init())
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
            chapters,
            roots,
            add_root,
            remove_root,
            rescan,
            scanning,
            save_position,
            search,
            categories,
            create_category,
            delete_category,
            set_category_mode,
            set_series_category,
            categories_of,
            set_series_mode
        ])
        .run(tauri::generate_context!())
        .expect("tauri failed to start");
}
