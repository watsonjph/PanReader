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
const MIGRATIONS: &[(&str, &str)] = &[("0001_init", include_str!("../migrations/0001_init.sql"))];

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

    #[test]
    fn checksum_ignores_line_endings() {
        assert_eq!(
            checksum("CREATE TABLE x;\nSELECT 1;"),
            checksum("CREATE TABLE x;\r\nSELECT 1;")
        );
        assert_ne!(checksum("CREATE TABLE x;"), checksum("CREATE TABLE y;"));
    }
}
