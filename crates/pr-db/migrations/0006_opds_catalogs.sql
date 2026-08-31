-- OPDS catalogs the reader added. Just an address and a name to show it under.
--
-- No credentials column, and there will not be one: invariant 13 keeps us to
-- free-to-access content, so a catalog that demands a login is reported rather than
-- worked around.
CREATE TABLE opds_catalogs (
    id       INTEGER PRIMARY KEY,
    url      TEXT NOT NULL UNIQUE,
    name     TEXT NOT NULL,
    added_at INTEGER NOT NULL DEFAULT (unixepoch())
);
