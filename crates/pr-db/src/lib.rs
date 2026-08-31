//! SQLite library, settings and reading positions.
//!
//! Synchronous on purpose. `CLAUDE.md` keeps tokio in `pr-app`, `pr-server` and
//! `pr-engine`; callers here use `spawn_blocking`. SQLite is fast enough that an async
//! driver would buy nothing but a runtime dependency and compile-time schema plumbing.

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
    /// First chapter in reading order. Its first page is the cover.
    pub cover_chapter_id: Option<i64>,
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
    pub page: i64,
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

fn mode_text(mode: Option<pr_core::ReadingMode>) -> Option<String> {
    mode.map(|m| match m {
        pr_core::ReadingMode::Rtl => "rtl".to_owned(),
        pr_core::ReadingMode::Ltr => "ltr".to_owned(),
        pr_core::ReadingMode::Webtoon => "webtoon".to_owned(),
    })
}

fn mode_from(text: Option<String>) -> Option<pr_core::ReadingMode> {
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
                "INSERT INTO series (source, source_id, title) VALUES ('local', ?1, ?2)
                 ON CONFLICT(source, source_id) DO UPDATE SET title = excluded.title",
                params![path, series.title],
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
                                 path = ?6, mtime = ?7, size = ?8
                             WHERE id = ?1",
                            params![
                                id,
                                series_id,
                                chapter.title,
                                chapter.number,
                                count,
                                chapter.path.to_string_lossy(),
                                chapter.mtime,
                                chapter.size as i64
                            ],
                        )?;
                        summary.chapters_kept += 1;
                    }
                    None => {
                        tx.execute(
                            "INSERT INTO chapters
                                 (series_id, source_id, title, number, page_count, path,
                                  mtime, size)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                            params![
                                series_id,
                                chapter.identity,
                                chapter.title,
                                chapter.number,
                                count,
                                chapter.path.to_string_lossy(),
                                chapter.mtime,
                                chapter.size as i64
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

    /// What the last scan saw, for the next one to skip.
    ///
    /// One query for the whole library rather than a lookup per chapter: ten thousand
    /// rows of four small columns is nothing next to the I/O it saves.
    pub fn known(&self) -> Result<pr_archive::scan::Known> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime, size, source_id, page_count FROM chapters")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                PathBuf::from(r.get::<_, String>(0)?),
                pr_archive::scan::Cached {
                    mtime: r.get(1)?,
                    size: r.get::<_, i64>(2)? as u64,
                    identity: r.get(3)?,
                    page_count: r.get::<_, i64>(4)? as usize,
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
                    coalesce(p.page, 0), coalesce(p.completed, 0)
             FROM chapters c
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
                completed: r.get::<_, i64>(6)? != 0,
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
                    (SELECT id FROM chapters WHERE series_id = s.id
                     ORDER BY number, title LIMIT 1)
             FROM series s LEFT JOIN chapters c ON c.series_id = s.id
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
                cover_chapter_id: r.get(4)?,
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
                        coalesce(p.page, 0), coalesce(p.completed, 0)
                 FROM chapters c
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
                        completed: r.get::<_, i64>(6)? != 0,
                    })
                },
            )
            .optional()?)
    }

    /// One row per chapter, rewritten on every page turn. This is precisely why
    /// position is not kept in the settings blob.
    pub fn save_position(&self, chapter_id: i64, page: i64, completed: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO positions (chapter_id, page, completed, updated_at)
             VALUES (?1, ?2, ?3, unixepoch())
             ON CONFLICT(chapter_id) DO UPDATE
             SET page = excluded.page, completed = excluded.completed,
                 updated_at = excluded.updated_at",
            params![chapter_id, page, completed as i64],
        )?;
        Ok(())
    }
}

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
            chapters: chapters
                .iter()
                .map(|(name, identity)| pr_archive::scan::ScannedChapter {
                    path: PathBuf::from(path).join(name),
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
        db.save_position(chapter.id, 42, false).unwrap();

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
}
