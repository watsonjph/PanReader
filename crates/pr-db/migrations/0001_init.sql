-- PanReader schema, version 1.
--
-- Migrations are append-only and checksummed. Never edit a file that has been applied:
-- add a new one. The runner refuses to open a database whose recorded checksum for an
-- applied migration no longer matches, because silently diverging schemas are far
-- worse to debug than a startup error.

-- Settings: one row, whole blob, loaded and saved wholesale. Never field-queried.
-- Anything written often enough to matter gets carved out into its own table below.
CREATE TABLE app_config (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    json TEXT NOT NULL
);

-- Folders the reader pointed us at.
CREATE TABLE library_roots (
    id       INTEGER PRIMARY KEY,
    path     TEXT NOT NULL UNIQUE,
    added_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Series. Identity is (source, source_id): for local files the source is 'local' and
-- source_id is the folder path; for a plugin it is the source id and its own key.
CREATE TABLE series (
    id           INTEGER PRIMARY KEY,
    source       TEXT NOT NULL,
    source_id    TEXT NOT NULL,
    title        TEXT NOT NULL,
    author       TEXT NOT NULL DEFAULT '',
    -- Which reader opens it. 'image' for manga and manhwa, 'text' for novels.
    kind         TEXT NOT NULL DEFAULT 'image',
    -- Per-series override. NULL means detect, which is the common case.
    reading_mode TEXT,
    cover_path   TEXT,
    added_at     INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (source, source_id)
);
CREATE INDEX idx_series_source ON series(source);
CREATE INDEX idx_series_title  ON series(title);

CREATE TABLE chapters (
    id         INTEGER PRIMARY KEY,
    series_id  INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
    source_id  TEXT NOT NULL,
    title      TEXT NOT NULL,
    -- Real, because chapters are numbered 10.5 more often than anyone would like.
    number     REAL,
    page_count INTEGER NOT NULL DEFAULT 0,
    added_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (series_id, source_id)
);
CREATE INDEX idx_chapters_series ON chapters(series_id, number);

-- Reading position, deliberately not in the config blob.
--
-- This is written on every page turn. Keeping it in the settings JSON would rewrite the
-- whole blob each time; here it is a one-row upsert.
CREATE TABLE positions (
    chapter_id  INTEGER PRIMARY KEY REFERENCES chapters(id) ON DELETE CASCADE,
    -- Image reader.
    page        INTEGER NOT NULL DEFAULT 0,
    -- Text reader: paragraph index plus character offset, so a font change does not
    -- move you. A pixel offset would.
    paragraph   INTEGER,
    char_offset INTEGER,
    completed   INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX idx_positions_updated ON positions(updated_at DESC);

CREATE TABLE categories (
    id      INTEGER PRIMARY KEY,
    name    TEXT NOT NULL UNIQUE,
    sort    INTEGER NOT NULL DEFAULT 0,
    -- Per-category default, which is how a "Manhwa" category reads as a strip without
    -- anyone setting it per series.
    reading_mode TEXT
);

CREATE TABLE series_categories (
    series_id   INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
    category_id INTEGER NOT NULL REFERENCES categories(id) ON DELETE CASCADE,
    PRIMARY KEY (series_id, category_id)
);
