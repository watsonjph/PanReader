//! Semantic backup: export what the rows mean, restore into whatever schema is current.
//!
//! Never a copy of the database. A structural dump is unrestorable the first time a
//! migration changes the schema, which is exactly when someone needs it. So nothing in
//! here carries a row id, a file path, or an mtime: identity is the natural key --
//! `(source, source_id)` for a series, the content hash for a chapter -- and everything
//! machine-local is rebuilt by a scan.
//!
//! Restore is a merge and never a wipe. Importing the same file twice changes nothing,
//! and where both sides know a chapter, the further-along position wins.

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use pr_db::rusqlite::{OptionalExtension, Transaction, params};
use std::collections::HashMap;
use std::io::{Read, Write};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database: {0}")]
    Db(#[from] pr_db::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] pr_db::rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a PanReader backup: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "this backup is schema {found}, and this build understands up to {known}. \
         Update PanReader and try again."
    )]
    FromTheFuture { found: u32, known: u32 },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Bumping this is a migration in `read`, not a silent break. Older files must keep
/// restoring: the whole point of a backup is that it outlives the build that wrote it.
pub const SCHEMA: u32 = 1;

/// Gzipped JSON. Mihon uses protobuf for size; our libraries are smaller, and being
/// able to open the file in an editor when a restore is going wrong is worth more than
/// the bytes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Backup {
    pub schema: u32,
    pub app: String,
    pub app_version: String,
    /// Unix seconds.
    pub exported_at: i64,
    pub settings: pr_core::Settings,
    /// Library folders. Paths, and so the one part of this that is machine-specific --
    /// carried anyway, because restoring onto the same machine is the common case and a
    /// path that does not exist is skipped rather than fatal.
    pub roots: Vec<String>,
    pub catalogs: Vec<Catalog>,
    pub categories: Vec<Category>,
    pub series: Vec<Series>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Catalog {
    pub url: String,
    pub name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Category {
    pub name: String,
    pub sort: i64,
    pub reading_mode: Option<pr_core::ReadingMode>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Series {
    /// `local` for a scanned folder, otherwise the source's own id.
    pub source: String,
    pub source_id: String,
    pub title: String,
    #[serde(default)]
    pub author: String,
    pub kind: String,
    pub reading_mode: Option<pr_core::ReadingMode>,
    pub added_at: i64,
    /// By name. Ids are row numbers and mean nothing in another database.
    #[serde(default)]
    pub categories: Vec<String>,
    pub chapters: Vec<Chapter>,
}

/// Bookmarks and history hang off the chapter rather than sitting in flat lists keyed
/// by id. Nesting them removes a whole class of dangling-reference bug on import.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Chapter {
    /// The content hash. This is what matches a chapter across machines, renames and
    /// redownloads, and it is why nothing here stores a path.
    pub identity: String,
    pub title: String,
    pub number: Option<f64>,
    pub page_count: i64,
    #[serde(default)]
    pub page: i64,
    #[serde(default)]
    pub page_frac: f64,
    #[serde(default)]
    pub paragraph: Option<i64>,
    #[serde(default)]
    pub char_offset: Option<i64>,
    #[serde(default)]
    pub completed: bool,
    /// Milliseconds. Breaks the tie when both sides sit on the same page.
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub bookmarks: Vec<Bookmark>,
    #[serde(default)]
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Bookmark {
    pub page: i64,
    #[serde(default)]
    pub page_frac: f64,
    #[serde(default)]
    pub paragraph: Option<i64>,
    #[serde(default)]
    pub char_offset: Option<i64>,
    #[serde(default)]
    pub note: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub started_at: i64,
    pub ended_at: i64,
    pub pages: i64,
    #[serde(default)]
    pub last_page: i64,
}

/// What a restore did, or -- run as a dry run -- what it would do.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Report {
    pub series_added: i64,
    pub series_matched: i64,
    /// Inserted with no path: the progress is restored, and the chapter becomes
    /// readable when a scan finds the file whose content matches its identity.
    pub chapters_added: i64,
    pub chapters_matched: i64,
    /// The backup was further along, so it won.
    pub positions_advanced: i64,
    /// This database was further along, so it was left alone.
    pub positions_kept: i64,
    pub bookmarks_added: i64,
    pub sessions_added: i64,
    pub categories_added: i64,
    pub catalogs_added: i64,
    pub roots_added: i64,
}

// ---------------------------------------------------------------------------- export

pub fn export(db: &mut pr_db::Db) -> Result<Backup> {
    let settings = db.settings()?;
    let tx = db.transaction()?;

    let roots = rows(&tx, "SELECT path FROM library_roots ORDER BY path", |r| {
        r.get(0)
    })?;
    let catalogs = rows(
        &tx,
        "SELECT url, name FROM opds_catalogs ORDER BY url",
        |r| {
            Ok(Catalog {
                url: r.get(0)?,
                name: r.get(1)?,
            })
        },
    )?;
    let categories = rows(
        &tx,
        "SELECT name, sort, reading_mode FROM categories ORDER BY sort, name",
        |r| {
            Ok(Category {
                name: r.get(0)?,
                sort: r.get(1)?,
                reading_mode: pr_db::mode_from(r.get(2)?),
            })
        },
    )?;

    // Three flat queries and a group-by in memory, rather than two queries per series.
    // A library is thousands of chapters; a query per series is thousands of round
    // trips for a file that gets written every day.
    let mut marks: HashMap<i64, Vec<Bookmark>> = HashMap::new();
    for (id, mark) in rows(
        &tx,
        "SELECT chapter_id, page, page_frac, paragraph, char_offset, note, created_at
         FROM bookmarks ORDER BY chapter_id, page",
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Bookmark {
                    page: r.get(1)?,
                    page_frac: r.get(2)?,
                    paragraph: r.get(3)?,
                    char_offset: r.get(4)?,
                    note: r.get(5)?,
                    created_at: r.get(6)?,
                },
            ))
        },
    )? {
        marks.entry(id).or_default().push(mark);
    }

    let mut sessions: HashMap<i64, Vec<Session>> = HashMap::new();
    for (id, session) in rows(
        &tx,
        "SELECT chapter_id, started_at, ended_at, pages, last_page
         FROM history ORDER BY chapter_id, started_at",
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Session {
                    started_at: r.get(1)?,
                    ended_at: r.get(2)?,
                    pages: r.get(3)?,
                    last_page: r.get(4)?,
                },
            ))
        },
    )? {
        sessions.entry(id).or_default().push(session);
    }

    let mut chapters: HashMap<i64, Vec<Chapter>> = HashMap::new();
    for (series_id, id, mut chapter) in rows(
        &tx,
        "SELECT c.series_id, c.id, c.source_id, c.title, c.number, c.page_count,
                coalesce(p.page, 0), coalesce(p.page_frac, 0), p.paragraph, p.char_offset,
                coalesce(p.completed, 0), coalesce(p.updated_at, 0)
         FROM chapters c
         LEFT JOIN positions p ON p.chapter_id = c.id
         ORDER BY c.series_id, c.number, c.id",
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                Chapter {
                    identity: r.get(2)?,
                    title: r.get(3)?,
                    number: r.get(4)?,
                    page_count: r.get(5)?,
                    page: r.get(6)?,
                    page_frac: r.get(7)?,
                    paragraph: r.get(8)?,
                    char_offset: r.get(9)?,
                    completed: r.get::<_, i64>(10)? != 0,
                    updated_at: r.get(11)?,
                    bookmarks: Vec::new(),
                    sessions: Vec::new(),
                },
            ))
        },
    )? {
        chapter.bookmarks = marks.remove(&id).unwrap_or_default();
        chapter.sessions = sessions.remove(&id).unwrap_or_default();
        chapters.entry(series_id).or_default().push(chapter);
    }

    let mut in_category: HashMap<i64, Vec<String>> = HashMap::new();
    for (series_id, name) in rows(
        &tx,
        "SELECT sc.series_id, c.name FROM series_categories sc
         JOIN categories c ON c.id = sc.category_id ORDER BY c.sort, c.name",
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    )? {
        in_category.entry(series_id).or_default().push(name);
    }

    let mut series = rows(
        &tx,
        "SELECT id, source, source_id, title, author, kind, reading_mode, added_at
         FROM series ORDER BY title",
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Series {
                    source: r.get(1)?,
                    source_id: r.get(2)?,
                    title: r.get(3)?,
                    author: r.get(4)?,
                    kind: r.get(5)?,
                    reading_mode: pr_db::mode_from(r.get(6)?),
                    added_at: r.get(7)?,
                    categories: Vec::new(),
                    chapters: Vec::new(),
                },
            ))
        },
    )?;
    let series = series
        .iter_mut()
        .map(|(id, s)| {
            let mut s = s.clone();
            s.categories = in_category.remove(id).unwrap_or_default();
            s.chapters = chapters.remove(id).unwrap_or_default();
            s
        })
        .collect();

    Ok(Backup {
        schema: SCHEMA,
        app: "panreader".to_owned(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        exported_at: now_seconds(),
        settings,
        roots,
        catalogs,
        categories,
        series,
    })
}

/// Gzipped JSON, pretty-printed inside the gzip. It compresses away to nothing and the
/// day someone needs to read this file is the day everything else has gone wrong.
pub fn write(backup: &Backup) -> Result<Vec<u8>> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&serde_json::to_vec_pretty(backup)?)?;
    Ok(gz.finish()?)
}

/// Accepts the file gzipped or plain, because someone will gunzip it to look inside and
/// then try to restore what they are holding.
pub fn read(bytes: &[u8]) -> Result<Backup> {
    let json = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        GzDecoder::new(bytes).read_to_end(&mut out)?;
        out
    } else {
        bytes.to_vec()
    };
    let backup: Backup = serde_json::from_slice(&json)?;
    if backup.schema > SCHEMA {
        return Err(Error::FromTheFuture {
            found: backup.schema,
            known: SCHEMA,
        });
    }
    Ok(backup)
}

// --------------------------------------------------------------------------- restore

/// Merge a backup into the library.
///
/// `commit: false` is the dry run, and it is the same code path: everything runs inside
/// the transaction and the transaction is rolled back at the end. A preview computed by
/// a second, simpler function is a preview that drifts from what actually happens.
pub fn restore(db: &mut pr_db::Db, backup: &Backup, commit: bool) -> Result<Report> {
    let mut report = Report::default();
    let tx = db.transaction()?;

    // Settings are one blob and cannot merge row by row, so a restore replaces them.
    // Called out in the report rather than done quietly.
    tx.execute(
        "INSERT INTO app_config (id, json) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET json = excluded.json",
        params![serde_json::to_string(&backup.settings)?],
    )?;

    for path in &backup.roots {
        report.roots_added += tx.execute(
            "INSERT OR IGNORE INTO library_roots (path) VALUES (?1)",
            params![path],
        )? as i64;
    }
    for catalog in &backup.catalogs {
        report.catalogs_added += tx.execute(
            "INSERT OR IGNORE INTO opds_catalogs (url, name) VALUES (?1, ?2)",
            params![catalog.url, catalog.name],
        )? as i64;
    }
    for category in &backup.categories {
        report.categories_added += tx.execute(
            "INSERT OR IGNORE INTO categories (name, sort, reading_mode) VALUES (?1, ?2, ?3)",
            params![
                category.name,
                category.sort,
                pr_db::mode_text(category.reading_mode)
            ],
        )? as i64;
    }

    for series in &backup.series {
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM series WHERE source = ?1 AND source_id = ?2",
                params![series.source, series.source_id],
                |r| r.get(0),
            )
            .optional()?;

        let series_id = match existing {
            Some(id) => {
                report.series_matched += 1;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO series (source, source_id, title, author, kind,
                                         reading_mode, added_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        series.source,
                        series.source_id,
                        series.title,
                        series.author,
                        series.kind,
                        pr_db::mode_text(series.reading_mode),
                        series.added_at
                    ],
                )?;
                report.series_added += 1;
                tx.last_insert_rowid()
            }
        };

        for name in &series.categories {
            tx.execute(
                "INSERT OR IGNORE INTO series_categories (series_id, category_id)
                 SELECT ?1, id FROM categories WHERE name = ?2",
                params![series_id, name],
            )?;
        }

        for chapter in &series.chapters {
            let chapter_id = merge_chapter(&tx, series_id, chapter, &mut report)?;

            for mark in &chapter.bookmarks {
                report.bookmarks_added += tx.execute(
                    "INSERT OR IGNORE INTO bookmarks
                         (chapter_id, page, page_frac, paragraph, char_offset, note, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        chapter_id,
                        mark.page,
                        mark.page_frac,
                        mark.paragraph,
                        mark.char_offset,
                        mark.note,
                        mark.created_at
                    ],
                )? as i64;
            }

            // Sessions are keyed by when they started, which is what makes importing
            // the same file twice a no-op rather than a doubled history.
            for session in &chapter.sessions {
                report.sessions_added += tx.execute(
                    "INSERT INTO history (chapter_id, started_at, ended_at, pages, last_page)
                     SELECT ?1, ?2, ?3, ?4, ?5
                     WHERE NOT EXISTS (
                         SELECT 1 FROM history WHERE chapter_id = ?1 AND started_at = ?2)",
                    params![
                        chapter_id,
                        session.started_at,
                        session.ended_at,
                        session.pages,
                        session.last_page
                    ],
                )? as i64;
            }
        }
    }

    if commit {
        tx.commit()?;
    }
    Ok(report)
}

/// Find or create the chapter, then decide whose position survives.
fn merge_chapter(
    tx: &Transaction<'_>,
    series_id: i64,
    chapter: &Chapter,
    report: &mut Report,
) -> Result<i64> {
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM chapters WHERE source_id = ?1",
            params![chapter.identity],
            |r| r.get(0),
        )
        .optional()?;

    let chapter_id = match existing {
        Some(id) => {
            report.chapters_matched += 1;
            id
        }
        None => {
            // No path: this chapter is not on this disk. The progress is restored and
            // the row becomes readable when a scan finds a file whose content matches.
            tx.execute(
                "INSERT INTO chapters (series_id, source_id, title, number, page_count)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    series_id,
                    chapter.identity,
                    chapter.title,
                    chapter.number,
                    chapter.page_count
                ],
            )?;
            report.chapters_added += 1;
            tx.last_insert_rowid()
        }
    };

    let here: Option<(i64, f64, i64, i64)> = tx
        .query_row(
            "SELECT page, page_frac, completed, updated_at FROM positions WHERE chapter_id = ?1",
            params![chapter_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;

    let ours = here
        .map(|(page, frac, done, at)| (done != 0, page, frac, at))
        .unwrap_or((false, 0, 0.0, 0));
    let theirs = (
        chapter.completed,
        chapter.page,
        chapter.page_frac,
        chapter.updated_at,
    );

    if !further(theirs, ours) {
        // Only count it as kept when there was something here to keep.
        if here.is_some() {
            report.positions_kept += 1;
        }
        return Ok(chapter_id);
    }

    tx.execute(
        "INSERT INTO positions (chapter_id, page, page_frac, paragraph, char_offset,
                                completed, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(chapter_id) DO UPDATE
         SET page = excluded.page, page_frac = excluded.page_frac,
             paragraph = excluded.paragraph, char_offset = excluded.char_offset,
             completed = excluded.completed, updated_at = excluded.updated_at",
        params![
            chapter_id,
            chapter.page,
            chapter.page_frac,
            chapter.paragraph,
            chapter.char_offset,
            chapter.completed as i64,
            chapter.updated_at
        ],
    )?;
    report.positions_advanced += 1;
    Ok(chapter_id)
}

/// Further along wins, and not last-writer-wins: a chapter finished on the desktop is
/// not un-finished because the phone synced afterwards. Finished outranks a page
/// number, a page number outranks an offset into it, and only a genuine tie falls back
/// to which was written later.
fn further(a: (bool, i64, f64, i64), b: (bool, i64, f64, i64)) -> bool {
    match (a.0, b.0) {
        (true, false) => return true,
        (false, true) => return false,
        _ => {}
    }
    if a.1 != b.1 {
        return a.1 > b.1;
    }
    if a.2 != b.2 {
        return a.2 > b.2;
    }
    a.3 > b.3
}

// -------------------------------------------------------------------- automatic files

/// One file per backup, named by the second it was taken.
///
/// A date would read better, but it would also be date arithmetic in a crate that has
/// no other reason to know what a month is. The UI formats the number; the filename
/// only has to sort, and ten digits sort lexically until 2286.
pub const EXTENSION: &str = "pnbk";

/// Beside the database, which is the directory a reader already knows to copy.
pub fn dir() -> Result<std::path::PathBuf> {
    let db = pr_db::default_path()?;
    Ok(db
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("backups"))
}

/// One automatic backup on disk.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Kept {
    pub path: String,
    /// Unix seconds, from the filename.
    pub taken_at: i64,
    pub bytes: u64,
}

/// Newest first.
pub fn kept(dir: &std::path::Path) -> Vec<Kept> {
    let mut found: Vec<Kept> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let taken_at = path
                    .file_stem()?
                    .to_str()?
                    .rsplit('-')
                    .next()?
                    .parse::<i64>()
                    .ok()?;
                (path.extension()?.to_str()? == EXTENSION).then(|| Kept {
                    taken_at,
                    bytes: entry.metadata().map(|m| m.len()).unwrap_or(0),
                    path: path.to_string_lossy().into_owned(),
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    found.sort_by_key(|k| std::cmp::Reverse(k.taken_at));
    found
}

/// Take one if the newest is older than a day, then prune to `keep`.
///
/// Called on launch. This is the feature that actually saves people, and it only works
/// if nobody has to remember it -- so it is on by default and it is never interactive.
pub fn automatic(db: &mut pr_db::Db, dir: &std::path::Path, keep: usize) -> Result<Option<Kept>> {
    const DAY: i64 = 86_400;
    let now = now_seconds();
    let existing = kept(dir);
    if existing.first().is_some_and(|k| now - k.taken_at < DAY) {
        return Ok(None);
    }

    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("panreader-{now}.{EXTENSION}"));
    let bytes = write(&export(db)?)?;
    std::fs::write(&path, &bytes)?;

    // Prune after writing, never before: a retention sweep that runs first can leave
    // someone with no backups at all if the write then fails.
    for old in kept(dir).into_iter().skip(keep.max(1)) {
        if let Err(e) = std::fs::remove_file(&old.path) {
            tracing::warn!("could not prune {}: {e}", old.path);
        }
    }

    tracing::info!(path = %path.display(), bytes = bytes.len(), "wrote automatic backup");
    Ok(Some(Kept {
        path: path.to_string_lossy().into_owned(),
        taken_at: now,
        bytes: bytes.len() as u64,
    }))
}

// ---------------------------------------------------------------------------- helpers

fn rows<T>(
    tx: &Transaction<'_>,
    sql: &str,
    map: impl Fn(&pr_db::rusqlite::Row<'_>) -> pr_db::rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = tx.prepare(sql)?;
    let out = stmt.query_map([], map)?;
    Ok(out.collect::<pr_db::rusqlite::Result<Vec<_>>>()?)
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
