-- updated_at moves from seconds to milliseconds.
--
-- Continue-reading orders by it, and at one-second resolution two chapters of the same
-- series read moments apart tie, so the shelf offers back an arbitrary one of them. The
-- 400 ms save debounce means that is reachable by ordinary flicking, not just by
-- contrivance. Existing rows are scaled so their order survives.
UPDATE positions SET updated_at = updated_at * 1000;
