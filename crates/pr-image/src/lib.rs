//! Decode, scale, tile, encode. CPU-bound and synchronous by design; callers put it
//! on rayon. Hard invariant 3 lives here: we never decode a page bigger than we draw.

use image::{codecs::jpeg::JpegEncoder, imageops, ImageReader, RgbImage};
use std::io::Cursor;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("jpeg decode: {0}")]
    Jpeg(#[from] jpeg_decoder::Error),
    #[error("image: {0}")]
    Image(#[from] image::ImageError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported pixel format {0:?}")]
    PixelFormat(jpeg_decoder::PixelFormat),
}

pub type Result<T> = std::result::Result<T, Error>;

fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// Source dimensions without decoding pixels. Used to build the tile layout.
pub fn probe(bytes: &[u8]) -> Result<(u32, u32)> {
    Ok(ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .into_dimensions()?)
}

/// Decode to at most 2x `target_w` wide, never less than `target_w` unless the
/// source is already smaller.
///
/// JPEG takes the DCT-domain path: the decoder is told to emit at 1/2, 1/4 or 1/8,
/// which lands the result somewhere in `[target_w, 2 * target_w]`. The remainder is
/// deliberately *not* resampled -- see `fit_width`.
#[tracing::instrument(skip(bytes), fields(bytes = bytes.len(), target_w))]
pub fn decode_scaled(bytes: &[u8], target_w: u32) -> Result<RgbImage> {
    let img = if is_jpeg(bytes) {
        match decode_jpeg_scaled(bytes, target_w) {
            Ok(img) => img,
            // CMYK, arithmetic coding, and other oddballs: let `image` handle it.
            Err(Error::PixelFormat(_)) => image::load_from_memory(bytes)?.into_rgb8(),
            Err(e) => return Err(e),
        }
    } else {
        // ponytail: PNG/WebP have no scaled-decode path, so this is a full decode.
        // If tall PNG strips turn up in real libraries, tile them at the container level.
        image::load_from_memory(bytes)?.into_rgb8()
    };
    Ok(fit_width(img, target_w))
}

fn decode_jpeg_scaled(bytes: &[u8], target_w: u32) -> Result<RgbImage> {
    let mut dec = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    dec.read_info()?;
    let info = dec.info().expect("read_info succeeded, so info is present");
    let (sw, sh) = (info.width as u32, info.height as u32);

    // Largest power-of-two reduction that still leaves us at or above the draw size.
    let mut denom = 1u32;
    while denom < 8 && target_w > 0 && sw / (denom * 2) >= target_w {
        denom *= 2;
    }
    let (rw, rh) = ((sw / denom).max(1), (sh / denom).max(1));
    let (aw, ah) = dec.scale(rw as u16, rh as u16)?;

    let pixels = dec.decode()?;
    let fmt = dec
        .info()
        .map(|i| i.pixel_format)
        .unwrap_or(info.pixel_format);
    let (w, h) = (aw as u32, ah as u32);

    match fmt {
        jpeg_decoder::PixelFormat::RGB24 => {
            RgbImage::from_raw(w, h, pixels).ok_or(Error::PixelFormat(fmt))
        }
        jpeg_decoder::PixelFormat::L8 => {
            let mut rgb = Vec::with_capacity(pixels.len() * 3);
            for l in pixels {
                rgb.extend_from_slice(&[l, l, l]);
            }
            RgbImage::from_raw(w, h, rgb).ok_or(Error::PixelFormat(fmt))
        }
        other => Err(Error::PixelFormat(other)),
    }
}

/// Resample only when the decoder overshot by more than 2x -- which, after the DCT
/// ladder, means only non-JPEG sources.
///
/// Inside 2x the compositor scales the tile on the GPU for free, and measurement says
/// software resampling is not close to free: a 1600px page resized to 1200px spends
/// ~60ms in `imageops::resize` on top of a ~16ms decode. Manga scans cluster at
/// 1400-2200px wide, exactly the band where the DCT ladder cannot help and the resize
/// would run on every page. Handing the webview an oversampled tile costs bytes; it
/// also happens to be sharper on HiDPI.
fn fit_width(img: RgbImage, target_w: u32) -> RgbImage {
    if target_w == 0 || img.width() <= target_w.saturating_mul(2) {
        return img;
    }
    let h = ((img.height() as u64 * target_w as u64) / img.width() as u64).max(1) as u32;
    // Triangle, not Lanczos3: Lanczos costs ~4x the time for no visible gain on line art.
    imageops::resize(&img, target_w, h, imageops::FilterType::Triangle)
}

/// Tallest page we hand over whole. Above this, tiling is what stops a 60,000px strip
/// being decoded in one piece (invariant 2), so passthrough must not apply.
pub const MAX_PASSTHROUGH_H: u32 = 4096;

/// Display-space geometry for one page, and the bridge back to whatever size the
/// decoder actually emitted.
///
/// The layout sent to the frontend and the slicing done in the tile cache must agree
/// exactly or the strip shears, so both derive from this one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageGrid {
    /// Display width, never larger than the source.
    pub w: u32,
    /// Display height at that width.
    pub h: u32,
    pub tiles: u32,
    /// Tile height in display space. Equal to `h` on a passthrough page, which is why
    /// such a page reports exactly one tile.
    pub tile_h: u32,
    passthrough: bool,
}

impl PageGrid {
    pub fn new((src_w, src_h): (u32, u32), display_w: u32, tile_h: u32) -> Self {
        let w = display_w.min(src_w);
        let h = ((src_h as u64 * w as u64) / src_w.max(1) as u64).max(1) as u32;

        // Drawn at its own size and short enough to hand over whole: no decode we could
        // perform would change a pixel of it, so don't.
        let passthrough = w == src_w && h <= MAX_PASSTHROUGH_H;
        let tile_h = if passthrough { h } else { tile_h.max(1) };

        Self {
            w,
            h,
            tiles: h.div_ceil(tile_h),
            tile_h,
            passthrough,
        }
    }

    /// True when the source bytes can be served exactly as they sit in the container:
    /// no decode, no re-encode, no cache entry. Costs one read and nothing else.
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// Rows `[y0, y1)` of a decoded image `decoded_h` tall that `tile` covers.
    ///
    /// `decode_scaled` may overshoot the display width by up to 2x, so the grid is
    /// defined in display space and projected onto the decoded pixels here. Boundaries
    /// are computed from absolute display offsets and rounded once, which is what keeps
    /// adjacent tiles sharing an edge instead of leaving a seam.
    pub fn bounds(&self, tile: u32, decoded_h: u32) -> (u32, u32) {
        let (dec, disp) = (decoded_h as u64, self.h.max(1) as u64);
        let project = |y: u64| (y.min(disp) * dec / disp) as u32;
        let y0 = project(tile as u64 * self.tile_h as u64);
        let y1 = project((tile as u64 + 1) * self.tile_h as u64);
        (y0, y1.max(y0))
    }
}

/// Horizontal slice of a decoded page. Clamped, so the last tile of a page is short.
pub fn tile(img: &RgbImage, y: u32, h: u32) -> RgbImage {
    let y = y.min(img.height());
    let h = h.min(img.height() - y);
    imageops::crop_imm(img, 0, y, img.width(), h).to_image()
}

pub fn encode_jpeg(img: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(img.len() / 8);
    JpegEncoder::new_with_quality(&mut out, quality).encode_image(img)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbImage::new(w, h);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]);
        }
        encode_jpeg(&img, 92).unwrap()
    }

    #[test]
    fn scaled_decode_lands_within_2x_of_target_and_keeps_aspect() {
        let src = sample_jpeg(1600, 4000);
        assert_eq!(probe(&src).unwrap(), (1600, 4000));

        for target in [1600, 1200, 800, 400, 100] {
            let img = decode_scaled(&src, target).unwrap();
            let w = img.width();
            assert!(
                (target..=target * 2).contains(&w),
                "target {target} produced width {w}, outside [target, 2*target]"
            );
            let ratio = img.height() as f64 / img.width() as f64;
            assert!(
                (ratio - 2.5).abs() < 0.01,
                "aspect drifted at {target}: {ratio}"
            );
        }
    }

    #[test]
    fn grid_partitions_any_decoded_height_exactly() {
        // Source dims, display width, and decoded heights including a 2x overshoot.
        let cases = [
            ((1600, 2300), 1200, [2300u32, 1725, 1150]),
            ((800, 8000), 1200, [8000, 4000, 2000]),
            ((2400, 3400), 1000, [3400, 1700, 1417]),
            ((900, 900), 900, [900, 450, 1800]),
        ];

        for ((sw, sh), display_w, decoded_heights) in cases {
            let grid = PageGrid::new((sw, sh), display_w, 1024);
            assert!(
                grid.tiles >= 1,
                "{sw}x{sh} at {display_w} produced no tiles"
            );

            for dec_h in decoded_heights {
                let mut end = 0;
                for t in 0..grid.tiles {
                    let (y0, y1) = grid.bounds(t, dec_h);
                    assert_eq!(y0, end, "seam before tile {t} of {sw}x{sh}@{dec_h}");
                    end = y1;
                }
                assert_eq!(end, dec_h, "tiles stop short of {sw}x{sh}@{dec_h}");
            }
        }
    }

    #[test]
    fn passthrough_applies_only_when_nothing_would_change() {
        // Yotsuba: 978px source drawn into a 1200px viewport. Nothing to resample.
        let page = PageGrid::new((978, 1400), 1200, 1024);
        assert!(page.is_passthrough());
        assert_eq!(page.tiles, 1, "a passthrough page is one piece");
        assert_eq!((page.w, page.h), (978, 1400));
        assert_eq!(
            page.bounds(0, 1400),
            (0, 1400),
            "tile 0 must cover the page"
        );

        // Same volume, the printed spread: wider than the viewport, so it gets scaled
        // and must go down the normal path.
        let spread = PageGrid::new((2100, 1400), 1200, 1024);
        assert!(
            !spread.is_passthrough(),
            "a downscaled page cannot pass through"
        );
        assert_eq!(
            (spread.w, spread.h),
            (1200, 800),
            "scaled down to fit the viewport"
        );
        // The distinguishing property is the tile height: a tiled page keeps the global
        // one, so a taller page splits. A passthrough page adopts its own height.
        assert_eq!(spread.tile_h, 1024);
        assert_eq!(PageGrid::new((2100, 4000), 1200, 1024).tiles, 3);

        // A webtoon strip is short-circuit bait: no downscale, but tiling it is the
        // whole point of invariant 2.
        let strip = PageGrid::new((800, 8000), 1200, 1024);
        assert!(!strip.is_passthrough(), "a tall strip must stay tiled");
        assert!(strip.tiles > 1);

        // Exactly at the height limit still passes; one pixel over does not.
        assert!(PageGrid::new((800, MAX_PASSTHROUGH_H), 1200, 1024).is_passthrough());
        assert!(!PageGrid::new((800, MAX_PASSTHROUGH_H + 1), 1200, 1024).is_passthrough());
    }

    #[test]
    fn grid_never_upscales() {
        let grid = PageGrid::new((800, 8000), 1200, 1024);
        assert_eq!(
            (grid.w, grid.h),
            (800, 8000),
            "a narrow page must not be blown up"
        );
        assert_eq!(PageGrid::new((1600, 2400), 800, 1024).h, 1200);
    }

    #[test]
    fn oversized_non_jpeg_is_still_capped() {
        // PNG has no DCT ladder, so `fit_width` is the only thing bounding it.
        let mut png = Vec::new();
        RgbImage::new(5000, 1000)
            .write_with_encoder(image::codecs::png::PngEncoder::new(&mut png))
            .unwrap();
        assert_eq!(decode_scaled(&png, 1000).unwrap().width(), 1000);
        // Inside 2x it comes back untouched rather than resampled.
        assert_eq!(decode_scaled(&png, 4000).unwrap().width(), 5000);
    }

    #[test]
    fn upscale_is_never_attempted() {
        let img = decode_scaled(&sample_jpeg(200, 300), 4000).unwrap();
        assert_eq!((img.width(), img.height()), (200, 300));
    }

    #[test]
    fn tiles_cover_the_page_with_no_gap_or_overrun() {
        let img = decode_scaled(&sample_jpeg(300, 1000), 300).unwrap();
        let th = 256;
        let mut covered = 0;
        for y in (0..img.height()).step_by(th as usize) {
            let t = tile(&img, y, th);
            assert_eq!(t.width(), img.width());
            covered += t.height();
        }
        assert_eq!(covered, img.height());
        assert_eq!(
            tile(&img, 900, 256).height(),
            100,
            "last tile must be clamped"
        );
    }
}
