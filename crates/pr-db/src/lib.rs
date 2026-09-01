//! SQLite library, settings and reading positions.
//!
//! Synchronous on purpose. `CLAUDE.md` keeps tokio in `pr-app`, `pr-server` and
//! `pr-engine`; callers here use `spawn_blocking`. SQLite is fast enough that an async
//! driver would buy nothing but a runtime dependency and compile-time schema plumbing.

pub use rusqlite;

use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("settings are not valid json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "migration {version} ({name}) has changed since it was applied. \
         Migrations are append-only: restore the original file and add a new one."
    )]
    MigrationChanged { version: i64, name: &'static str },
    #[error("no home directory to put the database in")]
    NoDataDir,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Applied in order, never edited once released. Adding one is a new entry here plus a
/// new file; the runner does the rest.
const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("../migrations/0001_init.sql")),
    (
        "0002_chapter_identity_index",
        include_str!("../migrations/0002_chapter_identity_index.sql"),
    ),
    (
        "0003_chapter_path",
        include_str!("../migrations/0003_chapter_path.sql"),
    ),
    (
        "0004_chapter_stamp",
        include_str!("../migrations/0004_chapter_stamp.sql"),
    ),
    (
        "0005_position_millis",
        include_str!("../migrations/0005_position_millis.sql"),
    ),
    (
        "0006_opds_catalogs",
        include_str!("../migrations/0006_opds_catalogs.sql"),
    ),
    (
        "0007_position_offset",
        include_str!("../migrations/0007_position_offset.sql"),
    ),
    (
        "0008_history_bookmarks",
        include_str!("../migrations/0008_history_bookmarks.sql"),
    ),
    (
        "0009_chapter_locator",
        include_str!("../migrations/0009_chapter_locator.sql"),
    ),
];

/// Cheap, stable, and only ever compared against itself, so a real hash would be
/// ceremony. This exists to catch an edited migration, not an adversary.
fn checksum(sql: &str) -> String {
    // FNV-1a over the bytes, ignoring line endings so a checkout with CRLF does not
    // look like tampering.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in sql.bytes().filter(|&b| b != b'\r') {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open, creating and migrating as needed.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::from_conn(conn)
    }

    /// In-memory, for tests.
    pub fn open_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        // WAL so a background scan writing does not block the reader reading. NORMAL
        // synchronous is the usual companion: a crash can lose the last commit, which
        // for a reading position is not worth an fsync per page turn.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;

        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Apply what has not run, and verify what has.
    ///
    /// The verification is the point. Editing a migration that has already run leaves
    /// two installs with different schemas and the same version number, which surfaces
    /// later as an impossible bug. Failing at startup is far cheaper.
    fn migrate(&mut self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 version    INTEGER PRIMARY KEY,
                 name       TEXT NOT NULL,
                 checksum   TEXT NOT NULL,
                 applied_at INTEGER NOT NULL DEFAULT (unixepoch())
             )",
        )?;

        for (index, (name, sql)) in MIGRATIONS.iter().enumerate() {
            let version = index as i64 + 1;
            let sum = checksum(sql);
            let applied: Option<String> = self
                .conn
                .query_row(
                    "SELECT checksum FROM schema_migrations WHERE version = ?1",
                    params![version],
                    |row| row.get(0),
                )
                .optional()?;

            match applied {
                Some(recorded) if recorded == sum => continue,
                Some(_) => return Err(Error::MigrationChanged { version, name }),
                None => {
                    let tx = self.conn.transaction()?;
                    tx.execute_batch(sql)?;
                    tx.execute(
                        "INSERT INTO schema_migrations (version, name, checksum)
                         VALUES (?1, ?2, ?3)",
                        params![version, name, sum],
                    )?;
                    tx.commit()?;
                    tracing::info!(version, name, "applied migration");
                }
            }
        }
        Ok(())
    }

    /// Settings, or the defaults when nothing has been saved yet.
    ///
    /// A blob that fails to parse is replaced by defaults rather than failing the
    /// launch. Losing a preference is an annoyance; refusing to start over one is not a
    /// trade anybody would choose.
    pub fn settings(&self) -> Result<pr_core::Settings> {
        let raw: Option<String> = self
            .conn
            .query_row("SELECT json FROM app_config WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?;

        Ok(raw
            .and_then(|json| match serde_json::from_str(&json) {
                Ok(settings) => Some(settings),
                Err(e) => {
                    tracing::warn!("settings were unreadable, using defaults: {e}");
                    None
                }
            })
            .unwrap_or_default())
    }

    /// A transaction over the whole schema, for `pr-sync`.
    ///
    /// Backup is schema-coupled by definition -- exporting what rows mean is still
    /// reading rows -- and re-stating every table's columns as accessors here would put
    /// the schema in two crates. One narrow handle instead.
    pub fn transaction(&mut self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self.conn.transaction()?)
    }

    pub fn save_settings(&self, settings: &pr_core::Settings) -> Result<()> {
        self.conn.execute(
            "INSERT INTO app_config (id, json) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET json = excluded.json",
            params![serde_json::to_string(settings)?],
        )?;
        Ok(())
    }
}

/// A series as the library lists it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SeriesRow {
    pub id: i64,
    pub title: String,
    pub path: String,
    pub chapter_count: i64,
    /// Chapters not finished. A series with none is read to the end.
    pub unread: i64,
    /// First chapter in reading order. Its first page is the cover.
    pub cover_chapter_id: Option<i64>,
    /// Unix seconds. The home screen's "Recently added" row sorts on it, which is why
    /// it is here rather than being a second query.
    pub added_at: i64,
    /// `image` or `text`. Which reader a click opens, and the one thing the shell has
    /// to know about a series before it opens anything.
    pub kind: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CatalogRow {
    pub id: i64,
    pub url: String,
    pub name: String,
}

/// Somewhere the reader left off, for the shelf to offer back.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResumeRow {
    pub chapter_id: i64,
    pub series_id: i64,
    pub series_title: String,
    pub chapter_title: String,
    pub number: Option<f64>,
    pub page: i64,
    pub page_frac: f64,
    pub page_count: i64,
    /// Which reader to open. Every row the shell can click through to carries it.
    pub kind: String,
}

/// One reading session, joined to enough of the library to render a row.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HistoryRow {
    pub id: i64,
    pub chapter_id: i64,
    pub series_id: i64,
    pub series_title: String,
    pub chapter_title: String,
    pub number: Option<f64>,
    pub cover_chapter_id: Option<i64>,
    pub started_at: i64,
    pub ended_at: i64,
    pub pages: i64,
    pub last_page: i64,
    pub kind: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BookmarkRow {
    pub id: i64,
    pub chapter_id: i64,
    pub series_id: i64,
    pub series_title: String,
    pub chapter_title: String,
    pub page: i64,
    pub page_frac: f64,
    pub paragraph: Option<i64>,
    pub char_offset: Option<i64>,
    pub note: String,
    pub created_at: i64,
    pub kind: String,
}

/// Derived from history every time it is asked for, never stored. A stored counter is a
/// second source of truth, and it starts lying the moment anything goes wrong.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct ReadingStats {
    pub chapters: i64,
    pub pages: i64,
    /// Summed session length. A session with a single event is zero, honestly.
    pub minutes: i64,
    pub days: i64,
    pub streak: i64,
    pub best_streak: i64,
}

/// A chapter as the library lists it, with wherever the reader got to.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChapterRow {
    pub id: i64,
    pub title: String,
    pub number: Option<f64>,
    pub page_count: i64,
    /// Where to read it from. Identity matches; path opens.
    pub path: String,
    /// Where inside the container, for a format that holds more than one chapter.
    pub locator: String,
    pub kind: String,
    pub page: i64,
    /// How far into that page, as a fraction of its height. Resolution-independent on
    /// purpose: a pixel offset stops meaning anything when the decode width changes.
    pub page_frac: f64,
    pub completed: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CategoryRow {
    pub id: i64,
    pub name: String,
    /// Applies to every series in it that has no override of its own.
    pub reading_mode: Option<pr_core::ReadingMode>,
    pub series_count: i64,
}

/// The reading mode as the schema stores it. Public because `pr-sync` writes the same
/// column and a second spelling of these three strings is a second thing to get wrong.
pub fn mode_text(mode: Option<pr_core::ReadingMode>) -> Option<String> {
    mode.map(|m| match m {
        pr_core::ReadingMode::Rtl => "rtl".to_owned(),
        pr_core::ReadingMode::Ltr => "ltr".to_owned(),
        pr_core::ReadingMode::Webtoon => "webtoon".to_owned(),
    })
}

/// An unrecognised mode reads as no override rather than as an error. The column is
/// nullable for exactly this reason: "detect it" is always a valid answer.
pub fn mode_from(text: Option<String>) -> Option<pr_core::ReadingMode> {
    match text.as_deref() {
        Some("rtl") => Some(pr_core::ReadingMode::Rtl),
        Some("ltr") => Some(pr_core::ReadingMode::Ltr),
        Some("webtoon") => Some(pr_core::ReadingMode::Webtoon),
        _ => None,
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ScanSummary {
    pub series: usize,
    pub chapters_added: usize,
    pub chapters_kept: usize,
}

impl Db {
    pub fn add_root(&self, path: &Path) -> Result<()> {
        self.conn.execute(
            "INSERT INTO library_roots (path) VALUES (?1) ON CONFLICT(path) DO NOTHING",
            params![path.to_string_lossy()],
        )?;
        Ok(())
    }

    pub fn roots(&self) -> Result<Vec<PathBuf>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM library_roots ORDER BY added_at")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<String>>>()?
            .into_iter()
            .map(PathBuf::from)
            .collect())
    }

    pub fn remove_root(&self, path: &Path) -> Result<()> {
        self.conn.execute(
            "DELETE FROM library_roots WHERE path = ?1",
            params![path.to_string_lossy()],
        )?;
        Ok(())
    }

    /// Fold a scan into the library.
    ///
    /// A chapter is matched by content identity, never by path, and a match keeps the
    /// existing row and therefore the existing reading position. That is the entire
    /// reason identity is content-derived: renaming a file, or moving it into a renamed
    /// series folder, must not cost the reader their place.
    ///
    /// Nothing is deleted here. A chapter that has vanished from disk stays until
    /// something explicitly prunes it, because an unplugged drive must not erase a
    /// year of progress.
    pub fn sync(&mut self, scanned: &[pr_archive::scan::ScannedSeries]) -> Result<ScanSummary> {
        let mut summary = ScanSummary::default();
        let tx = self.conn.transaction()?;

        for series in scanned {
            let path = series.path.to_string_lossy().to_string();
            tx.execute(
                "INSERT INTO series (source, source_id, title, author, kind)
                 VALUES ('local', ?1, ?2, ?3, ?4)
                 ON CONFLICT(source, source_id) DO UPDATE
                 SET title = excluded.title, author = excluded.author, kind = excluded.kind",
                params![path, series.title, series.author, series.kind],
            )?;
            let series_id: i64 = tx.query_row(
                "SELECT id FROM series WHERE source = 'local' AND source_id = ?1",
                params![path],
                |r| r.get(0),
            )?;
            summary.series += 1;

            for chapter in &series.chapters {
                let existing: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM chapters WHERE source_id = ?1",
                        params![chapter.identity],
                        |r| r.get(0),
                    )
                    .optional()?;

                let count = chapter.page_count as i64;
                match existing {
                    // Known content. Refresh what can change, keep the row and its
                    // position.
                    Some(id) => {
                        tx.execute(
                            "UPDATE chapters
                             SET series_id = ?2, title = ?3, number = ?4, page_count = ?5,
                                 path = ?6, mtime = ?7, size = ?8, locator = ?9
                             WHERE id = ?1",
                            params![
                                id,
                                series_id,
                                chapter.title,
                                chapter.number,
                                count,
                                chapter.path.to_string_lossy(),
                                chapter.mtime,
                                chapter.size as i64,
                                chapter.locator
                            ],
                        )?;
                        summary.chapters_kept += 1;
                    }
                    None => {
                        tx.execute(
                            "INSERT INTO chapters
                                 (series_id, source_id, title, number, page_count, path,
                                  mtime, size, locator)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                            params![
                                series_id,
                                chapter.identity,
                                chapter.title,
                                chapter.number,
                                count,
                                chapter.path.to_string_lossy(),
                                chapter.mtime,
                                chapter.size as i64,
                                chapter.locator
                            ],
                        )?;
                        summary.chapters_added += 1;
                    }
                }
            }
        }

        tx.commit()?;
        Ok(summary)
    }

    pub fn catalogs(&self) -> Result<Vec<CatalogRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, url, name FROM opds_catalogs ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(CatalogRow {
                id: r.get(0)?,
                url: r.get(1)?,
                name: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn add_catalog(&self, url: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO opds_catalogs (url, name) VALUES (?1, ?2)
             ON CONFLICT(url) DO UPDATE SET name = excluded.name",
            params![url, name],
        )?;
        Ok(())
    }

    pub fn remove_catalog(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM opds_catalogs WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// What the last scan saw, for the next one to skip.
    ///
    /// One query for the whole library rather than a lookup per chapter: ten thousand
    /// rows of four small columns is nothing next to the I/O it saves.
    pub fn known(&self) -> Result<pr_archive::scan::Known> {
        let mut stmt = self.conn.prepare(
            "SELECT path, mtime, size, source_id, page_count, title, number FROM chapters",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                PathBuf::from(r.get::<_, String>(0)?),
                pr_archive::scan::Cached {
                    mtime: r.get(1)?,
                    size: r.get::<_, i64>(2)? as u64,
                    identity: r.get(3)?,
                    page_count: r.get::<_, i64>(4)? as usize,
                    title: r.get(5)?,
                    number: r.get(6)?,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The whole library. `search` with no query and no category is the same
    /// statement; a second copy of the SQL only creates a path that forgets the
    /// category filter.
    pub fn library(&self) -> Result<Vec<SeriesRow>> {
        self.search("", None)
    }

    /// Chapters of one series, in reading order, each with its saved position.
    pub fn chapters(&self, series_id: i64) -> Result<Vec<ChapterRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.title, c.number, c.page_count, c.path,
                    coalesce(p.page, 0), coalesce(p.page_frac, 0), coalesce(p.completed, 0),
                    c.locator, s.kind
             FROM chapters c
             JOIN series s ON s.id = c.series_id
             LEFT JOIN positions p ON p.chapter_id = c.id
             WHERE c.series_id = ?1
             ORDER BY c.number, c.title",
        )?;
        let rows = stmt.query_map(params![series_id], |r| {
            Ok(ChapterRow {
                id: r.get(0)?,
                title: r.get(1)?,
                number: r.get(2)?,
                page_count: r.get(3)?,
                path: r.get(4)?,
                page: r.get(5)?,
                page_frac: r.get(6)?,
                completed: r.get::<_, i64>(7)? != 0,
                locator: r.get(8)?,
                kind: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Series whose title contains `query`, case-insensitively.
    ///
    /// ponytail: a scan, because a leading wildcard cannot use an index anyway and ten
    /// thousand short titles compare in about a millisecond. FTS5 is the answer when
    /// chapter *text* becomes searchable for the novel reader, not before.
    pub fn search(&self, query: &str, category: Option<i64>) -> Result<Vec<SeriesRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.title, s.source_id, count(c.id),
                    coalesce(sum(CASE WHEN p.completed = 1 THEN 0 ELSE 1 END), 0),
                    (SELECT id FROM chapters WHERE series_id = s.id
                     ORDER BY number, title LIMIT 1),
                    s.added_at, s.kind
             FROM series s
             LEFT JOIN chapters c ON c.series_id = s.id
             LEFT JOIN positions p ON p.chapter_id = c.id
             WHERE s.title LIKE ?1 ESCAPE '\\'
               AND (?2 IS NULL OR EXISTS (
                   SELECT 1 FROM series_categories sc
                   WHERE sc.series_id = s.id AND sc.category_id = ?2))
             GROUP BY s.id ORDER BY s.title",
        )?;
        // The wildcards are ours. Anything typed is literal, so a title containing % or
        // _ searches for that character rather than matching everything.
        let escaped = query
            .replace('\\', r"\\")
            .replace('%', r"\%")
            .replace('_', r"\_");
        let rows = stmt.query_map(params![format!("%{escaped}%"), category], |r| {
            Ok(SeriesRow {
                id: r.get(0)?,
                title: r.get(1)?,
                path: r.get(2)?,
                chapter_count: r.get(3)?,
                unread: r.get(4)?,
                cover_chapter_id: r.get(5)?,
                added_at: r.get(6)?,
                kind: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Where to pick back up, most recently read first.
    ///
    /// Finished chapters are excluded, and so is one still on page zero: opening
    /// something and closing it immediately is not a thing to offer back. One row per
    /// series, or six chapters of one title would be the whole list.
    pub fn continue_reading(&self, limit: i64) -> Result<Vec<ResumeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, s.id, s.title, c.title, c.number, p.page, p.page_frac, c.page_count,
                    s.kind, max(p.updated_at)
             FROM positions p
             JOIN chapters c ON c.id = p.chapter_id
             JOIN series s ON s.id = c.series_id
             WHERE p.completed = 0 AND p.page > 0
             GROUP BY s.id
             ORDER BY max(p.updated_at) DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(ResumeRow {
                chapter_id: r.get(0)?,
                series_id: r.get(1)?,
                series_title: r.get(2)?,
                chapter_title: r.get(3)?,
                number: r.get(4)?,
                page: r.get(5)?,
                page_frac: r.get(6)?,
                page_count: r.get(7)?,
                kind: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn categories(&self) -> Result<Vec<CategoryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.name, c.reading_mode, count(sc.series_id)
             FROM categories c
             LEFT JOIN series_categories sc ON sc.category_id = c.id
             GROUP BY c.id ORDER BY c.sort, c.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CategoryRow {
                id: r.get(0)?,
                name: r.get(1)?,
                reading_mode: mode_from(r.get(2)?),
                series_count: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn create_category(&self, name: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO categories (name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
            params![name],
        )?;
        Ok(())
    }

    pub fn delete_category(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM categories WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn set_category_mode(&self, id: i64, mode: Option<pr_core::ReadingMode>) -> Result<()> {
        self.conn.execute(
            "UPDATE categories SET reading_mode = ?2 WHERE id = ?1",
            params![id, mode_text(mode)],
        )?;
        Ok(())
    }

    pub fn set_series_category(
        &self,
        series_id: i64,
        category_id: i64,
        member: bool,
    ) -> Result<()> {
        if member {
            self.conn.execute(
                "INSERT INTO series_categories (series_id, category_id) VALUES (?1, ?2)
                 ON CONFLICT DO NOTHING",
                params![series_id, category_id],
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM series_categories WHERE series_id = ?1 AND category_id = ?2",
                params![series_id, category_id],
            )?;
        }
        Ok(())
    }

    pub fn categories_of(&self, series_id: i64) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT category_id FROM series_categories WHERE series_id = ?1")?;
        let rows = stmt.query_map(params![series_id], |r| r.get::<_, i64>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_series_mode(
        &self,
        series_id: i64,
        mode: Option<pr_core::ReadingMode>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE series SET reading_mode = ?2 WHERE id = ?1",
            params![series_id, mode_text(mode)],
        )?;
        Ok(())
    }

    /// How a chapter should be read, as far as the library knows.
    ///
    /// Returns the series override and the category default separately, because they sit
    /// at different precedence levels in `pr_core::detect`: an override beats detection,
    /// a category default only applies when nothing was detected.
    ///
    /// A series in two categories that disagree takes the first by sort order. Picking
    /// deterministically matters more than picking cleverly.
    pub fn modes_for_chapter(
        &self,
        chapter_id: i64,
    ) -> Result<(Option<pr_core::ReadingMode>, Option<pr_core::ReadingMode>)> {
        let found: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT s.reading_mode,
                        (SELECT cat.reading_mode FROM series_categories sc
                         JOIN categories cat ON cat.id = sc.category_id
                         WHERE sc.series_id = s.id AND cat.reading_mode IS NOT NULL
                         ORDER BY cat.sort, cat.name LIMIT 1)
                 FROM chapters c JOIN series s ON s.id = c.series_id
                 WHERE c.id = ?1",
                params![chapter_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        Ok(match found {
            Some((series, category)) => (mode_from(series), mode_from(category)),
            None => (None, None),
        })
    }

    /// One chapter by id, which is what opening it needs.
    pub fn chapter(&self, chapter_id: i64) -> Result<Option<ChapterRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT c.id, c.title, c.number, c.page_count, c.path,
                        coalesce(p.page, 0), coalesce(p.page_frac, 0), coalesce(p.completed, 0),
                        c.locator, s.kind
                 FROM chapters c
                 JOIN series s ON s.id = c.series_id
                 LEFT JOIN positions p ON p.chapter_id = c.id
                 WHERE c.id = ?1",
                params![chapter_id],
                |r| {
                    Ok(ChapterRow {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        number: r.get(2)?,
                        page_count: r.get(3)?,
                        path: r.get(4)?,
                        page: r.get(5)?,
                        page_frac: r.get(6)?,
                        completed: r.get::<_, i64>(7)? != 0,
                        locator: r.get(8)?,
                        kind: r.get(9)?,
                    })
                },
            )
            .optional()?)
    }

    /// How long a chapter turned out to be.
    ///
    /// The image reader knows this from the scan -- a CBZ's page count is its entry
    /// count. A novel chapter's length is its block count, and finding that means
    /// parsing it, which a scan of a twelve-hundred-chapter book has no business doing.
    /// So the reader records it the first time the chapter is opened, and the chapter
    /// list shows a length from then on.
    pub fn set_page_count(&self, chapter_id: i64, count: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE chapters SET page_count = ?2 WHERE id = ?1 AND page_count <> ?2",
            params![chapter_id, count],
        )?;
        Ok(())
    }

    /// One row per chapter, rewritten on every page turn. This is precisely why
    /// position is not kept in the settings blob.
    pub fn save_position(
        &self,
        chapter_id: i64,
        page: i64,
        frac: f64,
        completed: bool,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO positions (chapter_id, page, page_frac, completed, updated_at)
             VALUES (?1, ?2, ?3, ?4, CAST(unixepoch('subsec') * 1000 AS INTEGER))
             ON CONFLICT(chapter_id) DO UPDATE
             SET page = excluded.page, page_frac = excluded.page_frac,
                 completed = excluded.completed, updated_at = excluded.updated_at",
            params![chapter_id, page, frac.clamp(0.0, 1.0), completed as i64],
        )?;
        Ok(())
    }

    /// Note a page turn against the reading log.
    ///
    /// Called from the same place as `save_position`, because a turn is exactly when
    /// both facts change. Re-opening a chapter inside the session window extends that
    /// session rather than starting one, so closing the app for coffee does not shred an
    /// afternoon into fifteen rows.
    pub fn record_read(&self, chapter_id: i64, page: i64) -> Result<()> {
        let touched = self.conn.execute(
            "UPDATE history
             SET ended_at = CAST(unixepoch('subsec') * 1000 AS INTEGER),
                 pages = pages + (last_page <> ?2),
                 last_page = ?2
             WHERE id = (SELECT id FROM history
                         WHERE chapter_id = ?1
                           AND ended_at >= CAST(unixepoch('subsec') * 1000 AS INTEGER) - ?3
                         ORDER BY ended_at DESC LIMIT 1)",
            params![chapter_id, page, SESSION_GAP_MS],
        )?;
        if touched > 0 {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO history (chapter_id, started_at, ended_at, pages, last_page)
             VALUES (?1, CAST(unixepoch('subsec') * 1000 AS INTEGER),
                         CAST(unixepoch('subsec') * 1000 AS INTEGER), 1, ?2)",
            params![chapter_id, page],
        )?;
        // Pruned on insert only, and by id rather than by date: ids are monotonic, so
        // this is a range delete on the primary key instead of a sort of the whole log.
        self.conn.execute(
            "DELETE FROM history WHERE id <= (SELECT max(id) - ?1 FROM history)",
            params![HISTORY_CAP],
        )?;
        Ok(())
    }

    pub fn history(&self, limit: i64) -> Result<Vec<HistoryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT h.id, h.chapter_id, s.id, s.title, c.title, c.number,
                    (SELECT id FROM chapters WHERE series_id = s.id ORDER BY number, id LIMIT 1),
                    h.started_at, h.ended_at, h.pages, h.last_page, s.kind
             FROM history h
             JOIN chapters c ON c.id = h.chapter_id
             JOIN series s ON s.id = c.series_id
             ORDER BY h.ended_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(HistoryRow {
                id: r.get(0)?,
                chapter_id: r.get(1)?,
                series_id: r.get(2)?,
                series_title: r.get(3)?,
                chapter_title: r.get(4)?,
                number: r.get(5)?,
                cover_chapter_id: r.get(6)?,
                started_at: r.get(7)?,
                ended_at: r.get(8)?,
                pages: r.get(9)?,
                last_page: r.get(10)?,
                kind: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Forget one session, or all of them. Stats are derived, so this resets them too,
    /// which is the honest behaviour.
    pub fn forget(&self, id: Option<i64>) -> Result<()> {
        match id {
            Some(id) => self
                .conn
                .execute("DELETE FROM history WHERE id = ?1", params![id])?,
            None => self.conn.execute("DELETE FROM history", [])?,
        };
        Ok(())
    }

    pub fn reading_stats(&self) -> Result<ReadingStats> {
        let (chapters, pages, millis) = self.conn.query_row(
            "SELECT count(DISTINCT chapter_id), coalesce(sum(pages), 0),
                    coalesce(sum(ended_at - started_at), 0) FROM history",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)?)),
        )?;

        // Day numbers rather than dates, so "consecutive" is a subtraction. Local time,
        // because a streak is about the reader's evenings, not UTC's.
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT
                    CAST(julianday(started_at / 1000, 'unixepoch', 'localtime') AS INTEGER) AS day
             FROM history ORDER BY day DESC",
        )?;
        let days: Vec<i64> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let today: i64 = self.conn.query_row(
            "SELECT CAST(julianday('now', 'localtime') AS INTEGER)",
            [],
            |r| r.get(0),
        )?;

        let mut best = 0;
        let mut run = 0;
        let mut previous: Option<i64> = None;
        for &day in &days {
            run = if previous == Some(day + 1) {
                run + 1
            } else {
                1
            };
            best = best.max(run);
            previous = Some(day);
        }

        // Yesterday still counts: a streak should survive today not having started yet.
        let offset = match days.first() {
            Some(&first) if first == today => 0,
            Some(&first) if first == today - 1 => 1,
            _ => -1,
        };
        let streak = if offset < 0 {
            0
        } else {
            days.iter()
                .enumerate()
                .take_while(|&(n, &day)| day == today - offset - n as i64)
                .count() as i64
        };

        Ok(ReadingStats {
            chapters,
            pages,
            minutes: millis / 60_000,
            days: days.len() as i64,
            streak,
            best_streak: best,
        })
    }

    /// Bookmark a spot, or clear it if it is already bookmarked. Returns whether one is
    /// there now, which is what the reader's button needs in order to render.
    pub fn toggle_bookmark(
        &self,
        chapter_id: i64,
        page: i64,
        frac: f64,
        paragraph: Option<i64>,
        char_offset: Option<i64>,
    ) -> Result<bool> {
        let gone = self.conn.execute(
            "DELETE FROM bookmarks
             WHERE chapter_id = ?1 AND page = ?2
               AND coalesce(paragraph, -1) = coalesce(?3, -1)
               AND coalesce(char_offset, -1) = coalesce(?4, -1)",
            params![chapter_id, page, paragraph, char_offset],
        )?;
        if gone > 0 {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT INTO bookmarks (chapter_id, page, page_frac, paragraph, char_offset)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                chapter_id,
                page,
                frac.clamp(0.0, 1.0),
                paragraph,
                char_offset
            ],
        )?;
        Ok(true)
    }

    /// Every bookmark, or one chapter's worth.
    pub fn bookmarks(&self, chapter_id: Option<i64>) -> Result<Vec<BookmarkRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT b.id, b.chapter_id, s.id, s.title, c.title, b.page, b.page_frac,
                    b.paragraph, b.char_offset, b.note, b.created_at, s.kind
             FROM bookmarks b
             JOIN chapters c ON c.id = b.chapter_id
             JOIN series s ON s.id = c.series_id
             WHERE ?1 IS NULL OR b.chapter_id = ?1
             ORDER BY s.title, c.number, c.id, b.page",
        )?;
        let rows = stmt.query_map(params![chapter_id], |r| {
            Ok(BookmarkRow {
                id: r.get(0)?,
                chapter_id: r.get(1)?,
                series_id: r.get(2)?,
                series_title: r.get(3)?,
                chapter_title: r.get(4)?,
                page: r.get(5)?,
                page_frac: r.get(6)?,
                paragraph: r.get(7)?,
                char_offset: r.get(8)?,
                note: r.get(9)?,
                created_at: r.get(10)?,
                kind: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn remove_bookmark(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM bookmarks WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn set_bookmark_note(&self, id: i64, note: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE bookmarks SET note = ?2 WHERE id = ?1",
            params![id, note],
        )?;
        Ok(())
    }
}

/// Half an hour away and you were still reading; a day away and you were not.
const SESSION_GAP_MS: i64 = 30 * 60 * 1000;
/// Rows kept. An unbounded log on the page turn path is how a database gets slow.
const HISTORY_CAP: i64 = 5_000;

/// Where the database lives. `PANREADER_DB` overrides it, which is what the tests and a
/// portable install use.
pub fn default_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PANREADER_DB") {
        return Ok(PathBuf::from(path));
    }
    directories::ProjectDirs::from("dev", "panreader", "PanReader")
        .map(|dirs| dirs.data_dir().join("library.db"))
        .ok_or(Error::NoDataDir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pr_core::{Fit, ReadingMode};

    #[test]
    fn a_fresh_database_migrates_and_returns_defaults() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.settings().unwrap(), pr_core::Settings::default());

        let applied: i64 = db
            .conn
            .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(applied, MIGRATIONS.len() as i64);
    }

    #[test]
    fn settings_round_trip_and_overwrite_rather_than_accumulate() {
        let db = Db::open_memory().unwrap();
        let mut settings = pr_core::Settings {
            default_reading_mode: ReadingMode::Ltr,
            fit: Fit::Width,
            page_padding: 16,
            double_page: true,
            ..Default::default()
        };
        db.save_settings(&settings).unwrap();
        assert_eq!(db.settings().unwrap(), settings);

        settings.page_padding = 32;
        db.save_settings(&settings).unwrap();
        assert_eq!(db.settings().unwrap(), settings);

        let rows: i64 = db
            .conn
            .query_row("SELECT count(*) FROM app_config", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "the config blob is one row, not an append log");
    }

    /// A config written by an older build must still load, and a field it never knew
    /// about must come back as its default rather than failing the launch.
    #[test]
    fn a_partial_config_loads_with_defaults_for_what_is_missing() {
        let db = Db::open_memory().unwrap();
        db.conn
            .execute(
                r#"INSERT INTO app_config (id, json) VALUES (1, '{"page_padding": 8}')"#,
                [],
            )
            .unwrap();

        let settings = db.settings().unwrap();
        assert_eq!(settings.page_padding, 8);
        assert_eq!(
            settings.fit,
            Fit::default(),
            "missing field took its default"
        );
    }

    #[test]
    fn unreadable_settings_fall_back_instead_of_failing_the_launch() {
        let db = Db::open_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO app_config (id, json) VALUES (1, 'not json')",
                [],
            )
            .unwrap();
        assert_eq!(db.settings().unwrap(), pr_core::Settings::default());
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let dir = std::env::temp_dir().join("pr_db_reopen_test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("library.db");

        let db = Db::open(&path).unwrap();
        db.save_settings(&pr_core::Settings {
            page_padding: 24,
            ..Default::default()
        })
        .unwrap();
        drop(db);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.settings().unwrap().page_padding, 24, "settings survived");
        let applied: i64 = db
            .conn
            .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(applied, MIGRATIONS.len() as i64, "no migration ran twice");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The guarantee kopuz's schema comment warns about, enforced rather than asked for.
    #[test]
    fn an_edited_migration_is_refused_rather_than_silently_diverging() {
        let db = Db::open_memory().unwrap();
        db.conn
            .execute(
                "UPDATE schema_migrations SET checksum = 'tampered' WHERE version = 1",
                [],
            )
            .unwrap();

        let mut db = db;
        match db.migrate() {
            Err(Error::MigrationChanged { version, .. }) => assert_eq!(version, 1),
            other => panic!("expected a MigrationChanged error, got {other:?}"),
        }
    }

    /// A scan result without touching the filesystem: this is about what `sync` does
    /// with identities, not about how they are produced.
    fn scanned(
        title: &str,
        path: &str,
        chapters: &[(&str, &str)],
    ) -> pr_archive::scan::ScannedSeries {
        pr_archive::scan::ScannedSeries {
            path: PathBuf::from(path),
            title: title.to_owned(),
            author: String::new(),
            kind: "image",
            chapters: chapters
                .iter()
                .map(|(name, identity)| pr_archive::scan::ScannedChapter {
                    path: PathBuf::from(path).join(name),
                    locator: String::new(),
                    title: (*name).to_owned(),
                    number: pr_archive::scan::chapter_number(name),
                    mtime: 0,
                    size: 0,
                    page_count: 20,
                    identity: (*identity).to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_scan_populates_the_library_and_rescanning_adds_nothing() {
        let mut db = Db::open_memory().unwrap();
        let scan = vec![scanned(
            "My Series",
            "/lib/My Series",
            &[("Chapter 1", "blake3:aaa"), ("Chapter 2", "blake3:bbb")],
        )];

        let first = db.sync(&scan).unwrap();
        assert_eq!(first.chapters_added, 2);
        assert_eq!(first.chapters_kept, 0);

        let again = db.sync(&scan).unwrap();
        assert_eq!(again.chapters_added, 0, "a rescan must not duplicate");
        assert_eq!(again.chapters_kept, 2);

        let library = db.library().unwrap();
        assert_eq!(library.len(), 1);
        assert_eq!(library[0].chapter_count, 2);

        let chapters = db.chapters(library[0].id).unwrap();
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].number, Some(1.0), "ordered by chapter number");
    }

    /// The property the whole content-addressing design exists to provide.
    #[test]
    fn renaming_a_chapter_and_its_series_keeps_the_reading_position() {
        let mut db = Db::open_memory().unwrap();
        db.sync(&[scanned(
            "Old Name",
            "/lib/Old Name",
            &[("Chapter 1", "blake3:aaa")],
        )])
        .unwrap();

        let series = db.library().unwrap().remove(0);
        let chapter = db.chapters(series.id).unwrap().remove(0);
        db.save_position(chapter.id, 42, 0.0, false).unwrap();

        // Same bytes, different names, different folder.
        let after = db
            .sync(&[scanned(
                "New Name",
                "/lib/New Name",
                &[("001 - Renamed", "blake3:aaa")],
            )])
            .unwrap();
        assert_eq!(after.chapters_added, 0, "matched by content, not by path");
        assert_eq!(after.chapters_kept, 1);

        let series = db
            .library()
            .unwrap()
            .into_iter()
            .find(|s| s.title == "New Name")
            .expect("the renamed series is in the library");
        let chapter = db.chapters(series.id).unwrap().remove(0);
        assert_eq!(chapter.page, 42, "the reader kept their place");
        assert_eq!(
            chapter.title, "001 - Renamed",
            "and the new name took effect"
        );
    }

    #[test]
    fn a_chapter_missing_from_disk_is_not_deleted() {
        let mut db = Db::open_memory().unwrap();
        db.sync(&[scanned(
            "S",
            "/lib/S",
            &[("Chapter 1", "blake3:aaa"), ("Chapter 2", "blake3:bbb")],
        )])
        .unwrap();

        // An unplugged drive looks exactly like this, and must not erase progress.
        db.sync(&[scanned("S", "/lib/S", &[("Chapter 1", "blake3:aaa")])])
            .unwrap();

        let series = db.library().unwrap().remove(0);
        assert_eq!(series.chapter_count, 2, "the absent chapter survived");
    }

    #[test]
    fn search_matches_case_insensitively_and_treats_wildcards_as_literal() {
        let mut db = Db::open_memory().unwrap();
        db.sync(&[
            scanned("Yotsubato", "/lib/a", &[("c1", "blake3:a")]),
            scanned("Berserk", "/lib/b", &[("c1", "blake3:b")]),
            scanned("100% Orange", "/lib/c", &[("c1", "blake3:c")]),
        ])
        .unwrap();

        let hit = db.search("yotsu", None).unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].title, "Yotsubato");
        assert!(
            hit[0].cover_chapter_id.is_some(),
            "search results carry a cover"
        );

        // A bare % would otherwise match the whole library.
        let literal = db.search("%", None).unwrap();
        assert_eq!(literal.len(), 1, "% is a character, not a wildcard");
        assert_eq!(literal[0].title, "100% Orange");

        assert!(db.search("nothing here", None).unwrap().is_empty());
        assert_eq!(
            db.search("", None).unwrap().len(),
            3,
            "an empty query is everything"
        );
    }

    #[test]
    fn search_narrows_to_a_category_and_composes_with_the_query() {
        let mut db = Db::open_memory().unwrap();
        db.sync(&[
            scanned("Solo Leveling", "/lib/a", &[("c1", "blake3:a")]),
            scanned("Tower of God", "/lib/b", &[("c1", "blake3:b")]),
            scanned("Berserk", "/lib/c", &[("c1", "blake3:c")]),
        ])
        .unwrap();
        let library = db.library().unwrap();
        db.create_category("Manhwa").unwrap();
        let manhwa = db.categories().unwrap().remove(0);
        for row in library.iter().filter(|s| s.title != "Berserk") {
            db.set_series_category(row.id, manhwa.id, true).unwrap();
        }

        assert_eq!(db.search("", None).unwrap().len(), 3);
        assert_eq!(db.search("", Some(manhwa.id)).unwrap().len(), 2);

        // Query and category narrow together rather than replacing one another.
        let both = db.search("tower", Some(manhwa.id)).unwrap();
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].title, "Tower of God");
        assert!(db.search("berserk", Some(manhwa.id)).unwrap().is_empty());
    }

    #[test]
    fn a_category_supplies_a_default_mode_and_a_series_override_outranks_it() {
        let mut db = Db::open_memory().unwrap();
        db.sync(&[scanned("Solo Leveling", "/lib/s", &[("c1", "blake3:s")])])
            .unwrap();
        let series = db.library().unwrap().remove(0);
        let chapter = db.chapters(series.id).unwrap().remove(0);

        // Nothing set: the library has no opinion, so detection decides.
        assert_eq!(db.modes_for_chapter(chapter.id).unwrap(), (None, None));

        db.create_category("Manhwa").unwrap();
        let category = db.categories().unwrap().remove(0);
        db.set_category_mode(category.id, Some(pr_core::ReadingMode::Webtoon))
            .unwrap();
        db.set_series_category(series.id, category.id, true)
            .unwrap();
        assert_eq!(
            db.modes_for_chapter(chapter.id).unwrap(),
            (None, Some(pr_core::ReadingMode::Webtoon)),
            "the category default applies"
        );

        db.set_series_mode(series.id, Some(pr_core::ReadingMode::Ltr))
            .unwrap();
        assert_eq!(
            db.modes_for_chapter(chapter.id).unwrap(),
            (
                Some(pr_core::ReadingMode::Ltr),
                Some(pr_core::ReadingMode::Webtoon)
            ),
            "an override on the series outranks its category"
        );

        assert_eq!(db.categories_of(series.id).unwrap(), vec![category.id]);
        db.set_series_category(series.id, category.id, false)
            .unwrap();
        assert!(db.categories_of(series.id).unwrap().is_empty());
    }

    #[test]
    fn roots_are_added_once_and_can_be_removed() {
        let db = Db::open_memory().unwrap();
        let path = Path::new("/lib");
        db.add_root(path).unwrap();
        db.add_root(path).unwrap();
        assert_eq!(db.roots().unwrap(), vec![PathBuf::from("/lib")]);

        db.remove_root(path).unwrap();
        assert!(db.roots().unwrap().is_empty());
    }

    #[test]
    fn checksum_ignores_line_endings() {
        assert_eq!(
            checksum("CREATE TABLE x;\nSELECT 1;"),
            checksum("CREATE TABLE x;\r\nSELECT 1;")
        );
        assert_ne!(checksum("CREATE TABLE x;"), checksum("CREATE TABLE y;"));
    }

    #[test]
    fn continue_reading_offers_the_latest_chapter_of_each_series_and_skips_finished() {
        let mut db = Db::open_memory().unwrap();
        db.sync(&[
            scanned("A", "/lib/A", &[("A1", "blake3:a1"), ("A2", "blake3:a2")]),
            scanned("B", "/lib/B", &[("B1", "blake3:b1")]),
            scanned("C", "/lib/C", &[("C1", "blake3:c1")]),
        ])
        .unwrap();

        let id = |identity: &str| -> i64 {
            db.conn
                .query_row(
                    "SELECT id FROM chapters WHERE source_id = ?1",
                    params![identity],
                    |r| r.get(0),
                )
                .unwrap()
        };

        db.save_position(id("blake3:a1"), 5, 0.0, false).unwrap();
        db.save_position(id("blake3:a2"), 9, 0.0, false).unwrap();
        db.save_position(id("blake3:b1"), 3, 0.0, true).unwrap();
        // Opened and closed without reading. Not somewhere to be sent back to.
        db.save_position(id("blake3:c1"), 0, 0.0, false).unwrap();

        // Stamped explicitly. Two saves can land in the same millisecond, and the
        // ordering rule is what is under test, not the clock.
        for (identity, at) in [("blake3:a1", 100), ("blake3:a2", 200)] {
            db.conn
                .execute(
                    "UPDATE positions SET updated_at = ?2 WHERE chapter_id = ?1",
                    params![id(identity), at],
                )
                .unwrap();
        }

        let resume = db.continue_reading(10).unwrap();
        assert_eq!(
            resume.len(),
            1,
            "one row per series, and only series actually in progress"
        );
        assert_eq!(resume[0].series_title, "A");
        assert_eq!(
            resume[0].chapter_title, "A2",
            "the most recently read chapter of the series, not the first"
        );
        assert_eq!(resume[0].page, 9);

        let shelf = db.search("", None).unwrap();
        let unread = |title: &str| shelf.iter().find(|r| r.title == title).unwrap().unread;
        assert_eq!(unread("A"), 2, "neither chapter is finished");
        assert_eq!(unread("B"), 0, "its only chapter is read");
        assert_eq!(unread("C"), 1);
    }

    /// S1 asks for the exact page *and offset*. A webtoon page can be eight thousand
    /// pixels tall, so landing at the top of the right page is not landing where you
    /// left off.
    #[test]
    fn a_position_keeps_where_in_the_page_not_only_which_page() {
        let mut db = Db::open_memory().unwrap();
        db.sync(&[scanned("S", "/lib/S", &[("C1", "blake3:c1")])])
            .unwrap();
        let chapter = db.chapters(db.library().unwrap()[0].id).unwrap().remove(0);
        assert_eq!(
            chapter.page_frac, 0.0,
            "an unread chapter starts at the top"
        );

        db.save_position(chapter.id, 3, 0.62, false).unwrap();
        let back = db.chapters(db.library().unwrap()[0].id).unwrap().remove(0);
        assert_eq!(back.page, 3);
        assert!((back.page_frac - 0.62).abs() < 1e-9);

        // A fraction outside the page is a bug upstream, not a scroll target.
        db.save_position(chapter.id, 3, 4.0, false).unwrap();
        let clamped = db.chapters(db.library().unwrap()[0].id).unwrap().remove(0);
        assert_eq!(clamped.page_frac, 1.0);
    }
    /// A chapter to log against, so the history tests are about history.
    fn one_chapter(db: &mut Db) -> i64 {
        db.sync(&[scanned("S", "/lib/S", &[("C1", "blake3:c1")])])
            .unwrap();
        db.chapters(db.library().unwrap()[0].id).unwrap()[0].id
    }

    /// The distinction the table exists for: positions are overwritten, history is not.
    #[test]
    fn a_reading_session_is_one_row_however_many_turns_it_holds() {
        let mut db = Db::open_memory().unwrap();
        let chapter = one_chapter(&mut db);

        for page in [0, 1, 1, 2, 3] {
            db.record_read(chapter, page).unwrap();
        }

        let history = db.history(50).unwrap();
        assert_eq!(history.len(), 1, "one session, not five rows");
        // Five calls, four distinct pages, and the first one opened the session.
        assert_eq!(
            history[0].pages, 4,
            "a re-save of the same page is not a turn"
        );
        assert_eq!(history[0].last_page, 3);
    }

    #[test]
    fn history_prunes_itself_rather_than_growing_without_end() {
        let mut db = Db::open_memory().unwrap();
        let chapter = one_chapter(&mut db);

        // Straight inserts, bypassing the session window, which is the only way to get
        // past the cap without waiting half an hour per row.
        for n in 0..HISTORY_CAP + 20 {
            db.conn
                .execute(
                    "INSERT INTO history (chapter_id, started_at, ended_at) VALUES (?1, ?2, ?2)",
                    params![chapter, n],
                )
                .unwrap();
        }
        db.record_read(chapter, 0).unwrap();

        let kept: i64 = db
            .conn
            .query_row("SELECT count(*) FROM history", [], |r| r.get(0))
            .unwrap();
        assert!(kept <= HISTORY_CAP, "capped, got {kept}");
    }

    #[test]
    fn stats_come_from_history_alone_so_forgetting_it_resets_them() {
        let mut db = Db::open_memory().unwrap();
        let chapter = one_chapter(&mut db);
        for page in 0..5 {
            db.record_read(chapter, page).unwrap();
        }

        let stats = db.reading_stats().unwrap();
        assert_eq!(stats.chapters, 1);
        assert_eq!(stats.pages, 5);
        assert_eq!(stats.days, 1);
        assert_eq!(stats.streak, 1, "read today");
        assert_eq!(stats.best_streak, 1);

        db.forget(None).unwrap();
        let after = db.reading_stats().unwrap();
        assert_eq!(after.pages, 0, "no stored counter left behind");
        assert_eq!(after.streak, 0);
    }

    #[test]
    fn a_streak_counts_consecutive_days_and_a_gap_ends_it() {
        let mut db = Db::open_memory().unwrap();
        let chapter = one_chapter(&mut db);
        let day = 86_400_000i64;
        let now: i64 = db
            .conn
            .query_row(
                "SELECT CAST(unixepoch('subsec') * 1000 AS INTEGER)",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Today, yesterday, the day before -- then a gap, then two more.
        for back in [0, 1, 2, 5, 6] {
            db.conn
                .execute(
                    "INSERT INTO history (chapter_id, started_at, ended_at) VALUES (?1, ?2, ?2)",
                    params![chapter, now - back * day],
                )
                .unwrap();
        }

        let stats = db.reading_stats().unwrap();
        assert_eq!(stats.streak, 3, "the gap at four days back ends it");
        assert_eq!(stats.days, 5);
        assert_eq!(stats.best_streak, 3);
    }

    #[test]
    fn bookmarking_the_same_spot_twice_clears_it() {
        let mut db = Db::open_memory().unwrap();
        let chapter = one_chapter(&mut db);

        assert!(db.toggle_bookmark(chapter, 12, 0.4, None, None).unwrap());
        let marks = db.bookmarks(Some(chapter)).unwrap();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].page, 12);
        assert_eq!(
            marks[0].page_frac, 0.4,
            "returns to the spot, not the page top"
        );

        assert!(!db.toggle_bookmark(chapter, 12, 0.4, None, None).unwrap());
        assert!(db.bookmarks(None).unwrap().is_empty());
    }

    /// Text coordinates and image coordinates share a table without colliding: the two
    /// readers bookmark different things about the same chapter id.
    #[test]
    fn a_text_bookmark_is_a_paragraph_not_a_page() {
        let mut db = Db::open_memory().unwrap();
        let chapter = one_chapter(&mut db);

        db.toggle_bookmark(chapter, 0, 0.0, Some(31), Some(140))
            .unwrap();
        db.toggle_bookmark(chapter, 0, 0.0, Some(31), Some(900))
            .unwrap();
        db.toggle_bookmark(chapter, 0, 0.0, None, None).unwrap();

        let marks = db.bookmarks(Some(chapter)).unwrap();
        assert_eq!(marks.len(), 3, "same page, three distinct spots");

        db.set_bookmark_note(marks[0].id, "the good bit").unwrap();
        assert_eq!(db.bookmarks(None).unwrap()[0].note, "the good bit");
    }
}
