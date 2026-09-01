use super::*;

/// A small library, written straight through the transaction handle. Going via
/// `pr_archive` would mean touching the filesystem to prove something about JSON.
fn library() -> pr_db::Db {
    let mut db = pr_db::Db::open_memory().unwrap();
    {
        let tx = db.transaction().unwrap();
        tx.execute_batch(
            "INSERT INTO categories (name, sort, reading_mode)
                 VALUES ('Reading', 0, NULL), ('Webtoons', 1, 'webtoon');
             INSERT INTO library_roots (path) VALUES ('D:/manga');
             INSERT INTO opds_catalogs (url, name) VALUES ('https://example/opds', 'Demo');

             INSERT INTO series (id, source, source_id, title, author, kind, reading_mode)
                 VALUES (1, 'local', 'D:/manga/Yotsuba', 'Yotsuba&!', 'Azuma', 'image', 'rtl');
             INSERT INTO series_categories VALUES (1, 1);

             INSERT INTO chapters (id, series_id, source_id, title, number, page_count, path)
                 VALUES (1, 1, 'blake3:aaa', 'Chapter 1', 1, 24, 'D:/manga/Yotsuba/c1.cbz'),
                        (2, 1, 'blake3:bbb', 'Chapter 2', 2, 20, 'D:/manga/Yotsuba/c2.cbz');

             INSERT INTO positions (chapter_id, page, page_frac, completed, updated_at)
                 VALUES (1, 23, 0.0, 1, 1000), (2, 7, 0.5, 0, 2000);

             INSERT INTO bookmarks (chapter_id, page, page_frac, note, created_at)
                 VALUES (2, 7, 0.5, 'the cicada page', 500);

             INSERT INTO history (chapter_id, started_at, ended_at, pages, last_page)
                 VALUES (1, 100, 900, 24, 23), (2, 1000, 2000, 8, 7);",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    db
}

/// Two backups are the same backup if they say the same thing. `exported_at` is a
/// clock reading, not a fact about the library.
fn same(a: &Backup, b: &Backup) -> bool {
    let mut a = serde_json::to_value(a).unwrap();
    let mut b = serde_json::to_value(b).unwrap();
    for v in [&mut a, &mut b] {
        v["exported_at"] = serde_json::Value::Null;
    }
    a == b
}

#[test]
fn a_backup_restores_into_an_empty_library_and_says_the_same_thing() {
    let mut source = library();
    let taken = export(&mut source).unwrap();
    let bytes = write(&taken).unwrap();

    // Through the file format, not just the struct: the gzip and the JSON are the part
    // that has to survive a version change.
    let reread = read(&bytes).unwrap();
    let mut fresh = pr_db::Db::open_memory().unwrap();
    let report = restore(&mut fresh, &reread, true).unwrap();

    assert_eq!(report.series_added, 1);
    assert_eq!(report.chapters_added, 2);
    assert_eq!(report.positions_advanced, 2);
    assert_eq!(report.bookmarks_added, 1);
    assert_eq!(report.sessions_added, 2);
    assert_eq!(report.categories_added, 2);

    let after = export(&mut fresh).unwrap();
    assert!(same(&taken, &after), "round trip lost something");
}

#[test]
fn restoring_the_same_file_twice_changes_nothing() {
    let mut source = library();
    let taken = export(&mut source).unwrap();

    let mut fresh = pr_db::Db::open_memory().unwrap();
    restore(&mut fresh, &taken, true).unwrap();
    let again = restore(&mut fresh, &taken, true).unwrap();

    assert_eq!(
        again,
        Report {
            series_matched: 1,
            chapters_matched: 2,
            // Same position on both sides is not further along, so nothing moves.
            positions_kept: 2,
            ..Default::default()
        },
        "a second restore must be a no-op"
    );
}

/// The property that makes this a merge rather than a wipe.
#[test]
fn the_further_along_position_wins_whichever_side_it_is_on() {
    let mut source = library();
    let taken = export(&mut source).unwrap();

    // The same library, but chapter 2 read further here and chapter 1 not finished.
    let mut here = library();
    {
        let tx = here.transaction().unwrap();
        tx.execute_batch(
            "UPDATE positions SET page = 19, completed = 0, updated_at = 50 WHERE chapter_id = 2;
             UPDATE positions SET page = 3, completed = 0, updated_at = 9999 WHERE chapter_id = 1;",
        )
        .unwrap();
        tx.commit().unwrap();
    }

    let report = restore(&mut here, &taken, true).unwrap();
    assert_eq!(report.positions_advanced, 1, "only chapter 1 moves");
    assert_eq!(report.positions_kept, 1);

    let after = export(&mut here).unwrap();
    let chapters = &after.series[0].chapters;
    // Finished beats a page number, even though this side was written far later.
    assert!(chapters[0].completed, "the finished chapter stays finished");
    // And a higher page beats the backup's lower one.
    assert_eq!(chapters[1].page, 19, "local progress is not rolled back");
}

/// A dry run is the same code path with the transaction rolled back, so what it reports
/// is what a restore would do -- not what a second, simpler function guesses.
#[test]
fn a_dry_run_reports_the_same_counts_and_writes_nothing() {
    let mut source = library();
    let taken = export(&mut source).unwrap();

    let mut fresh = pr_db::Db::open_memory().unwrap();
    let planned = restore(&mut fresh, &taken, false).unwrap();
    assert!(
        export(&mut fresh).unwrap().series.is_empty(),
        "a dry run wrote to the database"
    );

    let done = restore(&mut fresh, &taken, true).unwrap();
    assert_eq!(planned, done);
}

/// The unlinked case, in our own format: progress for a chapter this machine does not
/// have. It is restored and it is unreadable, which is the honest pair.
#[test]
fn a_chapter_that_is_not_on_this_disk_keeps_its_progress() {
    let mut source = library();
    let taken = export(&mut source).unwrap();

    let mut fresh = pr_db::Db::open_memory().unwrap();
    restore(&mut fresh, &taken, true).unwrap();

    let series = fresh.library().unwrap();
    let chapters = fresh.chapters(series[0].id).unwrap();
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[1].page, 7, "the position came back");
    assert_eq!(
        chapters[1].path, "",
        "and it has no path until a scan finds the file"
    );
}

#[test]
fn a_backup_from_a_newer_build_is_refused_rather_than_half_read() {
    let mut source = library();
    let mut taken = export(&mut source).unwrap();
    taken.schema = SCHEMA + 1;

    match read(&write(&taken).unwrap()) {
        Err(Error::FromTheFuture { found, known }) => {
            assert_eq!((found, known), (SCHEMA + 1, SCHEMA));
        }
        other => panic!("expected FromTheFuture, got {other:?}"),
    }
}

/// Someone will gunzip the file to look inside it and then try to restore what they are
/// holding. That should work.
#[test]
fn a_gunzipped_backup_still_reads() {
    let mut source = library();
    let taken = export(&mut source).unwrap();
    let plain = serde_json::to_vec(&taken).unwrap();
    assert!(same(&taken, &read(&plain).unwrap()));
}

#[test]
fn further_along_prefers_finished_then_page_then_offset_then_time() {
    // (completed, page, frac, updated_at)
    assert!(further((true, 0, 0.0, 0), (false, 99, 0.9, 999)));
    assert!(!further((false, 99, 0.9, 999), (true, 0, 0.0, 0)));
    assert!(further((false, 10, 0.0, 0), (false, 9, 0.9, 999)));
    assert!(further((false, 10, 0.5, 0), (false, 10, 0.4, 999)));
    assert!(further((false, 10, 0.5, 2), (false, 10, 0.5, 1)));
    assert!(!further((false, 10, 0.5, 1), (false, 10, 0.5, 1)));
}

/// Retention keeps the newest and never leaves the directory empty.
#[test]
fn automatic_backups_rotate_and_do_not_run_twice_in_a_day() {
    let dir = std::env::temp_dir().join("pr_sync_rotate_test");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = library();

    // Three older files, plus the one this call writes.
    std::fs::create_dir_all(&dir).unwrap();
    for stamp in [1_000, 2_000, 3_000] {
        std::fs::write(dir.join(format!("panreader-{stamp}.{EXTENSION}")), b"old").unwrap();
    }
    // Something that is not ours, to prove the sweep does not tidy the directory.
    std::fs::write(dir.join("notes.txt"), b"hello").unwrap();

    let written = automatic(&mut db, &dir, 2).unwrap().expect("took one");
    let after = kept(&dir);
    assert_eq!(after.len(), 2, "pruned to keep");
    assert_eq!(after[0].taken_at, written.taken_at, "newest first");
    assert!(dir.join("notes.txt").exists(), "left other files alone");

    assert!(
        automatic(&mut db, &dir, 2).unwrap().is_none(),
        "already have one from today"
    );

    // And what it wrote is a real backup, not just a file of the right size.
    let restored = read(&std::fs::read(&written.path).unwrap()).unwrap();
    assert_eq!(restored.series.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}
