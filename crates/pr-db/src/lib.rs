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
}

/// A chapter as the library lists it, with wherever the reader got to.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChapterRow {
    pub id: i64,
    pub title: String,
    pub number: Option<f64>,
    pub page_count: i64,
    pub series_path: String,
    pub page: i64,
    pub completed: bool,
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
        Ok(rows.filter_map(|r| r.ok()).map(PathBuf::from).collect())
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
                             SET series_id = ?2, title = ?3, number = ?4, page_count = ?5
                             WHERE id = ?1",
                            params![id, series_id, chapter.title, chapter.number, count],
                        )?;
                        summary.chapters_kept += 1;
                    }
                    None => {
                        tx.execute(
                            "INSERT INTO chapters (series_id, source_id, title, number, page_count)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![
                                series_id,
                                chapter.identity,
                                chapter.title,
                                chapter.number,
                                count
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

    pub fn library(&self) -> Result<Vec<SeriesRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.title, s.source_id, count(c.id)
             FROM series s LEFT JOIN chapters c ON c.series_id = s.id
             GROUP BY s.id ORDER BY s.title",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SeriesRow {
                id: r.get(0)?,
                title: r.get(1)?,
                path: r.get(2)?,
                chapter_count: r.get(3)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Chapters of one series, in reading order, each with its saved position.
    pub fn chapters(&self, series_id: i64) -> Result<Vec<ChapterRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.title, c.number, c.page_count, s.source_id,
                    coalesce(p.page, 0), coalesce(p.completed, 0)
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
                series_path: r.get(4)?,
                page: r.get(5)?,
                completed: r.get::<_, i64>(6)? != 0,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
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
