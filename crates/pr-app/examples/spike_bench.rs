//! Phase 0 measurement. Answers the roadmap's numbers for everything below the
//! webview; the sustained-fps figure still needs a human watching the HUD.
//!
//!   cargo run -p pr-app --example spike_bench --release
//!
//! Reports the two things that decide the phase: how long a chapter takes to reach
//! its first painted tile, and how many display pixels per second of tile the
//! pipeline can produce. A 4000px/s flick cannot outrun the filler if the second
//! number is comfortably above it.

use pr_archive::PageSource;
use rayon::prelude::*;
use std::path::Path;
use std::time::{Duration, Instant};

const DISPLAY_W: u32 = 1200;
const TILE_H: u32 = 1024;
const QUALITY: u8 = 82;
/// Enough pages to average out one unlucky decode without waiting on 200 of them.
const SWEEP_PAGES: usize = 16;

/// First few and last page names, to eyeball that natural sort did the right thing.
fn preview(src: &PageSource) -> String {
    let base = |i: usize| {
        src.name(i)
            .unwrap_or_default()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_string()
    };
    let last = src.len() - 1;
    format!("{}, {}, {} ... {}", base(0), base(1), base(2), base(last))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

struct Sweep {
    display_px: u64,
    /// Pages served straight from the container, and pages that cost real work.
    passthrough: usize,
    decoded: usize,
    tiles: usize,
    bytes: usize,
    read: Duration,
    decode: Duration,
    encode_all: Duration,
    encode_one: Duration,
}

fn sweep(src: &PageSource, dims: &[(u32, u32)], pages: usize) -> anyhow::Result<Sweep> {
    let mut s = Sweep {
        display_px: 0,
        passthrough: 0,
        decoded: 0,
        tiles: 0,
        bytes: 0,
        read: Duration::ZERO,
        decode: Duration::ZERO,
        encode_all: Duration::ZERO,
        encode_one: Duration::ZERO,
    };

    for (i, &page_dims) in dims.iter().enumerate().take(pages) {
        let t = Instant::now();
        let raw = src.read(i)?;
        s.read += t.elapsed();

        // The flick consumes display pixels, so production is measured in those, not
        // in whatever the decoder happened to emit.
        let grid = pr_image::PageGrid::new(page_dims, DISPLAY_W, TILE_H);
        s.display_px += grid.h as u64;

        // Mirror what the app does: a page needing no resampling never reaches the codec.
        if grid.is_passthrough() {
            s.passthrough += 1;
            s.tiles += 1;
            s.bytes += raw.len();
            continue;
        }
        s.decoded += 1;

        let t = Instant::now();
        let img = pr_image::decode_scaled(&raw, DISPLAY_W)?;
        s.decode += t.elapsed();
        let slice = |k: u32| {
            let (y0, y1) = grid.bounds(k, img.height());
            pr_image::tile(&img, y0, y1 - y0)
        };

        // What the viewer actually waits for on a jump: one tile, not the page.
        let t = Instant::now();
        let first = pr_image::encode_jpeg(&slice(0), QUALITY)?;
        s.encode_one += t.elapsed();

        let t = Instant::now();
        let encoded = (0..grid.tiles)
            .into_par_iter()
            .map(|k| pr_image::encode_jpeg(&slice(k), QUALITY))
            .collect::<Result<Vec<_>, _>>()?;
        s.encode_all += t.elapsed();

        s.tiles += encoded.len();
        s.bytes += encoded.iter().map(Vec::len).sum::<usize>();
        drop(first);
    }
    Ok(s)
}

fn report(label: &str, path: &Path) -> anyhow::Result<()> {
    println!("\n=== {label} ({}) ===", path.display());

    let t = Instant::now();
    let src = PageSource::open(path)?;
    let open = t.elapsed();

    let t = Instant::now();
    let prefixes = src.read_prefixes(64 * 1024)?;
    let dims = prefixes
        .par_iter()
        .map(|head| pr_image::probe(head))
        .collect::<Result<Vec<_>, _>>()?;
    let probe = t.elapsed();

    let pages = SWEEP_PAGES.min(src.len());
    let s = sweep(&src, &dims, pages)?;

    let per_read = |d: Duration| ms(d) / pages as f64;
    // Decode and encode averages only make sense over the pages that actually ran them.
    let per_page = |d: Duration| ms(d) / s.decoded.max(1) as f64;
    // Page 0 decides its own first paint: a passthrough page never reaches the codec.
    let first_decode = if pr_image::PageGrid::new(dims[0], DISPLAY_W, TILE_H).is_passthrough() {
        0.0
    } else {
        per_page(s.decode)
    };
    let first_paint = ms(open) + ms(probe) + per_read(s.read) + first_decode;
    // Tile production rate, the number a fast flick has to beat.
    let fill = s.read + s.decode + s.encode_all;
    let px_per_s = s.display_px as f64 / fill.as_secs_f64();

    println!("  pages in chapter      {}", src.len());
    println!("  source page 0         {}x{}", dims[0].0, dims[0].1);
    println!("  reading order         {}", preview(&src));

    // Wide pages are printed spreads and Phase 1 shows them alone. Flag the widest so
    // the aspect threshold gets picked from real scans rather than guessed.
    let median = {
        let mut r: Vec<f64> = dims.iter().map(|&(w, h)| w as f64 / h as f64).collect();
        r.sort_by(|a, b| a.partial_cmp(b).unwrap());
        r[r.len() / 2]
    };
    let (widest, ratio) = dims.iter().enumerate().fold((0, 0.0), |acc, (i, &(w, h))| {
        let r = w as f64 / h as f64;
        if r > acc.1 {
            (i, r)
        } else {
            acc
        }
    });
    println!(
        "  widest page           #{widest} {:.2} vs median {median:.2} ({})",
        ratio,
        src.name(widest).unwrap_or_default()
    );
    println!("  open + probe all      {:.1} ms", ms(open) + ms(probe));
    println!("  --- per page, {pages} sampled, decoded to {DISPLAY_W}px wide ---");
    println!("  read from container   {:.1} ms", per_read(s.read));
    println!(
        "  passthrough / decoded {} / {}  (of {pages} sampled)",
        s.passthrough, s.decoded
    );
    println!("  scaled decode         {:.1} ms", per_page(s.decode));
    println!("  encode 1 tile         {:.1} ms", per_page(s.encode_one));
    println!(
        "  encode all tiles      {:.1} ms  ({:.1} tiles/page, rayon)",
        per_page(s.encode_all),
        s.tiles as f64 / pages as f64
    );
    println!(
        "  tile size             {:.0} KB avg",
        s.bytes as f64 / s.tiles as f64 / 1024.0
    );
    println!(
        "  chapter tile set      {:.1} MB (all {} pages, extrapolated)",
        (s.bytes as f64 / pages as f64 * src.len() as f64) / 1_048_576.0,
        src.len()
    );
    if s.decoded == 0 {
        println!("  (every sampled page was served straight from the container)");
    }
    println!("  --- verdict ---");
    println!(
        "  first painted tile    {:.0} ms   [budget 400]  {}",
        first_paint,
        if first_paint < 400.0 { "PASS" } else { "FAIL" }
    );
    println!(
        "  tile production       {:.0} px/s [flick 4000] {}",
        px_per_s,
        if px_per_s > 4000.0 { "PASS" } else { "FAIL" }
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1));
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build_global()?;
    println!("rayon threads: {}", threads.max(1));

    // Any paths given on the command line are measured instead of the fixtures, so a
    // real library can be checked without regenerating anything.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        report("paged CBZ", Path::new("fixtures/spike.cbz"))?;
        report("webtoon strip", Path::new("fixtures/strip"))?;
    } else {
        for arg in &args {
            report(arg, Path::new(arg))?;
        }
    }
    Ok(())
}
