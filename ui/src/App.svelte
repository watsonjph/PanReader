<script>
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import {
    clickStep,
    fitScale,
    groupOf,
    spreadGroups,
    pageAt,
    pageFrac,
    pageStep,
    pageTops,
    scrollForFrac,
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
  /// Cover request width in device px: twice the drawn card, so HiDPI stays sharp.
  const COVER_W = 320;

  let series = $state([]);
  let resume = $state([]);
  let seriesChapters = $state([]);
  let openSeries = $state(null);
  let chapterId = $state(null);
  let chapterTitle = $state("");
  let libraryRoots = $state([]);
  let catalogs = $state([]);
  let catalogUrl = $state("");
  // The catalog being browsed, and the trail back out of it.
  let opds = $state(null);
  let opdsTrail = $state([]);
  let opdsBusy = $state(false);
  // Which library folder downloads land in. Only ever one the reader added.
  let downloadRoot = $state(null);
  let rootInput = $state("");
  let busy = $state(false);
  let query = $state("");
  let searchTimer = 0;
  let cats = $state([]);
  let activeCat = $state(null);
  let seriesCats = $state([]);
  let newCat = $state("");
  let manageCats = $state(false);
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
  let spread = $state(false);
  let zoom = $state(1);
  let panX = $state(0);
  let panY = $state(0);
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
  let saveTimer = 0;
  let positionTimer = 0;
  let tops = []; // CSS-px top of each page, padding included
  // Reactive because it drives the grab cursor; the rest of the drag state is only
  // ever read from handlers.
  let dragging = $state(false);
  let dragMoved = false;
  let dragX = 0;
  let dragY = 0;
  let padHeld = new Set();

  const current = $derived(seriesChapters.find((c) => c.id === chapterId) ?? null);
  const paged = $derived(mode !== "webtoon");
  const groups = $derived.by(() => {
    void epoch, spread;
    return layout ? spreadGroups(layout.pages, { enabled: spread }) : [];
  });
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
    `${base}/t/${chapterId}/${index}/${t}/${layout.display_w}`;

  function rebuildTops() {
    tops = layout ? pageTops(layout.pages, pad, dpr) : [];
  }

  function canvasHeight() {
    return layout ? stripHeight(layout.total_h, layout.pages.length, pad, dpr) : 0;
  }

  // The paged view. Rebuilt only when something it depends on changes, which for a page
  // turn is once -- there is no scroll path here to keep off the main thread.
  const view = $derived.by(() => {
    void epoch, page, fit, vw, vh, mode, rot, rotLock, pad, sample, spread, zoom;
    if (!layout || mode === "webtoon") return null;
    const members = (groups[groupOf(groups, page)] ?? [page])
      .map((i) => layout.pages[i])
      .filter(Boolean);
    if (!members.length) return null;

    // A pair is measured and scaled as one unit, so the two halves stay the same size
    // and the spread fits the window as a whole rather than page by page.
    const naturalW = members.reduce((sum, p) => sum + p.w / dpr, 0);
    const naturalH = Math.max(...members.map((p) => p.h / dpr));

    const turn = turnFor({ rot, rotLock, w: naturalW, h: naturalH, fit, vw, vh });
    const quarter = turn === 90 || turn === 270;
    const base = quarter
      ? fitScale(fit, naturalH, naturalW, vw, vh)
      : fitScale(fit, naturalW, naturalH, vw, vh);
    const scale = base * zoom;

    const pages = members.map((p) => {
      const drawW = Math.round((p.w / dpr) * scale);
      const tiles = tileRects(p).map(({ t, top, bottom }) => {
        const y0 = Math.round((top / dpr) * scale);
        const y1 = Math.round((bottom / dpr) * scale);
        return { t, url: tileUrl(p.index, t), top: y0, h: y1 - y0 };
      });
      const drawH = tiles.length
        ? tiles[tiles.length - 1].top + tiles[tiles.length - 1].h
        : 0;
      return { p, drawW, drawH, tiles };
    });

    const totalW = pages.reduce((sum, v) => sum + v.drawW, 0);
    const totalH = Math.max(...pages.map((v) => v.drawH));
    return {
      pages,
      // Right to left means the earlier page sits on the right.
      order: rtl ? [...pages].reverse() : pages,
      turn,
      totalW,
      totalH,
      boxW: quarter ? totalH : totalW,
      boxH: quarter ? totalW : totalH,
      broken: members.filter((p) => !p.readable).map((p) => p.index + 1),
    };
  });

  async function load(chapter, keepPage = false) {
    const wanted = keepPage ? page : chapter.page ?? 0;
    // Strip mode keeps its place by page and a fraction of that page, never by pixel: a
    // reload can change the decode width and the padding, so an old pixel offset means
    // nothing while a fraction still does.
    const wantedScroll = keepPage && layout && !paged ? pageAt(tops, scroller.scrollTop) : wanted;
    const wantedFrac =
      keepPage && layout && !paged
        ? pageFrac(tops, scroller.scrollTop, wantedScroll, canvasHeight())
        : (chapter.page_frac ?? 0);
    chapterId = chapter.id;
    chapterTitle = chapter.title;
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
      layout = await invoke("open_chapter", { chapterId: chapter.id, displayW });
    } catch (e) {
      error = String(e);
      layout = null;
      return;
    }

    resetView();
    detected = layout.reading;
    if (!overridden) mode = layout.reading.mode;
    pageCount = layout.pages.length;
    page = Math.min(wanted, pageCount - 1);
    epoch++;

    layout.maxW = layout.pages.reduce((m, p) => Math.max(m, p.w), 1);
    rebuildTops();
    canvas.style.width = css(layout.maxW) + "px";
    canvas.style.height = canvasHeight() + "px";
    scroller.scrollTop = paged
      ? 0
      : scrollForFrac(tops, wantedScroll, wantedFrac, canvasHeight());
    dirty = true;
    if (paged) prefetch();
  }

  /// Warm the pages either side of this one. The browser caches by URL and our tiles are
  /// immutable, so touching the URL is the whole prefetch.
  function prefetch() {
    if (!layout) return;
    const at = groupOf(groups, page);
    for (const d of [1, 2, 3, -1]) {
      for (const i of groups[at + d] ?? []) {
        const p = layout.pages[i];
        if (!p) continue;
        for (const { t } of tileRects(p)) new Image().src = tileUrl(p.index, t);
      }
    }
  }

  /// Debounced because it runs per keystroke. The query itself is a scan of the
  /// series table, which is about a millisecond for ten thousand titles -- the debounce
  /// is to avoid a round trip per character, not to spare the database.
  function onSearch() {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(async () => {
      try {
        series = await invoke("search", { query, category: activeCat });
      } catch (e) {
        error = String(e);
      }
    }, 120);
  }

  const MODES_OPT = [null, "rtl", "ltr", "webtoon"];
  const modeLabel = (m) => m ?? "detect";

  async function refreshCategories() {
    try {
      cats = await invoke("categories");
    } catch (e) {
      error = String(e);
    }
  }

  async function filterBy(id) {
    activeCat = activeCat === id ? null : id;
    await refreshLibrary();
  }

  async function addCategory() {
    const name = newCat.trim();
    if (!name) return;
    try {
      await invoke("create_category", { name });
      newCat = "";
      await refreshCategories();
    } catch (e) {
      error = String(e);
    }
  }

  /// Cycles detect -> rtl -> ltr -> webtoon. "detect" is null, meaning the category
  /// has no opinion and page shape decides.
  async function cycleCategoryMode(cat) {
    const next = MODES_OPT[(MODES_OPT.indexOf(cat.reading_mode) + 1) % MODES_OPT.length];
    await invoke("set_category_mode", { id: cat.id, mode: next });
    await refreshCategories();
  }

  async function toggleSeriesCategory(catId) {
    if (!openSeries) return;
    const member = !seriesCats.includes(catId);
    await invoke("set_series_category", {
      seriesId: openSeries.id,
      categoryId: catId,
      member,
    });
    seriesCats = await invoke("categories_of", { seriesId: openSeries.id });
    await refreshCategories();
  }

  async function refreshLibrary() {
    try {
      base ||= await invoke("tile_base");
      [series, libraryRoots, busy, resume, catalogs] = await Promise.all([
        invoke("search", { query, category: activeCat }),
        invoke("roots"),
        invoke("scanning"),
        invoke("continue_reading"),
        invoke("catalogs"),
      ]);
    } catch (e) {
      error = String(e);
    }
  }

  const coverUrl = (chapterId) => `${base}/c/${chapterId}/${COVER_W}`;

  /// The system folder picker. Pasting a path worked but is not how anyone expects to
  /// add a folder.
  async function pickFolder() {
    error = null;
    try {
      const picked = await openDialog({ directory: true, multiple: false });
      if (!picked) return; // cancelled
      rootInput = picked;
      await addRoot();
    } catch (e) {
      error = String(e);
    }
  }

  /// Follow a feed. `push` records where we came from so Back works.
  async function openFeed(url, push = true) {
    opdsBusy = true;
    error = null;
    try {
      const page = await invoke("opds_browse", { url });
      if (push && opds) opdsTrail = [...opdsTrail, opds.url];
      opds = page;
    } catch (e) {
      error = String(e);
    } finally {
      opdsBusy = false;
    }
  }

  async function opdsBack() {
    const previous = opdsTrail.at(-1);
    opdsTrail = opdsTrail.slice(0, -1);
    if (previous) await openFeed(previous, false);
    else opds = null;
  }

  async function addCatalog() {
    const url = catalogUrl.trim();
    if (!url) return;
    opdsBusy = true;
    error = null;
    try {
      await invoke("add_catalog", { url });
      catalogUrl = "";
      await refreshLibrary();
    } catch (e) {
      error = String(e);
    } finally {
      opdsBusy = false;
    }
  }

  /// Download the first format the image reader can open, falling back to whatever is
  /// offered. Nothing here needs to know it came from a server: it lands in a library
  /// root and the rescan treats it as an ordinary local chapter.
  async function grab(entry) {
    const downloads = entry.kind.Publication?.downloads ?? [];
    const pick =
      downloads.find((d) => /comicbook|zip/.test(d.mime)) ?? downloads[0];
    if (!pick) return;
    opdsBusy = true;
    error = null;
    try {
      await invoke("opds_download", {
        href: pick.href,
        title: entry.title,
        mime: pick.mime,
        root: downloadRoot,
      });
      watchScan();
    } catch (e) {
      error = String(e);
    } finally {
      opdsBusy = false;
    }
  }

  async function addRoot() {
    const path = rootInput.trim();
    if (!path) return;
    error = null;
    try {
      await invoke("add_root", { path });
      rootInput = "";
      await watchScan();
    } catch (e) {
      error = String(e);
    }
  }

  /// A scan runs on its own thread, so the library is polled until it settles rather
  /// than blocking a command on it (invariant 7).
  async function watchScan() {
    busy = true;
    for (let i = 0; i < 600; i++) {
      await new Promise((r) => setTimeout(r, 200));
      await refreshLibrary();
      if (!busy) return;
    }
  }

  async function showSeries(row) {
    openSeries = row;
    try {
      seriesChapters = await invoke("chapters", { seriesId: row.id });
      seriesCats = await invoke("categories_of", { seriesId: row.id });
    } catch (e) {
      error = String(e);
    }
  }

  function toLibrary() {
    chapterId = null;
    layout = null;
    for (const el of live.values()) el.remove();
    live.clear();
    refreshLibrary();
  }

  /// Debounced: a page turn is cheap but a flick is twenty of them, and this is a disk
  /// write. The row is a one-row upsert, which is why position is not in the settings
  /// blob at all.
  function savePosition() {
    if (chapterId === null) return;
    clearTimeout(positionTimer);
    const id = chapterId;
    const at = page;
    // Paged mode has no within-page offset. The strip does, and losing it means
    // reopening a webtoon at the top of an eight thousand pixel page.
    const frac =
      paged || !scroller ? 0 : pageFrac(tops, scroller.scrollTop, at, canvasHeight());
    const done = pageCount > 0 && page >= pageCount - 1;
    positionTimer = setTimeout(() => {
      invoke("save_position", { chapterId: id, page: at, frac, completed: done }).catch(
        () => {},
      );
    }, 400);
  }

  /// Persist the view settings.
  ///
  /// Debounced, and deliberately not on the page-turn path: settings change rarely,
  /// so the whole blob is rewritten, whereas reading position gets its own table
  /// because it is written on every turn.
  function persist() {
    clearTimeout(saveTimer);
    saveTimer = setTimeout(() => {
      invoke("save_settings", {
        settings: {
          default_reading_mode: overridden ? mode : (detected?.mode ?? "rtl"),
          fit,
          downsample: sample,
          page_padding: pad,
          rotation: rot,
          rotation_lock: rotLock,
          double_page: spread,
          cover_alone: true,
        },
      }).catch((e) => console.warn("could not save settings:", e));
    }, 500);
  }

  /// Re-aim the background fill at the current page. Debounced: flicking through
  /// twenty pages should retarget once, not twenty times.
  function reWarm() {
    if (!layout) return;
    clearTimeout(warmTimer);
    warmTimer = setTimeout(() => {
      invoke("warm", { chapterId, page, displayW: layout.display_w }).catch(() => {});
    }, 400);
  }

  function go(delta) {
    if (!layout || !delta) return;
    // Steps are groups, so a double-page spread advances by two pages and a lone
    // printed spread by one, without the caller having to know which.
    const at = groupOf(groups, page);
    const next = Math.min(Math.max(at + delta, 0), groups.length - 1);
    const first = groups[next]?.[0];
    if (first === undefined || first === page) return;
    page = first;
    savePosition();
    resetView();
    prefetch();
    reWarm();
  }

  /// Zoom and pan belong to the page you were looking at, not the next one.
  function resetView() {
    zoom = 1;
    panX = 0;
    panY = 0;
  }

  /// Double-click zooms toward the point under the cursor, so the thing you aimed at
  /// stays where it is instead of sliding to the middle.
  function zoomAt(clientX, clientY) {
    if (zoom > 1) return resetView();
    const factor = 2;
    const cx = clientX - vw / 2 - panX;
    const cy = clientY - vh / 2 - panY;
    zoom = factor;
    panX -= cx * (factor - 1);
    panY -= cy * (factor - 1);
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

    // The strip scrolls natively, so go() never runs and this is the only place that
    // knows where the reader actually is. Position and the warm target both hang off
    // it; without this a webtoon warms around page 0 for the whole chapter.
    const at = pageAt(tops, scroller.scrollTop);
    if (at !== page) {
      page = at;
      reWarm();
    }
    // Every scroll, not only a page change: the offset within a page is half the
    // position in a webtoon. The 400 ms debounce means a continuous scroll writes once,
    // when it stops.
    savePosition();

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
      pollGamepad(dt);
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
        if (chapterId !== null) {
          invoke("stats", { chapterId })
            .then((s) => (rust = s))
            .catch(() => {});
        }
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
    persist();
    if (layout && current) load(current, true);
  }

  /// Padding only moves pages apart; nothing needs re-decoding.
  function setPad(next) {
    pad = next;
    persist();
    rebuildTops();
    if (canvas) canvas.style.height = canvasHeight() + "px";
    dropStripTiles();
    dirty = true;
  }

  function setMode(next) {
    mode = next;
    overridden = true;
    persist();
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

    if (k === "Escape") toLibrary();
    else if (k === "s") autoscroll = !autoscroll;
    else if (k === "f") { fit = FITS[(FITS.indexOf(fit) + 1) % FITS.length]; persist(); }
    else if (k === "d") setSample(SAMPLES[(SAMPLES.indexOf(sample) + 1) % SAMPLES.length]);
    else if (k === "p") setPad(PADS[(PADS.indexOf(pad) + 1) % PADS.length]);
    else if (k === "[") { rot = (rot + 270) % 360; persist(); }
    else if (k === "]") { rot = (rot + 90) % 360; persist(); }
    else if (k === "l") { rotLock = !rotLock; persist(); }
    else if (k === "w") { spread = !spread; persist(); }
    else if (k === "F" || k === "F11") toggleFullscreen();
    else if (k === "0") resetView();
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
    // A drag is not a click. Without this, panning a zoomed page turns it as well.
    if (!paged || dragMoved) return;
    go(clickStep(e.clientX, window.innerWidth, rtl));
  }

  /// Double-click zooms, but only in the middle third.
  ///
  /// The sides turn pages on a single click, and a double-click there would fire the
  /// turn first. Delaying every turn to wait for a possible second click would make the
  /// reader feel sluggish, so the two gestures get separate zones instead. The middle
  /// already does nothing on a single click.
  function onDoubleClick(e) {
    if (paged && clickStep(e.clientX, window.innerWidth, rtl) === 0) {
      zoomAt(e.clientX, e.clientY);
    }
  }

  function onWheel(e) {
    if (!paged) return; // the strip scrolls natively
    e.preventDefault();
    if (overflows) {
      panX -= e.deltaX;
      panY -= e.deltaY;
    } else {
      go(e.deltaY > 0 ? 1 : -1);
    }
  }

  const overflows = $derived(
    !!view && (zoom > 1 || view.boxW > vw || view.boxH > vh),
  );

  function onPointerDown(e) {
    if (!paged || !overflows) return;
    dragging = true;
    dragMoved = false;
    dragX = e.clientX;
    dragY = e.clientY;
    e.currentTarget.setPointerCapture?.(e.pointerId);
  }

  function onPointerMove(e) {
    if (!dragging) return;
    const dx = e.clientX - dragX;
    const dy = e.clientY - dragY;
    if (Math.abs(dx) + Math.abs(dy) > 4) dragMoved = true;
    panX += dx;
    panY += dy;
    dragX = e.clientX;
    dragY = e.clientY;
  }

  function onPointerUp() {
    dragging = false;
    // Cleared on the next frame so the click that follows this release still sees it.
    setTimeout(() => (dragMoved = false), 0);
  }

  /// Standard-layout buttons: d-pad left/right/up/down, then the two shoulders.
  const PAD_BACK = [14, 12, 4];
  const PAD_FORWARD = [15, 13, 5];
  /// Below this the stick is at rest; sticks do not return to exactly zero.
  const STICK_DEADZONE = 0.15;

  /// Polled from the frame loop, because the Gamepad API has no events for buttons.
  ///
  /// Reading on a TV is a real use for this, and a controller that repeats a page turn
  /// every frame is useless -- so turns fire on the press edge, while the stick scrolls
  /// continuously because that is what a stick is for.
  function pollGamepad(dt) {
    const pads = navigator.getGamepads?.() ?? [];
    const pressed = new Set();

    for (const gp of pads) {
      if (!gp) continue;
      gp.buttons.forEach((b, i) => b.pressed && pressed.add(i));

      if (!paged && scroller) {
        const y = gp.axes[1] ?? 0;
        if (Math.abs(y) > STICK_DEADZONE) {
          scroller.scrollTop += y * 2000 * (dt / 1000);
          dirty = true;
        }
      }
    }

    if (paged) {
      const edge = (b) => pressed.has(b) && !padHeld.has(b);
      if (PAD_FORWARD.some(edge)) go(1);
      else if (PAD_BACK.some(edge)) go(-1);
    }
    padHeld = pressed;
  }

  function toggleFullscreen() {
    if (document.fullscreenElement) document.exitFullscreen?.();
    else document.documentElement.requestFullscreen?.().catch(() => {});
  }

  function onResize() {
    vw = window.innerWidth;
    vh = window.innerHeight;
    dirty = true;
    // Re-request the layout so tiles are produced at the new width. Debounced: a drag
    // resize would otherwise ask for a new chapter layout on every frame.
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      if (layout && current) load(current, true);
    }, 300);
  }

  onMount(async () => {
    vw = window.innerWidth;
    vh = window.innerHeight;
    // Settings first: the chapter open uses the persisted reading-mode default, so
    // loading them afterwards would detect against the wrong fallback once.
    try {
      const saved = await invoke("settings");
      mode = saved.default_reading_mode;
      fit = saved.fit;
      sample = saved.downsample;
      pad = saved.page_padding;
      rot = saved.rotation;
      rotLock = saved.rotation_lock;
      spread = saved.double_page;
    } catch (e) {
      console.warn("could not load settings:", e);
    }
    frames();
    await refreshCategories();
    await refreshLibrary();
    if (busy) watchScan();
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
  <div
    class="paged"
    class:grabbable={overflows}
    class:grabbing={dragging}
    onclick={onClick}
    ondblclick={onDoubleClick}
    onwheel={onWheel}
    onpointerdown={onPointerDown}
    onpointermove={onPointerMove}
    onpointerup={onPointerUp}
    onpointercancel={onPointerUp}
  >
    <div
      class="page"
      style="width:{view.boxW}px;height:{view.boxH}px;padding:{pad}px;transform:translate({panX}px,{panY}px)"
    >
      <div
        class="turn"
        style="width:{view.totalW}px;height:{view.totalH}px;transform:translate(-50%,-50%) rotate({view.turn}deg)"
      >
        {#each view.order as pv (pv.p.index)}
          <div class="leaf" style="width:{pv.drawW}px;height:{pv.drawH}px">
            {#each pv.tiles as t (t.t)}
              <img
                src={t.url}
                alt=""
                decoding="async"
                draggable="false"
                style="position:absolute;left:0;top:{t.top}px;width:100%;height:{t.h}px"
                onload={() =>
                  (hud.firstPaint ??= Math.round(performance.now() - openedAt))}
              />
            {/each}
          </div>
        {/each}
      </div>
      {#if view.broken.length}
        <p class="broken">
          Page{view.broken.length === 1 ? "" : "s"}
          {view.broken.join(", ")} could not be read.
        </p>
      {/if}
    </div>
  </div>
{/if}

{#if error}
  <p class="error">{error}</p>
{/if}

{#if chapterId === null}
  <div class="library">
    <h1>Library</h1>

    <div class="roots">
      {#each libraryRoots as root (root)}
        <div class="root">
          <span>{root}</span>
          <button
            onclick={async () => {
              await invoke("remove_root", { path: root });
              refreshLibrary();
            }}>remove</button
          >
        </div>
      {/each}
      <div class="root">
        <button onclick={pickFolder}>Add folder…</button>
        <input
          placeholder="…or paste a path"
          bind:value={rootInput}
          onkeydown={(e) => e.key === "Enter" && addRoot()}
        />
        <button onclick={async () => { await invoke("rescan"); watchScan(); }}>
          Rescan
        </button>
        {#if busy}<span class="busy">scanning…</span>{/if}
      </div>

      <div class="root">
        <input
          placeholder="OPDS catalog URL (Komga, Kavita, Calibre-Web)"
          bind:value={catalogUrl}
          onkeydown={(e) => e.key === "Enter" && addCatalog()}
        />
        <button onclick={addCatalog}>Add catalog</button>
        {#if opdsBusy}<span class="busy">working…</span>{/if}
      </div>

      {#if catalogs.length}
        <div class="chips">
          {#each catalogs as cat (cat.id)}
            <button class="chip" class:on={opds?.url === cat.url}
              onclick={() => { opdsTrail = []; openFeed(cat.url, false); }}>
              {cat.name}
            </button>
          {/each}
          {#if opds}
            <button class="chip ghost" onclick={() => { opds = null; opdsTrail = []; }}>
              Close catalog
            </button>
          {/if}
        </div>
      {/if}
    </div>

    {#if opds}
      <h2 class="section">
        {#if opdsTrail.length}
          <button class="chip" onclick={opdsBack}>Back</button>
        {/if}
        {opds.feed.title || "Catalog"}
      </h2>

      {#if libraryRoots.length > 1}
        <div class="chips">
          <span class="meta">Download into</span>
          {#each libraryRoots as root (root)}
            <button
              class="chip"
              class:on={(downloadRoot ?? libraryRoots[0]) === root}
              onclick={() => (downloadRoot = root)}
            >
              {root}
            </button>
          {/each}
        </div>
      {/if}

      {#if opds.feed.entries.length === 0}
        <p class="empty">This feed is empty.</p>
      {/if}

      <div class="shelf">
        {#each opds.feed.entries as entry (entry.id)}
          {@const nav = entry.kind.Navigation}
          <button
            class="card"
            onclick={() => (nav ? openFeed(nav.href) : grab(entry))}
          >
            <div class="cover">
              {#if entry.thumbnail}
                <img src={entry.thumbnail} alt="" loading="lazy" decoding="async" />
              {/if}
            </div>
            <b>{entry.title}</b>
            <span class="meta">
              {nav ? "browse" : entry.author ?? "download"}
            </span>
          </button>
        {/each}
      </div>

      {#if opds.feed.next}
        <button class="chip" onclick={() => openFeed(opds.feed.next)}>Next page</button>
      {/if}
    {/if}

    {#if series.length || query || activeCat !== null}
      <input
        class="search"
        placeholder="Search series"
        bind:value={query}
        oninput={onSearch}
      />

      <div class="chips">
        <button class="chip" class:on={activeCat === null} onclick={() => filterBy(null)}>
          All
        </button>
        {#each cats as cat (cat.id)}
          <button class="chip" class:on={activeCat === cat.id} onclick={() => filterBy(cat.id)}>
            {cat.name}
            <span class="count">{cat.series_count}</span>
          </button>
        {/each}
        <button class="chip ghost" onclick={() => (manageCats = !manageCats)}>
          {manageCats ? "Done" : "Edit categories"}
        </button>
      </div>

      {#if manageCats}
        <div class="manage">
          <div class="row">
            <input placeholder="New category" bind:value={newCat}
              onkeydown={(e) => e.key === "Enter" && addCategory()} />
            <button class="chip accent" onclick={addCategory}>Add</button>
          </div>
          {#each cats as cat (cat.id)}
            <div class="row">
              <span class="name">{cat.name}</span>
              <!-- A category with no mode leaves detection alone; that is the default
                   and the common case. -->
              <button class="chip" onclick={() => cycleCategoryMode(cat)}>
                reads {modeLabel(cat.reading_mode)}
              </button>
              <button
                class="chip danger"
                onclick={async () => {
                  await invoke("delete_category", { id: cat.id });
                  if (activeCat === cat.id) activeCat = null;
                  await refreshCategories();
                  await refreshLibrary();
                }}>Delete</button
              >
            </div>
          {/each}
        </div>
      {/if}
    {/if}

    {#if series.length === 0 && !busy}
      <p class="empty">
        {query ? `Nothing matches “${query}”.` : "Add a folder to start your library."}
      </p>
    {/if}

    <!-- Where you were, before what you own. A reader who opens the app is far more
         often resuming than browsing. -->
    {#if resume.length && !query && activeCat === null}
      <h2 class="section">Continue reading</h2>
      <div class="shelf resume">
        {#each resume as r (r.chapter_id)}
          <button
            class="card"
            onclick={() =>
              load({
                id: r.chapter_id,
                title: r.chapter_title,
                page: r.page,
                page_frac: r.page_frac,
              })}
          >
            <div class="cover">
              {#if base}
                <img src={coverUrl(r.chapter_id)} alt="" loading="lazy" decoding="async" />
              {/if}
              <div class="progress" style="width:{((r.page + 1) / r.page_count) * 100}%"></div>
            </div>
            <b>{r.series_title}</b>
            <span class="meta">
              {r.chapter_title} · {r.page + 1}/{r.page_count}
            </span>
          </button>
        {/each}
      </div>
      <h2 class="section">Library</h2>
    {/if}

    <div class="shelf">
      {#each series as row (row.id)}
        <button class="card" class:on={openSeries?.id === row.id} onclick={() => showSeries(row)}>
          <div class="cover">
            {#if row.cover_chapter_id !== null && base}
              <img src={coverUrl(row.cover_chapter_id)} alt="" loading="lazy" decoding="async" />
            {/if}
            {#if row.unread > 0 && row.unread < row.chapter_count}
              <span class="badge">{row.unread}</span>
            {/if}
          </div>
          <b>{row.title}</b>
          <span class="meta">
            {row.chapter_count} chapter{row.chapter_count === 1 ? "" : "s"}
          </span>
        </button>
      {/each}
    </div>

    {#if openSeries}
      <h2>{openSeries.title}</h2>
      {#if cats.length}
        <div class="chips">
          {#each cats as cat (cat.id)}
            <button
              class="chip"
              class:on={seriesCats.includes(cat.id)}
              onclick={() => toggleSeriesCategory(cat.id)}
            >
              {cat.name}
            </button>
          {/each}
        </div>
      {/if}
      <div class="chapters">
        {#each seriesChapters as c (c.id)}
          <button class="chapter" onclick={() => load(c)}>
            <span>{c.title}</span>
            <span class="meta">
              {c.page_count} pages
              {#if c.completed}· read{:else if c.page > 0}· page {c.page + 1}{/if}
            </span>
          </button>
        {/each}
      </div>
    {/if}
  </div>
{/if}

<div class="hud" class:reading={chapterId !== null}>
  <b>{chapterTitle}</b>
  <button onclick={toLibrary}>library [esc]</button>
  {#if !paged}
    <button onclick={() => (autoscroll = !autoscroll)}>
      {autoscroll ? "stop" : "flick"} [s]
    </button>
  {/if}
  <hr />
  <div>mode <b>{mode}</b> {overridden ? "(manual)" : `via ${detected?.source ?? "-"}`}</div>
  <div>fit <b>{fit}</b> [f] · mode [m] · reset [r]</div>
  <div>double page <b>{spread ? "on" : "off"}</b> [w] · full [F]</div>
  {#if zoom !== 1}
    <div>zoom <b>{zoom}x</b> — drag to pan, [0] resets</div>
  {/if}
  <div>downsample <b>{sample === 1 ? "full" : sample}</b> [d]</div>
  <div>padding <b>{pad}</b> px [p]</div>
  <div>
    rotate <b>{view ? view.turn : rot}</b>&deg; [ [ ] ] · auto-turn wide
    <b>{rotLock ? "off" : "on"}</b> [l]
  </div>
  {#if paged}
    <div>
      page <b>{view ? view.pages.map((v) => v.p.index + 1).join("-") : page + 1}</b>
      / {pageCount}
    </div>
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
  /* DESIGN.md's token layer. The full themes-as-data generator is S2; these are the
     same names, so that work becomes a swap rather than a rewrite. */
  :global(:root) {
    --bg: #0b0b0d;
    --raised: #141416;
    --text: #f2f0ea;
    --text-muted: #8e8b84;
    --progress: #6f9c7e;
    --accent: #e0a94e;
    --accent-soft: #f0c982;
    --highlight: #6fa8c7;
    --danger: #e5544a;

    --glass: rgba(255, 255, 255, 0.05);
    --glass-hover: rgba(255, 255, 255, 0.1);
    --hairline: rgba(255, 255, 255, 0.1);

    --r-1: 8px;
    --r-2: 12px;
    --r-full: 999px;

    --dur-fast: 90ms;
    --ease: cubic-bezier(0.2, 0, 0, 1);
  }
  :global(body) {
    margin: 0;
    background: var(--bg);
    color: var(--text);
    font: 13px/1.4 "IBM Plex Mono", ui-monospace, monospace;
    overflow: hidden;
  }
  /* People read for hours. DESIGN.md, Motion. */
  @media (prefers-reduced-motion: reduce) {
    :global(*) {
      transition: none !important;
    }
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
    /* Panning is a transform, not a scroll, so the container must not scroll too. */
    overflow: hidden;
    touch-action: none;
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
    display: flex;
  }
  .leaf {
    position: relative;
    flex: none;
  }
  .paged.grabbable {
    cursor: grab;
  }
  .paged.grabbing {
    cursor: grabbing;
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
  .library {
    position: fixed;
    inset: 0;
    overflow-y: auto;
    padding: 24px;
    background: var(--bg);
  }
  .library h1,
  .library h2 {
    font-weight: 600;
    margin: 0 0 12px;
  }
  .library h2 {
    margin-top: 24px;
    font-size: 15px;
  }
  .roots {
    margin-bottom: 20px;
  }
  .root {
    display: flex;
    gap: 8px;
    align-items: center;
    margin-bottom: 6px;
    color: var(--text-muted);
  }
  .root input {
    font: inherit;
    flex: 1 1 340px;
    max-width: 460px;
    padding: 4px 8px;
    background: var(--raised);
    color: var(--text);
    border: 1px solid var(--hairline);
  }
  .busy {
    color: var(--accent);
  }
  .empty {
    color: var(--text-muted);
  }
  .shelf {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(160px, 1fr));
    gap: 12px;
  }
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 16px;
  }
  /* kopuz's chip, in our tokens: a pill of glass that fills when selected. */
  .chip {
    font: inherit;
    font-size: 11px;
    font-weight: 600;
    height: 28px;
    padding: 0 12px;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    border-radius: var(--r-full);
    background: var(--glass);
    border: 1px solid var(--hairline);
    color: var(--text-muted);
    cursor: pointer;
    transition:
      background var(--dur-fast) var(--ease),
      color var(--dur-fast) var(--ease);
  }
  .chip:hover {
    background: var(--glass-hover);
    color: var(--text);
  }
  .chip.on {
    background: var(--glass-hover);
    color: var(--text);
    border-color: var(--accent);
  }
  .chip.ghost {
    border-style: dashed;
  }
  /* Dark ink on amber: white would fail contrast where kopuz's white-on-indigo does
     not. Same idiom, different accent. */
  .chip.accent {
    background: var(--accent);
    border-color: var(--accent);
    color: #16150f;
  }
  .chip.accent:hover {
    background: var(--accent-soft);
    color: #16150f;
  }
  .chip.danger:hover {
    color: var(--danger);
    border-color: var(--danger);
  }
  .count {
    color: var(--text-muted);
  }
  .manage {
    margin-bottom: 20px;
  }
  .manage .row {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 6px;
  }
  .manage .name {
    min-width: 160px;
  }
  .manage input {
    font: inherit;
    padding: 4px 8px;
    background: var(--raised);
    color: var(--text);
    border: 1px solid var(--hairline);
    border-radius: var(--r-1);
  }
  .search {
    font: inherit;
    display: block;
    width: 100%;
    max-width: 460px;
    margin-bottom: 16px;
    padding: 6px 10px;
    background: #1a1a1d;
    color: #e8e6df;
    border: 1px solid #ffffff20;
  }
  .cover {
    /* Reserve the 2:3 box before the image arrives so the grid does not reflow as
       covers stream in. */
    aspect-ratio: 2 / 3;
    background: var(--glass);
    overflow: hidden;
    margin-bottom: 8px;
    /* Anchors the unread badge and the progress bar. */
    position: relative;
  }
  .section {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-muted);
    margin: 0 0 10px;
  }
  /* The one place a raw accent bar is right: it is progress, not decoration. */
  .progress {
    position: absolute;
    left: 0;
    bottom: 0;
    height: 3px;
    background: var(--progress);
  }
  /* Unread count, shown only when some are read -- an untouched series would put the
     same number on every card, which tells the reader nothing. */
  .badge {
    position: absolute;
    top: 6px;
    right: 6px;
    min-width: 18px;
    height: 18px;
    padding: 0 5px;
    border-radius: var(--r-full);
    background: var(--accent);
    color: #16150f;
    font-size: 10px;
    font-weight: 700;
    line-height: 18px;
    text-align: center;
  }
  .cover img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }
  .card {
    /* Offscreen cards skip layout and paint entirely. The intrinsic size keeps the
       scrollbar honest, so a ten thousand cover library still scrolls like a list. */
    content-visibility: auto;
    contain-intrinsic-size: auto 300px;
  }
  .card,
  .chapter {
    font: inherit;
    text-align: left;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 10px 12px;
    background: var(--glass);
    color: var(--text);
    border: 1px solid var(--hairline);
    border-radius: var(--r-2);
    cursor: pointer;
    transition: background var(--dur-fast) var(--ease);
  }
  .card:hover,
  .chapter:hover {
    background: var(--glass-hover);
  }
  .card.on {
    border-color: var(--accent);
  }
  .chapters {
    display: flex;
    flex-direction: column;
    gap: 4px;
    max-width: 620px;
  }
  .chapter {
    flex-direction: row;
    justify-content: space-between;
  }
  .meta {
    color: var(--text-muted);
  }
  /* The instrumentation only belongs on screen while something is being read. */
  .hud:not(.reading) {
    display: none;
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
