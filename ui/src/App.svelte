<script>
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import {
    clickStep,
    fitScale,
    pageAt,
    pageStep,
    pageTops,
    stripHeight,
    tileRects,
    turnFor,
  } from "./reader.js";

  // Kept mounted above and below the viewport in strip mode. Tuned so a 4000px/s flick
  // still has half a second of runway before it outruns the tile filler.
  const OVERSCAN = 2400;
  const MAX_DEVICE_W = 1600;
  const FITS = ["page", "width", "height", "original"];
  const MODES = ["rtl", "ltr", "webtoon"];
  /// Fraction of the natural display width to actually decode. Lower trades sharpness
  /// for memory and bandwidth.
  const SAMPLES = [1, 0.75, 0.5];
  /// Gap between pages, in CSS px. Zero is a seamless webtoon; anything above it is
  /// what Mihon calls CONTINUOUS_VERTICAL. One setting beats two modes.
  const PADS = [0, 8, 16, 32];

  let kind = $state("cbz");
  let error = $state(null);
  let hud = $state({ fps: 0, worst: 0, dropped: 0, mounted: 0, firstPaint: null });
  let rust = $state({});
  let autoscroll = $state(false);

  let detected = $state(null); // what the backend worked out, and why
  let mode = $state("rtl"); // effective mode; a manual pick overrides the detection
  let overridden = $state(false);
  let fit = $state("page");
  let sample = $state(1);
  let pad = $state(0);
  let rot = $state(0);
  let rotLock = $state(false);
  let page = $state(0);
  let pageCount = $state(0);
  let epoch = $state(0); // bumped on load so the paged view recomputes
  let vw = $state(1200);
  let vh = $state(800);

  let scroller = $state(null);
  let canvas = $state(null);

  // Scroll-path state, deliberately outside $state: the strip canvas is imperative.
  let layout = null;
  let base = "";
  let dpr = 1;
  let live = new Map();
  let dirty = true;
  let openedAt = 0;
  let resizeTimer = 0;
  let warmTimer = 0;
  let tops = []; // CSS-px top of each page, padding included

  const paged = $derived(mode !== "webtoon");
  const unreadable = $derived.by(() => {
    void epoch;
    return layout ? layout.pages.filter((p) => !p.readable).length : 0;
  });
  const rtl = $derived(mode === "rtl");

  // Device px -> CSS px, rounded on absolute coordinates so neighbouring tiles share an
  // edge exactly. Rounding each tile's height independently is what puts seams down a
  // webtoon.
  const css = (deviceY) => Math.round(deviceY / dpr);

  const tileUrl = (index, t) =>
    `${base}/t/${kind}/${index}/${t}/${layout.display_w}`;

  function rebuildTops() {
    tops = layout ? pageTops(layout.pages, pad, dpr) : [];
  }

  function canvasHeight() {
    return layout ? stripHeight(layout.total_h, layout.pages.length, pad, dpr) : 0;
  }

  // The paged view. Rebuilt only when something it depends on changes, which for a page
  // turn is once -- there is no scroll path here to keep off the main thread.
  const view = $derived.by(() => {
    void epoch, page, fit, vw, vh, mode, rot, rotLock, pad, sample;
    if (!layout || mode === "webtoon") return null;
    const p = layout.pages[page];
    if (!p) return null;

    const naturalW = p.w / dpr;
    const naturalH = p.h / dpr;

    const turn = turnFor({
      rot,
      rotLock,
      w: naturalW,
      h: naturalH,
      fit,
      vw,
      vh,
    });
    const quarter = turn === 90 || turn === 270;

    // Turned, the page is measured against the viewport with its axes swapped.
    const scale = quarter
      ? fitScale(fit, naturalH, naturalW, vw, vh)
      : fitScale(fit, naturalW, naturalH, vw, vh);
    const drawW = Math.round(naturalW * scale);
    const tiles = tileRects(p).map(({ t, top, bottom }) => {
      const y0 = Math.round((top / dpr) * scale);
      const y1 = Math.round((bottom / dpr) * scale);
      return { t, url: tileUrl(p.index, t), top: y0, h: y1 - y0 };
    });
    const drawH = tiles.length ? tiles[tiles.length - 1].top + tiles[tiles.length - 1].h : 0;

    // The box is the page's footprint after turning; the page itself is drawn unturned
    // inside it and rotated about its centre.
    return {
      p,
      drawW,
      drawH,
      tiles,
      turn,
      boxW: quarter ? drawH : drawW,
      boxH: quarter ? drawW : drawH,
    };
  });

  async function load(next, keepPage = false) {
    const wanted = keepPage ? page : 0;
    // Strip mode keeps its place by page, not by pixel: a reload can change the width
    // we decode at and the padding between pages, so the old offset means nothing.
    const wantedScroll = keepPage && layout && !paged ? pageAt(tops, scroller.scrollTop) : 0;
    kind = next;
    error = null;
    hud.firstPaint = null;
    for (const el of live.values()) el.remove();
    live.clear();

    dpr = window.devicePixelRatio || 1;
    // Downsampling shrinks what we ask the backend to produce. Worth knowing: below 1
    // it also takes pages out of the passthrough path, because a page is only passed
    // through untouched when it is drawn at its own size. Less memory, more CPU.
    const displayW = Math.round(
      Math.min(scroller.clientWidth, MAX_DEVICE_W / dpr) * dpr * sample,
    );
    openedAt = performance.now();
    try {
      base ||= await invoke("tile_base");
      layout = await invoke("open_chapter", { kind, displayW });
    } catch (e) {
      error = String(e);
      layout = null;
      return;
    }

    detected = layout.reading;
    if (!overridden) mode = layout.reading.mode;
    pageCount = layout.pages.length;
    page = Math.min(wanted, pageCount - 1);
    epoch++;

    layout.maxW = layout.pages.reduce((m, p) => Math.max(m, p.w), 1);
    rebuildTops();
    canvas.style.width = css(layout.maxW) + "px";
    canvas.style.height = canvasHeight() + "px";
    scroller.scrollTop = tops[wantedScroll] ?? 0;
    dirty = true;
    if (paged) prefetch();
  }

  /// Warm the pages either side of this one. The browser caches by URL and our tiles are
  /// immutable, so touching the URL is the whole prefetch.
  function prefetch() {
    if (!layout) return;
    for (const d of [1, 2, 3, -1]) {
      const p = layout.pages[page + d];
      if (!p) continue;
      for (const { t } of tileRects(p)) new Image().src = tileUrl(p.index, t);
    }
  }

  /// Re-aim the background fill at the current page. Debounced: flicking through
  /// twenty pages should retarget once, not twenty times.
  function reWarm() {
    if (!layout) return;
    clearTimeout(warmTimer);
    warmTimer = setTimeout(() => {
      invoke("warm", { kind, page, displayW: layout.display_w }).catch(() => {});
    }, 400);
  }

  function go(delta) {
    if (!layout) return;
    const next = Math.min(Math.max(page + delta, 0), layout.pages.length - 1);
    if (next !== page) {
      page = next;
      prefetch();
      reWarm();
    }
  }

  function mount(p, t) {
    // Tile height is per page: a passthrough page is one tile as tall as itself. Offsets
    // stay rounded from absolute device coordinates so tiles inside a page never gap;
    // the page's own padded top is added afterwards.
    const top = p.y + t * p.tile_h;
    const bottom = Math.min(p.y + p.h, top + p.tile_h);
    const y = tops[p.index] + css(top) - css(p.y);
    const img = document.createElement("img");
    img.src = tileUrl(p.index, t);
    img.decoding = "async";
    img.style.cssText =
      `position:absolute;left:${css(layout.maxW - p.w) / 2}px;top:${y}px;` +
      `width:${css(p.w)}px;height:${css(bottom) - css(top)}px;display:block`;
    if (hud.firstPaint === null) {
      img.addEventListener(
        "load",
        () => {
          hud.firstPaint ??= Math.round(performance.now() - openedAt);
        },
        { once: true },
      );
    }
    canvas.appendChild(img);
    return img;
  }

  function ensureTiles() {
    if (!layout || paged) return;
    const top = scroller.scrollTop - OVERSCAN;
    const bottom = scroller.scrollTop + scroller.clientHeight + OVERSCAN;
    const want = new Set();

    for (let i = pageAt(tops, Math.max(top, 0)); i < layout.pages.length; i++) {
      const p = layout.pages[i];
      if (tops[i] > bottom) break;
      // Pages carry at most a handful of tiles, so testing each for overlap is cheaper
      // to read than the divisions it replaces, and exact at the page's last short tile.
      for (const { t, top: a, bottom: b } of tileRects(p)) {
        const y0 = tops[i] + css(p.y + a) - css(p.y);
        const y1 = tops[i] + css(p.y + b) - css(p.y);
        if (y1 < top || y0 > bottom) continue;
        const key = `${p.index}:${t}`;
        want.add(key);
        if (!live.has(key)) live.set(key, mount(p, t));
      }
    }
    for (const [key, el] of live) {
      if (!want.has(key)) {
        el.remove();
        live.delete(key);
      }
    }
    hud.mounted = live.size;
  }

  // One rAF loop drives both the frame-time measurement and the strip's tile window, so
  // scrolling never schedules work of its own.
  function frames() {
    let prev = performance.now(),
      lastHud = 0,
      samples = [];

    const tick = (now) => {
      const dt = now - prev;
      prev = now;
      samples.push(dt);
      if (samples.length > 300) samples.shift();

      if (autoscroll && scroller && !paged) {
        scroller.scrollTop += (4000 * dt) / 1000;
        dirty = true;
        if (scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 1) {
          autoscroll = false;
        }
      }
      if (dirty) {
        dirty = false;
        ensureTiles();
      }

      if (now - lastHud > 250 && samples.length > 30) {
        const sorted = [...samples].sort((a, b) => a - b);
        const p50 = sorted[sorted.length >> 1];
        hud.fps = Math.round(1000 / p50);
        hud.worst = Math.round(sorted[Math.floor(sorted.length * 0.99)] * 10) / 10;
        hud.dropped = samples.filter((d) => d > p50 * 1.5).length;
        lastHud = now;
        invoke("stats", { kind })
          .then((s) => (rust = s))
          .catch(() => {});
      }
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  }

  function dropStripTiles() {
    for (const el of live.values()) el.remove();
    live.clear();
    hud.mounted = 0;
  }

  /// Changing the sample rate changes the width we ask for, so the chapter has to be
  /// laid out again. Keeps your place.
  function setSample(next) {
    sample = next;
    if (layout) load(kind, true);
  }

  /// Padding only moves pages apart; nothing needs re-decoding.
  function setPad(next) {
    pad = next;
    rebuildTops();
    if (canvas) canvas.style.height = canvasHeight() + "px";
    dropStripTiles();
    dirty = true;
  }

  function setMode(next) {
    mode = next;
    overridden = true;
    if (mode === "webtoon") {
      dirty = true;
    } else {
      // The scroller is hidden but its tiles would otherwise stay in the DOM, holding
      // decoded bitmaps for a view nobody is looking at.
      dropStripTiles();
      prefetch();
    }
  }

  function onKey(e) {
    const k = e.key;

    if (k === "1") load("cbz");
    else if (k === "2") load("strip");
    else if (k === "s") autoscroll = !autoscroll;
    else if (k === "f") fit = FITS[(FITS.indexOf(fit) + 1) % FITS.length];
    else if (k === "d") setSample(SAMPLES[(SAMPLES.indexOf(sample) + 1) % SAMPLES.length]);
    else if (k === "p") setPad(PADS[(PADS.indexOf(pad) + 1) % PADS.length]);
    else if (k === "[") rot = (rot + 270) % 360;
    else if (k === "]") rot = (rot + 90) % 360;
    else if (k === "l") rotLock = !rotLock;
    else if (k === "m") setMode(MODES[(MODES.indexOf(mode) + 1) % MODES.length]);
    else if (k === "r") {
      overridden = false;
      if (detected) mode = detected.mode;
    } else if (!paged) return;
    else if (k === "Home") go(-pageCount);
    else if (k === "End") go(pageCount);
    else if (pageStep(k, rtl)) go(pageStep(k, rtl));
    else return;
    e.preventDefault();
  }

  function onClick(e) {
    if (paged) go(clickStep(e.clientX, window.innerWidth, rtl));
  }

  function onResize() {
    vw = window.innerWidth;
    vh = window.innerHeight;
    dirty = true;
    // Re-request the layout so tiles are produced at the new width. Debounced: a drag
    // resize would otherwise ask for a new chapter layout on every frame.
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      if (layout) load(kind, true);
    }, 300);
  }

  onMount(() => {
    vw = window.innerWidth;
    vh = window.innerHeight;
    frames();
    load("cbz");
  });
</script>

<svelte:window on:keydown={onKey} on:resize={onResize} />

<div
  class="scroller"
  class:hidden={paged}
  bind:this={scroller}
  onscroll={() => (dirty = true)}
>
  <div class="canvas" bind:this={canvas}></div>
</div>

{#if paged && view}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="paged" onclick={onClick}>
    <div class="page" style="width:{view.boxW}px;height:{view.boxH}px;padding:{pad}px">
      <div
        class="turn"
        style="width:{view.drawW}px;height:{view.drawH}px;transform:translate(-50%,-50%) rotate({view.turn}deg)"
      >
        {#each view.tiles as t (t.t)}
          <img
            src={t.url}
            alt=""
            decoding="async"
            style="position:absolute;left:0;top:{t.top}px;width:100%;height:{t.h}px"
            onload={() => (hud.firstPaint ??= Math.round(performance.now() - openedAt))}
          />
        {/each}
      </div>
      {#if !view.p.readable}
        <p class="broken">Page {page + 1} could not be read.</p>
      {/if}
    </div>
  </div>
{/if}

{#if error}
  <p class="error">{error}</p>
{/if}

<div class="hud">
  <b>{kind}</b>
  <button onclick={() => load("cbz")} disabled={kind === "cbz"}>cbz [1]</button>
  <button onclick={() => load("strip")} disabled={kind === "strip"}>strip [2]</button>
  {#if !paged}
    <button onclick={() => (autoscroll = !autoscroll)}>
      {autoscroll ? "stop" : "flick"} [s]
    </button>
  {/if}
  <hr />
  <div>mode <b>{mode}</b> {overridden ? "(manual)" : `via ${detected?.source ?? "-"}`}</div>
  <div>fit <b>{fit}</b> [f] · mode [m] · reset [r]</div>
  <div>downsample <b>{sample === 1 ? "full" : sample}</b> [d]</div>
  <div>padding <b>{pad}</b> px [p]</div>
  <div>
    rotate <b>{view ? view.turn : rot}</b>&deg; [ [ ] ] · auto-turn wide
    <b>{rotLock ? "off" : "on"}</b> [l]
  </div>
  {#if paged}
    <div>page <b>{page + 1}</b> / {pageCount}</div>
  {/if}
  {#if unreadable}
    <div>unreadable <b>{unreadable}</b> page{unreadable === 1 ? "" : "s"}</div>
  {/if}
  <hr />
  <div>fps p50 <b>{hud.fps}</b></div>
  <div>worst frame <b>{hud.worst}</b> ms</div>
  <div>dropped <b>{hud.dropped}</b> / 300</div>
  {#if !paged}<div>tiles mounted <b>{hud.mounted}</b></div>{/if}
  <div>first paint <b>{hud.firstPaint ?? "-"}</b> ms</div>
  <hr />
  <div>page decodes <b>{rust.page_decodes ?? 0}</b></div>
  <div>decode avg <b>{(rust.decode_ms_avg ?? 0).toFixed(1)}</b> ms</div>
  <div>encode avg <b>{(rust.encode_ms_avg ?? 0).toFixed(1)}</b> ms</div>
  <div>tile hit/miss <b>{rust.hits ?? 0}</b>/<b>{rust.misses ?? 0}</b></div>
  <div>passthrough <b>{rust.passthrough ?? 0}</b></div>
  <div>warmed <b>{rust.warmed ?? 0}</b> pages</div>
  <div>cache <b>{(rust.cached_mb ?? 0).toFixed(1)}</b> MB</div>
</div>

<style>
  :global(body) {
    margin: 0;
    background: #0e0e10;
    color: #e8e6df;
    font: 13px/1.4 "IBM Plex Mono", ui-monospace, monospace;
    overflow: hidden;
  }
  .scroller {
    position: fixed;
    inset: 0;
    overflow-y: scroll;
    overflow-x: hidden;
    /* Let the compositor own the scroll; the tile window follows in rAF. */
    will-change: scroll-position;
  }
  .scroller.hidden {
    visibility: hidden;
    pointer-events: none;
  }
  .canvas {
    position: relative;
    margin: 0 auto;
  }
  .paged {
    position: fixed;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    overflow: auto;
  }
  .page {
    position: relative;
    flex: none;
    box-sizing: content-box;
  }
  .turn {
    position: absolute;
    left: 50%;
    top: 50%;
    transform-origin: center;
  }
  .hud {
    position: fixed;
    top: 12px;
    right: 12px;
    padding: 12px;
    background: #1a1a1dee;
    border: 1px solid #ffffff20;
    min-width: 210px;
  }
  .hud hr {
    border: 0;
    border-top: 1px solid #ffffff20;
    margin: 8px 0;
  }
  .hud b {
    color: #6f9c7e;
  }
  button {
    font: inherit;
    background: #e8e6df;
    color: #16150f;
    border: 0;
    padding: 3px 7px;
    cursor: pointer;
  }
  button:disabled {
    background: #55524a;
    color: #e8e6df;
    cursor: default;
  }
  .broken {
    position: absolute;
    left: 50%;
    bottom: 12px;
    transform: translateX(-50%);
    margin: 0;
    padding: 6px 10px;
    background: #b33a2b;
    color: #fff;
    white-space: nowrap;
  }
  .error {
    position: fixed;
    left: 12px;
    bottom: 12px;
    max-width: 60ch;
    margin: 0;
    padding: 12px;
    background: #b33a2b;
    color: #fff;
  }
</style>
