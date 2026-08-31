-- Identity says whether two chapters are the same; it does not say where to read one
-- from. Reconstructing the path from the series folder plus the chapter title is lossy
-- -- an archive's title is its stem, without the extension -- so store it.
--
-- The path changes when a file moves; the identity does not. That is the division of
-- labour: identity matches, path opens.
ALTER TABLE chapters ADD COLUMN path TEXT NOT NULL DEFAULT '';
