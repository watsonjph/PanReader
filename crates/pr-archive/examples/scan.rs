//! What the scanner sees in a real library root.
//!
//!   cargo run -p pr-archive --example scan -- <path>

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| "data".into());
    let t = std::time::Instant::now();
    let series = pr_archive::scan::scan_root(std::path::Path::new(&root));
    let elapsed = t.elapsed();

    let chapters: usize = series.iter().map(|s| s.chapters.len()).sum();
    let pages: usize = series
        .iter()
        .flat_map(|s| &s.chapters)
        .map(|c| c.page_count)
        .sum();
    println!(
        "{} series, {chapters} chapters, {pages} pages in {:.1} ms\n",
        series.len(),
        elapsed.as_secs_f64() * 1000.0
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
