//! Tauri commands and app state. See ROADMAP.md for where this sits.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod opds;
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

    /// A series cover: the first page of its first chapter, scaled, decoded once ever.
    ///
    /// Deliberately does not go through `Chapter`, which probes every page's header at
    /// open. A cover needs exactly one page, and a library screen asks for hundreds of
    /// them at once.
    ///
    /// The webview caches these for the session, so the file is about the cold start: a
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

    /// The live background palette for a chapter's cover.
    ///
    /// Reads the cached cover rather than the source page, so this costs one small
    /// JPEG decode and never reopens an archive. Signature 1 changes the background on
    /// every selection change, so it has to be cheap enough to run on hover.
    fn palette(&self, chapter_id: i64) -> anyhow::Result<Vec<String>> {
        let cover = self.cover(chapter_id, 320)?;
        Ok(pr_image::palette(&cover)?
            .iter()
            .map(|[r, g, b]| format!("{r},{g},{b}"))
            .collect())
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
fn catalogs(app: State<App>) -> Result<Vec<pr_db::CatalogRow>, String> {
    app.db.lock().catalogs().map_err(|e| format!("{e:#}"))
}

/// Add a catalog, verifying it is one before saving it.
///
/// A URL that is not a feed is a typo far more often than it is a server problem, and
/// finding that out at add time is much clearer than an empty browse later.
#[tauri::command]
async fn add_catalog(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let page = opds::browse(url.trim())
        .await
        .map_err(|e| format!("{e:#}"))?;
    let name = if page.feed.title.is_empty() {
        url.clone()
    } else {
        page.feed.title.clone()
    };
    app.state::<App>()
        .db
        .lock()
        .add_catalog(url.trim(), &name)
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn remove_catalog(app: State<App>, id: i64) -> Result<(), String> {
    app.db
        .lock()
        .remove_catalog(id)
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn opds_browse(url: String) -> Result<opds::Page, String> {
    opds::browse(&url).await.map_err(|e| format!("{e:#}"))
}

/// Download a publication into a library root, then fold it into the library.
///
/// The rescan is the whole integration: a downloaded file is an ordinary local chapter
/// the moment it lands, so nothing else in the app needs to know it came from a server.
/// It goes in the root flat rather than in a subfolder, because `scan_root` reads a
/// loose archive as a series of one and a subfolder would instead read as one series
/// called "downloads" with every unrelated book as its chapters.
///
/// The root is the caller's choice when there is more than one; the UI asks, because
/// silently picking one is the kind of thing someone only notices after downloading
/// forty books into the wrong folder.
#[tauri::command]
async fn opds_download(
    app: tauri::AppHandle,
    href: String,
    title: String,
    mime: String,
    root: Option<String>,
) -> Result<String, String> {
    let roots = app
        .state::<App>()
        .db
        .lock()
        .roots()
        .map_err(|e| format!("{e:#}"))?;
    let root = match root {
        // Only a root the reader already added. A path off the wire would let a feed
        // choose where we write.
        Some(chosen) => roots
            .into_iter()
            .find(|r| r == Path::new(&chosen))
            .ok_or("that is not one of your library folders")?,
        None => roots
            .into_iter()
            .next()
            .ok_or("add a library folder first, so there is somewhere to download to")?,
    };

    let path = opds::download(&href, &title, &mime, &root)
        .await
        .map_err(|e| format!("{e:#}"))?;

    let handle = app.clone();
    std::thread::spawn(move || {
        if let Err(e) = handle.state::<App>().scan() {
            tracing::warn!("scan after download failed: {e:#}");
        }
    });
    Ok(path.display().to_string())
}

/// Eight colours for the live background, as "r,g,b" strings ready for rgb().
///
/// Small and fixed, so a command rather than a route.
#[tauri::command]
fn palette(app: State<App>, chapter_id: i64) -> Result<Vec<String>, String> {
    app.palette(chapter_id).map_err(|e| format!("{e:#}"))
}

/// The shelf's resume row. Small and fixed, so it is a command rather than a route.
#[tauri::command]
fn continue_reading(app: State<App>) -> Result<Vec<pr_db::ResumeRow>, String> {
    app.db
        .lock()
        .continue_reading(12)
        .map_err(|e| format!("{e:#}"))
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
    frac: f64,
    completed: bool,
) -> Result<(), String> {
    let db = app.db.lock();
    db.save_position(chapter_id, page, frac, completed)
        .map_err(|e| format!("{e:#}"))?;
    // Same call site on purpose: a turn is exactly when both facts change, and a second
    // IPC round trip per page would be a round trip per page.
    db.record_read(chapter_id, page)
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn history(app: State<App>, limit: i64) -> Result<Vec<pr_db::HistoryRow>, String> {
    app.db
        .lock()
        .history(limit.clamp(1, 500))
        .map_err(|e| format!("{e:#}"))
}

/// Drop one session, or the whole log. Stats derive from it, so this resets those too.
#[tauri::command]
fn forget(app: State<App>, id: Option<i64>) -> Result<(), String> {
    app.db.lock().forget(id).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn reading_stats(app: State<App>) -> Result<pr_db::ReadingStats, String> {
    app.db.lock().reading_stats().map_err(|e| format!("{e:#}"))
}

/// Returns whether the spot is bookmarked now, which is what the button renders from.
#[tauri::command]
fn toggle_bookmark(
    app: State<App>,
    chapter_id: i64,
    page: i64,
    frac: f64,
    paragraph: Option<i64>,
    char_offset: Option<i64>,
) -> Result<bool, String> {
    app.db
        .lock()
        .toggle_bookmark(chapter_id, page, frac, paragraph, char_offset)
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn bookmarks(app: State<App>, chapter_id: Option<i64>) -> Result<Vec<pr_db::BookmarkRow>, String> {
    app.db
        .lock()
        .bookmarks(chapter_id)
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn remove_bookmark(app: State<App>, id: i64) -> Result<(), String> {
    app.db
        .lock()
        .remove_bookmark(id)
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn set_bookmark_note(app: State<App>, id: i64, note: String) -> Result<(), String> {
    app.db
        .lock()
        .set_bookmark_note(id, &note)
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
            continue_reading,
            palette,
            catalogs,
            add_catalog,
            remove_catalog,
            opds_browse,
            opds_download,
            categories,
            create_category,
            delete_category,
            set_category_mode,
            set_series_category,
            categories_of,
            set_series_mode,
            history,
            forget,
            reading_stats,
            toggle_bookmark,
            bookmarks,
            remove_bookmark,
            set_bookmark_note
        ])
        .run(tauri::generate_context!())
        .expect("tauri failed to start");
}

#[cfg(test)]
mod cover_tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A library with one folder chapter holding one real JPEG.
    fn library(dir: &Path) -> App {
        let chapter = dir.join("Series").join("Chapter 1");
        std::fs::create_dir_all(&chapter).unwrap();
        std::fs::write(
            chapter.join("p1.jpg"),
            pr_image::flat_jpeg(600, 900, [180, 40, 40]).unwrap(),
        )
        .unwrap();

        // App::open reads PANREADER_DB, so the whole thing lands in the temp dir and
        // the cover cache goes beside it.
        let db_path = dir.join("library.db");
        unsafe { std::env::set_var("PANREADER_DB", &db_path) };
        let app = App::open().unwrap();
        app.db.lock().add_root(dir).unwrap();
        app.scan().unwrap();
        app
    }

    /// The point of the cache is the cold start: a shelf must not reopen every archive
    /// in the library to paint. Proving it by consequence -- the source is deleted and
    /// the cover still comes back -- is stronger than counting decodes.
    #[test]
    fn a_cover_is_decoded_once_and_served_from_disk_after() {
        let dir = tmp("pr_cover_cache");
        let app = library(&dir);

        let chapter_id = app.db.lock().search("", None).unwrap()[0]
            .cover_chapter_id
            .unwrap();

        let first = app.cover(chapter_id, 320).unwrap();
        assert!(!first.is_empty());
        // At or above the asked-for width, never below: decode_scaled takes the nearest
        // DCT scale and only resamples past a 2x overshoot, so 600 stands rather than
        // paying for a resize the shelf will not notice.
        assert!(
            pr_image::probe(&first).unwrap().0 >= 320,
            "a cover must never come back narrower than the shelf asked for"
        );

        let cached = dir.join("covers").join(format!("{chapter_id}-320.jpg"));
        assert!(
            cached.exists(),
            "the decode is kept for the next cold start"
        );

        // Nothing left to decode from. A second call can only succeed off the cache.
        std::fs::remove_dir_all(dir.join("Series")).unwrap();
        let second = app.cover(chapter_id, 320).unwrap();
        assert_eq!(first, second);

        // A different width is a different cover, not a stale hit.
        assert!(app.cover(chapter_id, 640).is_err());

        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
