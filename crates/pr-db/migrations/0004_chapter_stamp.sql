-- What a scan saw on disk, so a rescan can skip opening a chapter it already knows.
-- Zero means "never stamped", which never matches a real stat and so rescans normally.
ALTER TABLE chapters ADD COLUMN mtime INTEGER NOT NULL DEFAULT 0;
ALTER TABLE chapters ADD COLUMN size INTEGER NOT NULL DEFAULT 0;
