-- Repositories the reader added, and the sources installed from them.
--
-- We host no repository and bundle no extension: every row here started as a URL
-- somebody typed.

CREATE TABLE repositories (
    id       INTEGER PRIMARY KEY,
    url      TEXT NOT NULL UNIQUE,
    name     TEXT NOT NULL DEFAULT '',
    added_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Installed sources.
--
-- Keyed by the plugin's own id rather than a row number, because `series.source` holds
-- that id and a library entry has to survive a reinstall. That is also why removing a
-- repository cascades to its sources and stops there: the entries that came from them
-- stay, unlinked, exactly as an unmatched import does.
--
-- The bundle text lives here rather than beside the database. A source is a few tens of
-- kilobytes of JavaScript, it is meaningless without the row, and keeping the two
-- together means there are no orphan files to sweep when a repository is removed. It is
-- deliberately not in the backup -- a backup carries the source list and the repository
-- URLs, never someone else's code.
CREATE TABLE sources (
    id           TEXT PRIMARY KEY,
    repo_id      INTEGER REFERENCES repositories(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    version      TEXT NOT NULL DEFAULT '',
    lang         TEXT NOT NULL DEFAULT 'en',
    -- `image` or `text`, spelled the way series.kind is, so one word means one thing.
    kind         TEXT NOT NULL,
    nsfw         INTEGER NOT NULL DEFAULT 0,
    -- The allowlist as the bundle declares it, one host per line. Stored so Settings
    -- can show what a source may reach without waking its isolate.
    hosts        TEXT NOT NULL DEFAULT '',
    bundle       TEXT NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 1,
    installed_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX idx_sources_repo ON sources(repo_id);
