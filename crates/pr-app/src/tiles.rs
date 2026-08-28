//! Tile cache. The whole Phase 0 question is whether tiles can be produced faster
//! than a fast flick consumes them, so every step here is timed.

use anyhow::Context;
use lru::LruCache;
use parking_lot::Mutex;
use pr_archive::PageSource;
use rayon::prelude::*;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Instant;

/// Display-space tile height. Small enough that a fill is quick, large enough that a
/// 1440p viewport holds ~2 tiles and the DOM stays tiny.
pub const TILE_H: u32 = 1024;

/// Byte ceiling, not a tile count. Measured tiles run 124KB on an unscaled strip to
/// 299KB on a 2x-overshot page, and a count-based bound sized for one is wrong for the
/// other. 96MB leaves room under the 400MB process budget for the decode peak.
const CACHE_BYTES: usize = 96 * 1024 * 1024;

const JPEG_QUALITY: u8 = 82;

/// LRU bounded by total encoded bytes rather than entry count.
struct TileCache {
    lru: LruCache<TileKey, Arc<Vec<u8>>>,
    bytes: usize,
}

impl TileCache {
    fn new() -> Self {
        Self {
            lru: LruCache::unbounded(),
            bytes: 0,
        }
    }

    fn get(&mut self, key: &TileKey) -> Option<Arc<Vec<u8>>> {
        self.lru.get(key).cloned()
    }

    fn put(&mut self, key: TileKey, value: Arc<Vec<u8>>) {
        if let Some(old) = self.lru.put(key, Arc::clone(&value)) {
            self.bytes -= old.len();
        }
        self.bytes += value.len();
        // A single tile larger than the whole budget would evict itself forever, so
        // stop once only one entry is left rather than looping to empty.
        while self.bytes > CACHE_BYTES && self.lru.len() > 1 {
            match self.lru.pop_lru() {
                Some((_, evicted)) => self.bytes -= evicted.len(),
                None => break,
            }
        }
    }
}

/// Bytes ready for the webview, plus the type they are in. Passthrough keeps whatever
/// the container held; everything else is re-encoded JPEG.
pub struct Served {
    pub data: Arc<Vec<u8>>,
    pub mime: &'static str,
}

fn mime_for(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        _ => "image/jpeg",
    }
}

#[derive(Hash, Eq, PartialEq, Clone, Copy, Debug)]
pub struct TileKey {
    pub page: usize,
    pub tile: u32,
    pub w: u32,
}

#[derive(Default)]
pub struct Stats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub page_decodes: AtomicU64,
    pub decode_us: AtomicU64,
    pub encode_us: AtomicU64,
    pub bytes_out: AtomicU64,
    pub passthrough: AtomicU64,
}

#[derive(Serialize, Default)]
pub struct StatsSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub page_decodes: u64,
    pub decode_ms_avg: f64,
    pub encode_ms_avg: f64,
    pub mb_out: f64,
    pub passthrough: u64,
    pub cached_tiles: usize,
    pub cached_mb: f64,
}

#[derive(Serialize, Clone)]
pub struct PageLayout {
    pub index: usize,
    /// Display-space size, i.e. what the webview lays out.
    pub w: u32,
    pub h: u32,
    /// Top edge in the continuous strip.
    pub y: u32,
    pub tiles: u32,
    /// Per page, not global: a passthrough page has one tile as tall as itself.
    pub tile_h: u32,
}

#[derive(Serialize, Clone)]
pub struct Layout {
    pub kind: String,
    pub display_w: u32,
    pub tile_h: u32,
    pub total_h: u32,
    pub pages: Vec<PageLayout>,
}

pub struct Chapter {
    kind: String,
    src: PageSource,
    /// Source dimensions, probed from headers at open. Never a full decode.
    dims: Vec<(u32, u32)>,
    cache: Mutex<TileCache>,
    /// ponytail: one global fill lock. Serialises page decodes, which is what we want
    /// on a 60k-px strip anyway; go per-page if two chapters are ever read at once.
    fill: Mutex<()>,
    pub stats: Stats,
}

impl Chapter {
    #[tracing::instrument(skip(src))]
    pub fn open(kind: &str, src: PageSource) -> anyhow::Result<Self> {
        let t = Instant::now();
        let prefixes = src.read_prefixes(64 * 1024)?;
        let dims = prefixes
            .into_par_iter()
            .enumerate()
            .map(|(i, head)| match pr_image::probe(&head) {
                Ok(d) => Ok(d),
                // Header past the 64KB window (huge EXIF, progressive scan): pay for
                // the full read on the few pages that need it.
                Err(_) => Ok(pr_image::probe(&src.read(i)?)?),
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        tracing::info!(
            pages = dims.len(),
            ms = t.elapsed().as_millis(),
            "probed chapter"
        );

        Ok(Self {
            kind: kind.to_owned(),
            src,
            dims,
            cache: Mutex::new(TileCache::new()),
            fill: Mutex::new(()),
            stats: Stats::default(),
        })
    }

    pub fn layout(&self, display_w: u32) -> Layout {
        let mut y = 0u32;
        let pages = self
            .dims
            .iter()
            .enumerate()
            .map(|(index, &dims)| {
                let grid = pr_image::PageGrid::new(dims, display_w, TILE_H);
                let page = PageLayout {
                    index,
                    w: grid.w,
                    h: grid.h,
                    y,
                    tiles: grid.tiles,
                    tile_h: grid.tile_h,
                };
                y = y.saturating_add(grid.h);
                page
            })
            .collect();
        Layout {
            kind: self.kind.clone(),
            display_w,
            tile_h: TILE_H,
            total_h: y,
            pages,
        }
    }

    /// Bytes for one tile.
    ///
    /// A page that needs no resampling skips the pipeline entirely: read the source and
    /// hand it over. Everything else is a cache hit, or a miss that decodes the page
    /// once and fills all of its tiles, so a scroll pays per page rather than per tile.
    pub fn tile(&self, key: TileKey) -> anyhow::Result<Served> {
        let dims = *self.dims.get(key.page).context("page out of range")?;
        if pr_image::PageGrid::new(dims, key.w, TILE_H).is_passthrough() {
            self.stats.passthrough.fetch_add(1, Relaxed);
            let name = self.src.name(key.page).unwrap_or_default();
            // Deliberately not cached: re-reading costs a fraction of a millisecond and
            // caching would duplicate bytes that are already on disk, crowding out the
            // tiles that actually cost something to produce.
            return Ok(Served {
                data: Arc::new(self.src.read(key.page)?),
                mime: mime_for(&name),
            });
        }
        if let Some(hit) = self.cache.lock().get(&key) {
            self.stats.hits.fetch_add(1, Relaxed);
            return Ok(Served {
                data: hit,
                mime: "image/jpeg",
            });
        }
        self.stats.misses.fetch_add(1, Relaxed);

        let _fill = self.fill.lock();
        // Another thread may have filled this page while we waited for the lock.
        if let Some(hit) = self.cache.lock().get(&key) {
            return Ok(Served {
                data: hit,
                mime: "image/jpeg",
            });
        }
        self.fill_page(key.page, key.w)?;
        self.cache
            .lock()
            .get(&key)
            .map(|data| Served {
                data,
                mime: "image/jpeg",
            })
            .ok_or_else(|| anyhow::anyhow!("tile {key:?} out of range"))
    }

    #[tracing::instrument(skip(self))]
    fn fill_page(&self, page: usize, display_w: u32) -> anyhow::Result<()> {
        let bytes = self.src.read(page)?;

        let t = Instant::now();
        // ponytail: one page decoded whole, at up to 2x the display width. A single
        // monolithic 60k-px image peaks in the hundreds of MB here. If that shows up in
        // the frame-time overlay, the next step is a codec with row-range decode, not a
        // smaller tile.
        let img = pr_image::decode_scaled(&bytes, display_w)?;
        self.stats
            .decode_us
            .fetch_add(t.elapsed().as_micros() as u64, Relaxed);
        self.stats.page_decodes.fetch_add(1, Relaxed);

        let dims = *self.dims.get(page).context("page out of range")?;
        let grid = pr_image::PageGrid::new(dims, display_w, TILE_H);
        let decoded_h = img.height();

        let t = Instant::now();
        let encoded = (0..grid.tiles)
            .into_par_iter()
            .map(|i| {
                let (y0, y1) = grid.bounds(i, decoded_h);
                let slice = pr_image::tile(&img, y0, y1 - y0);
                Ok((i, Arc::new(pr_image::encode_jpeg(&slice, JPEG_QUALITY)?)))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.stats
            .encode_us
            .fetch_add(t.elapsed().as_micros() as u64, Relaxed);

        let mut cache = self.cache.lock();
        for (tile, data) in encoded {
            self.stats.bytes_out.fetch_add(data.len() as u64, Relaxed);
            cache.put(
                TileKey {
                    page,
                    tile,
                    w: display_w,
                },
                data,
            );
        }
        Ok(())
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let s = &self.stats;
        let decodes = s.page_decodes.load(Relaxed).max(1) as f64;
        let cache = self.cache.lock();
        StatsSnapshot {
            hits: s.hits.load(Relaxed),
            misses: s.misses.load(Relaxed),
            page_decodes: s.page_decodes.load(Relaxed),
            decode_ms_avg: s.decode_us.load(Relaxed) as f64 / 1000.0 / decodes,
            encode_ms_avg: s.encode_us.load(Relaxed) as f64 / 1000.0 / decodes,
            mb_out: s.bytes_out.load(Relaxed) as f64 / 1_048_576.0,
            passthrough: s.passthrough.load(Relaxed),
            cached_tiles: cache.lru.len(),
            cached_mb: cache.bytes as f64 / 1_048_576.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(tile: u32) -> TileKey {
        TileKey {
            page: 0,
            tile,
            w: 1200,
        }
    }

    #[test]
    fn cache_evicts_by_bytes_and_keeps_the_newest() {
        let mut cache = TileCache::new();
        let big = CACHE_BYTES / 4 + 1;
        for i in 0..8 {
            cache.put(key(i), Arc::new(vec![0u8; big]));
        }
        assert!(
            cache.bytes <= CACHE_BYTES,
            "over budget at {} bytes",
            cache.bytes
        );
        assert!(cache.get(&key(7)).is_some(), "most recent tile was evicted");
        assert!(
            cache.get(&key(0)).is_none(),
            "oldest tile survived eviction"
        );
    }

    #[test]
    fn replacing_a_key_does_not_double_count() {
        let mut cache = TileCache::new();
        cache.put(key(0), Arc::new(vec![0u8; 1000]));
        cache.put(key(0), Arc::new(vec![0u8; 400]));
        assert_eq!(cache.bytes, 400);
        assert_eq!(cache.lru.len(), 1);
    }

    #[test]
    fn a_tile_larger_than_the_budget_is_still_served() {
        let mut cache = TileCache::new();
        cache.put(key(0), Arc::new(vec![0u8; CACHE_BYTES * 2]));
        assert!(
            cache.get(&key(0)).is_some(),
            "oversized tile evicted itself"
        );
    }
}
