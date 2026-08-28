<script>
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";

  // Kept mounted above and below the viewport. Tuned so a 4000px/s flick still has
  // half a second of runway before it outruns the tile filler.
  const OVERSCAN = 2400;
  const MAX_DEVICE_W = 1600;

  let kind = $state("cbz");
  let error = $state(null);
  let hud = $state({ fps: 0, worst: 0, dropped: 0, mounted: 0, firstPaint: null });
  let rust = $state({});
  let autoscroll = $state(false);

  let scroller = $state(null);
  let canvas = $state(null);

  // Scroll-path state, deliberately outside $state: the reader canvas is imperative.
  let layout = null;
  let base = "";
  let dpr = 1;
  let live = new Map();
  let dirty = true;
  let openedAt = 0;

  // Device px -> CSS px, rounded on absolute strip coordinates so neighbouring tiles
  // share an edge exactly. Rounding each tile's height independently is what puts
  // 1px seams down a webtoon.
  const css = (deviceY) => Math.round(deviceY / dpr);

  async function load(next) {
    kind = next;
    error = null;
    hud.firstPaint = null;
    for (const el of live.values()) el.remove();
    live.clear();

    dpr = window.devicePixelRatio || 1;
    const displayW = Math.round(Math.min(scroller.clientWidth, MAX_DEVICE_W / dpr) * dpr);
    openedAt = performance.now();
    try {
      base ||= await invoke("tile_base");
      layout = await invoke("open_chapter", { kind, displayW });
    } catch (e) {
      error = String(e);
      layout = null;
      return;
    }
    layout.maxW = layout.pages.reduce((m, p) => Math.max(m, p.w), 1);
    canvas.style.width = css(layout.maxW) + "px";
    canvas.style.height = css(layout.total_h) + "px";
    scroller.scrollTop = 0;
    dirty = true;
  }

  function pageAt(y) {
    let lo = 0, hi = layout.pages.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (layout.pages[mid].y <= y) lo = mid; else hi = mid - 1;
    }
    return lo;
  }

  function mount(p, t) {
    // Tile height is per page: a passthrough page is one tile as tall as itself.
    const top = p.y + t * p.tile_h;
    const bottom = Math.min(p.y + p.h, top + p.tile_h);
    const img = document.createElement("img");
    img.src = `${base}/t/${kind}/${p.index}/${t}/${layout.display_w}`;
    img.decoding = "async";
    img.style.cssText =
      `position:absolute;left:${css(layout.maxW - p.w) / 2}px;top:${css(top)}px;` +
      `width:${css(p.w)}px;height:${css(bottom) - css(top)}px;display:block`;
    if (hud.firstPaint === null) {
      img.addEventListener("load", () => {
        hud.firstPaint ??= Math.round(performance.now() - openedAt);
      }, { once: true });
    }
    canvas.appendChild(img);
    return img;
  }

  function ensureTiles() {
    if (!layout) return;
    const top = (scroller.scrollTop - OVERSCAN) * dpr;
    const bottom = (scroller.scrollTop + scroller.clientHeight + OVERSCAN) * dpr;
    const want = new Set();

    for (let i = pageAt(Math.max(top, 0)); i < layout.pages.length; i++) {
      const p = layout.pages[i];
      if (p.y > bottom) break;
      const first = Math.max(0, Math.floor((top - p.y) / p.tile_h));
      const last = Math.min(p.tiles - 1, Math.floor((bottom - p.y) / p.tile_h));
      for (let t = first; t <= last; t++) {
        const key = `${p.index}:${t}`;
        want.add(key);
        if (!live.has(key)) live.set(key, mount(p, t));
      }
    }
    for (const [key, el] of live) {
      if (!want.has(key)) { el.remove(); live.delete(key); }
    }
    hud.mounted = live.size;
  }

  // One rAF loop drives both the frame-time measurement and the tile window, so
  // scrolling never schedules work of its own.
  function frames() {
    let prev = performance.now(), lastHud = 0, samples = [], dropped = 0;

    const tick = (now) => {
      const dt = now - prev;
      prev = now;
      samples.push(dt);
      if (samples.length > 300) samples.shift();

      if (autoscroll && scroller) {
        scroller.scrollTop += (4000 * dt) / 1000;
        dirty = true;
        if (scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 1) {
          autoscroll = false;
        }
      }
      if (dirty) { dirty = false; ensureTiles(); }

      if (now - lastHud > 250 && samples.length > 30) {
        const sorted = [...samples].sort((a, b) => a - b);
        const p50 = sorted[sorted.length >> 1];
        const p99 = sorted[Math.floor(sorted.length * 0.99)];
        dropped = samples.filter((d) => d > p50 * 1.5).length;
        hud.fps = Math.round(1000 / p50);
        hud.worst = Math.round(p99 * 10) / 10;
        hud.dropped = dropped;
        lastHud = now;
        invoke("stats", { kind }).then((s) => (rust = s)).catch(() => {});
      }
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  }

  function onKey(e) {
    if (e.key === "s") autoscroll = !autoscroll;
    else if (e.key === "1") load("cbz");
    else if (e.key === "2") load("strip");
    else return;
    e.preventDefault();
  }

  onMount(() => {
    frames();
    load("cbz");
  });
</script>

<svelte:window on:keydown={onKey} />

<div class="scroller" bind:this={scroller} onscroll={() => (dirty = true)}>
  <div class="canvas" bind:this={canvas}></div>
</div>

{#if error}
  <p class="error">{error}</p>
{/if}

<div class="hud">
  <b>{kind}</b>
  <button onclick={() => load("cbz")} disabled={kind === "cbz"}>cbz [1]</button>
  <button onclick={() => load("strip")} disabled={kind === "strip"}>strip [2]</button>
  <button onclick={() => (autoscroll = !autoscroll)}>{autoscroll ? "stop" : "flick"} [s]</button>
  <hr />
  <div>fps p50 <b>{hud.fps}</b></div>
  <div>worst frame <b>{hud.worst}</b> ms</div>
  <div>dropped <b>{hud.dropped}</b> / 300</div>
  <div>tiles mounted <b>{hud.mounted}</b></div>
  <div>first paint <b>{hud.firstPaint ?? "-"}</b> ms</div>
  <hr />
  <div>page decodes <b>{rust.page_decodes ?? 0}</b></div>
  <div>decode avg <b>{(rust.decode_ms_avg ?? 0).toFixed(1)}</b> ms</div>
  <div>encode avg <b>{(rust.encode_ms_avg ?? 0).toFixed(1)}</b> ms</div>
  <div>tile hit/miss <b>{rust.hits ?? 0}</b>/<b>{rust.misses ?? 0}</b></div>
  <div>passthrough <b>{rust.passthrough ?? 0}</b></div>
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
  .canvas {
    position: relative;
    margin: 0 auto;
  }
  .hud {
    position: fixed;
    top: 12px;
    right: 12px;
    padding: 12px;
    background: #1a1a1dee;
    border: 1px solid #ffffff20;
    min-width: 190px;
  }
  .hud hr { border: 0; border-top: 1px solid #ffffff20; margin: 8px 0; }
  .hud b { color: #6f9c7e; }
  button {
    font: inherit;
    background: #e8e6df;
    color: #16150f;
    border: 0;
    padding: 3px 7px;
    cursor: pointer;
  }
  button:disabled { background: #55524a; color: #e8e6df; cursor: default; }
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
