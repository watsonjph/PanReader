//! Synthesises the Phase 0 fixtures so the spike runs without shipping any content.
//!
//!   cargo run -p pr-app --example make_fixtures --release
//!
//! Writes fixtures/spike.cbz (200 pages) and fixtures/strip/ (~64,000px tall).
//! Both are gitignored. Point PANREADER_CBZ / PANREADER_STRIP at real files instead
//! once you have them; synthetic pages compress unrealistically well.

use image::RgbImage;
use std::fs;
use std::io::Write;
use std::path::Path;

/// Dense, high-frequency content so tile seams and pop-in are visible, and so JPEG
/// cannot cheat its way to an unrealistically small file.
fn page(w: u32, h: u32, offset: u32) -> RgbImage {
    RgbImage::from_fn(w, h, |x, y| {
        let y = y + offset;
        let band: u8 = if (y / 64).is_multiple_of(2) { 235 } else { 30 };
        let rule: u8 = if y % 256 < 3 || x % 256 < 3 { 0 } else { band };
        let tone = ((x * 7 + y * 13) % 97) as u8 / 4;
        image::Rgb([
            rule.saturating_sub(tone),
            rule,
            rule.saturating_sub(tone / 2),
        ])
    })
}

fn main() -> anyhow::Result<()> {
    let root = Path::new("fixtures");
    fs::create_dir_all(root.join("strip"))?;

    let mut zip = zip::ZipWriter::new(fs::File::create(root.join("spike.cbz"))?);
    let opts: zip::write::FileOptions<()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 1..=200u32 {
        zip.start_file(format!("page{i}.jpg"), opts)?;
        zip.write_all(&pr_image::encode_jpeg(&page(1600, 2300, i * 37), 88)?)?;
    }
    zip.finish()?;
    println!("fixtures/spike.cbz: 200 pages at 1600x2300");

    // 8 x 8000px = 64,000px of continuous strip, the shape a manhwa chapter arrives in.
    for i in 1..=8u32 {
        let bytes = pr_image::encode_jpeg(&page(800, 8000, i * 8000), 88)?;
        fs::write(root.join("strip").join(format!("{i:03}.jpg")), bytes)?;
    }
    println!("fixtures/strip: 8 images, 64,000px tall total");
    Ok(())
}
