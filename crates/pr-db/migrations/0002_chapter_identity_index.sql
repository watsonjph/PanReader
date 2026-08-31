-- Chapters are matched by content identity during a rescan rather than by path, so
-- that renaming a file keeps the reader's progress. Without this index that lookup is a
-- full table scan once per chapter found on disk.
CREATE INDEX idx_chapters_identity ON chapters(source_id);
