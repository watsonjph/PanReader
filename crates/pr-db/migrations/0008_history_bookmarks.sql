-- History and bookmarks. Both are things `positions` deliberately cannot answer.

-- History is a log of reading sessions, not of page turns.
--
-- `positions` holds one row per chapter and is overwritten on every turn: it answers
-- "where am I". History answers "what was I reading on Tuesday", which needs the row
-- to survive the next turn. A row per turn would be an unbounded write on the page
-- turn path, so instead a row covers a session: re-opening the same chapter inside the
-- session window extends the row rather than adding one.
--
-- `last_page` exists so `pages` counts turns rather than saves. Position is written on
-- every scroll settle, and counting those would make "pages read" a measure of how
-- twitchy the scroll wheel is.
CREATE TABLE history (
    id         INTEGER PRIMARY KEY,
    chapter_id INTEGER NOT NULL REFERENCES chapters(id) ON DELETE CASCADE,
    -- Milliseconds, matching positions.updated_at.
    started_at INTEGER NOT NULL,
    ended_at   INTEGER NOT NULL,
    pages      INTEGER NOT NULL DEFAULT 1,
    last_page  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_history_time    ON history(started_at DESC);
CREATE INDEX idx_history_chapter ON history(chapter_id, ended_at DESC);

-- A place worth coming back to. The image reader fills page and page_frac; the text
-- reader fills paragraph and char_offset, the same coordinates its position uses, so a
-- font change does not move a bookmark.
CREATE TABLE bookmarks (
    id          INTEGER PRIMARY KEY,
    chapter_id  INTEGER NOT NULL REFERENCES chapters(id) ON DELETE CASCADE,
    page        INTEGER NOT NULL DEFAULT 0,
    page_frac   REAL NOT NULL DEFAULT 0,
    paragraph   INTEGER,
    char_offset INTEGER,
    note        TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL DEFAULT (unixepoch())
);
-- Bookmarking the same spot twice is a toggle, not two rows. A plain UNIQUE would not
-- do it: SQLite treats NULLs as distinct, so every image-reader row would be unique on
-- the strength of its two empty text columns.
CREATE UNIQUE INDEX idx_bookmark_spot
    ON bookmarks(chapter_id, page, coalesce(paragraph, -1), coalesce(char_offset, -1));
CREATE INDEX idx_bookmark_chapter ON bookmarks(chapter_id);
