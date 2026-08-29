//! Reader geometry and navigation, kept out of the component so it can be tested.
//!
//! Everything here is a pure function of numbers. The component owns the DOM, the
//! network and the state; this file owns the decisions, which is where the bugs were.

/// A wide page must fit this much better turned before turning is worth it.
export const AUTO_ROTATE_GAIN = 1.2;

/// Tile boundaries within a page, in device px. Mirrors `PageGrid::bounds` on the Rust
/// side; if the two disagree, tiles are sliced at offsets the layout did not intend.
export function tileRects(page) {
  const out = [];
  for (let t = 0; t < page.tiles; t++) {
    const top = Math.min(t * page.tile_h, page.h);
    const bottom = Math.min((t + 1) * page.tile_h, page.h);
    out.push({ t, top, bottom });
  }
  return out;
}

/// Scale that brings a `w` x `h` page (CSS px) into a `vw` x `vh` viewport.
export function fitScale(fit, w, h, vw, vh) {
  if (fit === "width") return vw / w;
  if (fit === "height") return vh / h;
  if (fit === "original") return 1;
  return Math.min(vw / w, vh / h);
}

/// The angle a page is actually drawn at.
///
/// Only a page wider than it is tall is ever turned automatically. "Does it fit better"
/// on its own is the wrong question: a portrait page on a landscape window always fits
/// better turned -- 978x1400 in a 1405x939 window gains 43% -- and is then unreadable.
/// Auto-turning exists for one case, a spread too wide for a tall window.
export function turnFor({ rot, rotLock, w, h, fit, vw, vh }) {
  if (rotLock || w <= h) return rot;
  const gain = fitScale(fit, h, w, vw, vh) / fitScale(fit, w, h, vw, vh);
  return gain > AUTO_ROTATE_GAIN ? (rot + 90) % 360 : rot;
}

/// Top of every page in CSS px, padding included.
///
/// Tops stay rounded from absolute device coordinates, so at zero padding the result is
/// what it would be with no padding feature at all and pages still butt together
/// seamlessly. Accumulating rounded per-page heights instead would drift by a pixel and
/// put a seam down a webtoon.
export function pageTops(pages, pad, dpr) {
  return pages.map((p, i) => Math.round(p.y / dpr) + i * pad);
}

/// Total height of the strip in CSS px, padding included.
export function stripHeight(totalH, pageCount, pad, dpr) {
  return Math.round(totalH / dpr) + Math.max(pageCount - 1, 0) * pad;
}

/// Index of the page covering CSS offset `y`.
export function pageAt(tops, y) {
  let lo = 0;
  let hi = tops.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (tops[mid] <= y) lo = mid;
    else hi = mid - 1;
  }
  return Math.max(lo, 0);
}

/// How far a key moves you: +1 forward in reading order, -1 back, 0 for keys the reader
/// does not own.
///
/// In right-to-left the left arrow advances, which is the entire point of the mode.
export function pageStep(key, rtl) {
  switch (key) {
    case "ArrowLeft":
      return rtl ? 1 : -1;
    case "ArrowRight":
      return rtl ? -1 : 1;
    case "ArrowDown":
    case "PageDown":
    case " ":
      return 1;
    case "ArrowUp":
    case "PageUp":
      return -1;
    default:
      return 0;
  }
}

/// Which way a click at `x` moves you, following the reading direction. The middle of
/// the screen does nothing.
export function clickStep(x, width, rtl) {
  const third = width / 3;
  if (x < third) return rtl ? 1 : -1;
  if (x > width - third) return rtl ? -1 : 1;
  return 0;
}
