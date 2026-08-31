-- Where in the page, not just which page.
--
-- A fraction rather than a pixel offset, for the same reason the text reader stores a
-- paragraph and a character rather than a scroll position: the decode width, the
-- downsampling and the page padding all change what a pixel means, and a webtoon page
-- can be eight thousand pixels tall, so landing at the top of the right page is not the
-- same as landing where you left off.
ALTER TABLE positions ADD COLUMN page_frac REAL NOT NULL DEFAULT 0;
