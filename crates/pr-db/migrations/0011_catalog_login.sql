-- A username for catalogs that want one.

-- 0006 said there would never be a credentials column, on the reading of invariant 13
-- that kept us to free-to-access content. That invariant has since been split in two,
-- and the half about authentication says the opposite: signing in to a server the
-- reader runs or has an account on -- Suwayomi, Komga, Kavita -- is their own library
-- and their own account, and is nothing to do with bot detection.
--
-- The username is not a secret and lives here so the UI can show who it signs in as.
-- The password is not here and never will be: it goes in the OS credential store,
-- keyed by origin. A database that holds the password is a backup that leaks it.
ALTER TABLE opds_catalogs ADD COLUMN username TEXT NOT NULL DEFAULT '';
