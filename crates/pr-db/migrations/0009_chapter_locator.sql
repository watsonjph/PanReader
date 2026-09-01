-- Where inside the container a chapter lives.

-- `path` opens the file; for a folder or a CBZ that is the whole address, because the
-- chapter *is* the file. A novel is one EPUB holding every chapter, so the file is only
-- half of it and the other half is the entry within: `OEBPS/text/one.xhtml`.
--
-- Its own column rather than a suffix on `path`, because a path with a separator baked
-- into it is a path that something eventually passes to the filesystem.
ALTER TABLE chapters ADD COLUMN locator TEXT NOT NULL DEFAULT '';
