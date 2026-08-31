//! What the scanner sees in a real library root.
//!
//!   cargo run -p pr-archive --example scan -- <path>

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| "data".into());
    let t = std::time::Instant::now();
    let series =
        pr_archive::scan::scan_root(std::path::Path::new(&root), &pr_archive::scan::Known::new());
    let elapsed = t.elapsed();

    // What a rescan costs, which is the number S1 is gated on. Feeding the first
    // scan's own results back in is exactly what the app does from the database.
    let known: pr_archive::scan::Known = series
        .iter()
        .flat_map(|s| &s.chapters)
        .map(|c| {
            (
                c.path.clone(),
                pr_archive::scan::Cached {
                    mtime: c.mtime,
                    size: c.size,
                    identity: c.identity.clone(),
                    page_count: c.page_count,
                    title: c.title.clone(),
                    number: c.number,
                },
            )
        })
        .collect();
    let t = std::time::Instant::now();
    let again = pr_archive::scan::scan_root(std::path::Path::new(&root), &known);
    let rescan = t.elapsed();
    assert_eq!(
        again.len(),
        series.len(),
        "a rescan must see the same library"
    );

    let chapters: usize = series.iter().map(|s| s.chapters.len()).sum();
    let pages: usize = series
        .iter()
        .flat_map(|s| &s.chapters)
        .map(|c| c.page_count)
        .sum();
    println!(
        "{} series, {chapters} chapters, {pages} pages\n  cold   {:>8.1} ms\n  rescan {:>8.1} ms\n",
        series.len(),
        elapsed.as_secs_f64() * 1000.0,
        rescan.as_secs_f64() * 1000.0
    );

    for s in series.iter().take(10) {
        println!("{}  ({} chapters)", s.title, s.chapters.len());
        for c in s.chapters.iter().take(3) {
            println!(
                "    {:<28} n={:<7} {:>4} pages  {}",
                c.title,
                c.number
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "-".into()),
                c.page_count,
                c.identity
            );
        }
    }
}
