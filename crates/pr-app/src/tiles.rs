//! Tile cache. The whole Phase 0 question is whether tiles can be produced faster
//! than a fast flick consumes them, so every step here is timed.

use lru::LruCache;
use parking_lot::Mutex;
use pr_archive::PageSource;
use rayon::prelude::*;
use serde::Serialize;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Instant;

/// Display-space tile height. Small enough that a fill is quick, large enough that a
/// 1440p viewport holds ~2 tiles and the DOM stays tiny.
pub const TILE_H: u32 = 1024;

/// ponytail: count-based LRU, ~250KB/tile at q80 => roughly 64MB. Switch to a byte
/// ceiling if real pages turn out to vary more than 2x in encoded size.
const CACHE_TILES: usize = 256;

const JPEG_QUALITY: u8 = 82;

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
}

#[derive(Serialize, Default)]
pub struct StatsSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub page_decodes: u64,
    pub decode_ms_avg: f64,
    pub encode_ms_avg: f64,
    pub mb_out: f64,
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
    cache: Mutex<LruCache<TileKey, Arc<Vec<u8>>>>,
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
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_TILES).unwrap())),
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
            .map(|(index, &(sw, sh))| {
                let w = display_w.min(sw);
                let h = ((sh as u64 * w as u64) / sw.max(1) as u64).max(1) as u32;
                let page = PageLayout {
                    index,
                    w,
                    h,
                    y,
                    tiles: h.div_ceil(TILE_H),
                };
                y = y.saturating_add(h);
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

    /// Encoded JPEG bytes for one tile. Cheap on a hit; on a miss it decodes the whole
    /// page once and fills every tile of that page, so a scroll pays per page, not per tile.
    pub fn tile(&self, key: TileKey) -> anyhow::Result<Arc<Vec<u8>>> {
        if let Some(hit) = self.cache.lock().get(&key).cloned() {
            self.stats.hits.fetch_add(1, Relaxed);
            return Ok(hit);
        }
        self.stats.misses.fetch_add(1, Relaxed);

        let _fill = self.fill.lock();
        // Another thread may have filled this page while we waited for the lock.
        if let Some(hit) = self.cache.lock().get(&key).cloned() {
            return Ok(hit);
        }
        self.fill_page(key.page, key.w)?;
        self.cache
            .lock()
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("tile {key:?} out of range"))
    }

    #[tracing::instrument(skip(self))]
    fn fill_page(&self, page: usize, display_w: u32) -> anyhow::Result<()> {
        let bytes = self.src.read(page)?;

        let t = Instant::now();
        // ponytail: one page decoded whole at display size. A single monolithic 60k-px
        // image peaks around 150MB here. If that shows up in the frame-time overlay,
        // the next step is a codec with row-range decode, not a smaller tile.
        let img = pr_image::decode_scaled(&bytes, display_w)?;
        self.stats
            .decode_us
            .fetch_add(t.elapsed().as_micros() as u64, Relaxed);
        self.stats.page_decodes.fetch_add(1, Relaxed);

        let t = Instant::now();
        let encoded = (0..img.height().div_ceil(TILE_H))
            .into_par_iter()
            .map(|i| {
                let slice = pr_image::tile(&img, i * TILE_H, TILE_H);
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
            cached_tiles: cache.len(),
            cached_mb: cache.iter().map(|(_, v)| v.len()).sum::<usize>() as f64 / 1_048_576.0,
        }
    }
}
