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

/// Decode to exactly `target_w` wide (or source width, whichever is smaller).
///
/// JPEG takes the DCT-domain path: the decoder is told to emit at 1/2, 1/4 or 1/8
/// and only the remaining <2x is resampled. That is the difference between 40ms and
/// 400ms on a 60,000px strip.
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

fn fit_width(img: RgbImage, target_w: u32) -> RgbImage {
    if target_w == 0 || img.width() <= target_w {
        return img;
    }
    let h = ((img.height() as u64 * target_w as u64) / img.width() as u64).max(1) as u32;
    // Triangle, not Lanczos3: after DCT reduction we are resampling by under 2x,
    // where Lanczos costs ~4x the time for no visible gain on line art.
    imageops::resize(&img, target_w, h, imageops::FilterType::Triangle)
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
    fn scaled_decode_hits_target_width_and_keeps_aspect() {
        let src = sample_jpeg(1600, 4000);
        assert_eq!(probe(&src).unwrap(), (1600, 4000));

        for target in [1600, 1200, 800, 400, 100] {
            let img = decode_scaled(&src, target).unwrap();
            assert_eq!(img.width(), target.min(1600), "target {target}");
            let ratio = img.height() as f64 / img.width() as f64;
            assert!(
                (ratio - 2.5).abs() < 0.01,
                "aspect drifted at {target}: {ratio}"
            );
        }
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
