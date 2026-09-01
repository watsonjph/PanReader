<script>
  import { onMount, tick } from "svelte";
  // Names from the same file the colours come from, so adding a theme still touches
  // data/themes.json and nothing else.
  import themeData from "../../data/themes.json";
  import { invoke, isMock, mockCover, mockPage } from "./ipc.js";
  import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
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
  let listView = $state(false);

  /// The one series the home screen leads with: whatever you are furthest into, or the
  /// newest thing in the library if you have not started anything.
  const featuredResume = $derived(resume[0] ?? null);
  const featured = $derived.by(() => {
    if (featuredResume) {
      const match = series.find((r) => r.id === featuredResume.series_id);
      if (match) return match;
    }
    return [...series].sort((a, b) => b.added_at - a.added_at)[0] ?? null;
  });

  /// Horizontal rows above the grid. Derived from what the shelf already has rather
  /// than from three more queries: the whole library is in hand, and sorting ten
  /// thousand rows client-side is cheaper than three round trips.
  ///
  /// ponytail: fixed composition. DESIGN.md wants these user-configurable and
  /// reorderable; that is a config schema and a drag affordance for a screen nobody has
  /// used yet. Add it when someone wants a different set.
  const rows = $derived.by(() => {
    if (query || activeCat !== null) return [];
    const byId = new Map(series.map((r) => [r.id, r]));
    const continuing = resume
      .map((r) => ({ resume: r, series: byId.get(r.series_id) }))
      .filter((x) => x.series)
      .map((x) => ({
        key: `c${x.resume.chapter_id}`,
        series: x.series,
        resume: x.resume,
        coverId: x.resume.chapter_id,
        note: `${x.resume.page + 1} / ${x.resume.page_count}`,
      }));

    const plain = (r, note) => ({
      key: `s${r.id}`,
      series: r,
      resume: null,
      coverId: r.cover_chapter_id,
      note,
    });

    return [
      { name: "Continue reading", items: continuing },
      {
        name: "Recently added",
        items: [...series]
          .sort((a, b) => b.added_at - a.added_at)
          .slice(0, 12)
          .map((r) => plain(r, `${r.chapter_count} chapters`)),
      },
      {
        name: "Unread",
        items: series
          .filter((r) => r.chapter_count > 0 && r.unread === r.chapter_count)
          .slice(0, 12)
          .map((r) => plain(r, `${r.chapter_count} chapters`)),
      },
    ];
  });

  /// Scroll a rail by most of its width, from a button inside that rail's header.
  function nudge(event, direction) {
    const rail = event.currentTarget.closest(".strip")?.querySelector(".rail");
    if (!rail) return;
    rail.scrollBy({
      left: direction * rail.clientWidth * 0.8,
      behavior: reduceMotion ? "auto" : "smooth",
    });
  }

  /// Which section of the shell is showing. Nav is a view switch, nothing more.
  let section = $state("library");
  let navIcons = $state(false);
  const NAV = [
    { id: "library", name: "Library", icon: "▤" },
    { id: "catalogs", name: "Catalogs", icon: "☁" },
    { id: "history", name: "History", icon: "◷" },
    { id: "settings", name: "Settings", icon: "⚙" },
  ];
  let error = $state(null);

  /// Stacks rather than bundled files. A reading face is a licence entry and a line in
  /// docs/FONTS.md; the system serif is good on every desktop and costs neither.
  const FACES = [
    { id: "serif", name: "Serif" },
    { id: "sans", name: "Sans" },
    { id: "mono", name: "Mono" },
  ];

  // The text reader. It shares the shell, the library, positions, history and
  // bookmarks with the image reader, and shares no rendering with it at all: the
  // browser measures prose and nothing here ever does.
  let text = $state(null);
  let prose = $state(null);
  /// Which block the reader is on, and how far into it. The same pair as the image
  /// reader's page and page_frac, so it persists through the same one-row upsert and
  /// backs up through the same file.
  let block = $state(0);
  let blockFrac = $state(0);
  let textFont = $state("serif");
  let textSize = $state(19);
  let textMeasure = $state(66);
  let textLeading = $state(160);
  let textPaged = $state(false);
  let textPaper = $state(false);
  let textVertical = $state(false);
  let textChrome = $state(false);

  let autoBackup = $state(true);
  let backupKeep = $state(8);

  /// History, bookmarks and the numbers derived from them. Loaded when the section is
  /// opened, not on launch: none of it is on the path to a first page.
  let log = $state([]);
  let marks = $state([]);
  let tally = $state(null);
  /// Which pages of the open chapter are bookmarked. Fetched once when the chapter
  /// opens: asking the backend on every turn would put an IPC round trip on the page
  /// turn path to answer a question a set already answers.
  let chapterMarks = $state(new Set());
  const bookmarked = $derived(chapterMarks.has(text ? block : page));

  let hud = $state({ fps: 0, worst: 0, dropped: 0, mounted: 0, firstPaint: null });
  let rust = $state({});
  let autoscroll = $state(false);

  let detected = $state(null); // what the backend worked out, and why
  let mode = $state("rtl"); // effective mode; a manual pick overrides the detection
  let overridden = $state(false);
  /// The app-wide fallback, which is a different thing from `mode`. `mode` is what the
  /// open chapter is being read as; this is what is used when nothing else knows. They
  /// were the same variable until pressing [m] on one manhwa was found to be rewriting
  /// the default for every unlabelled series in the library.
  let defaultMode = $state("rtl");
  /// The series the open chapter belongs to, wherever it was opened from. The chapter
  /// panel knows it; a resume, history or bookmark row carries it on the row.
  let openSeriesId = $state(null);
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
    isMock ? mockPage(index) : `${base}/t/${chapterId}/${index}/${t}/${layout.display_w}`;

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

  // ---------------------------------------------------------------- DESIGN.md shell

  const THEMES = [
    { id: "system", name: "System" },
    ...Object.entries(themeData)
      .filter(([id]) => id !== "//")
      .map(([id, t]) => ({ id, name: t.name })),
  ];
  let theme = $state("ink");
  let systemDark = $state(true);
  /// Whether the theme in force paints light type on a dark ground.
  const themeIsDark = $derived.by(() => {
    const id = theme === "system" ? (systemDark ? "ink" : "day") : theme;
    return themeData[id]?.dark ?? true;
  });
  let liveBg = $state(true);
  let reduceMotion = $state(false);
  /// Eight "r,g,b" strings from the cover currently on screen, or null for a flat --bg.
  let palette = $state(null);
  let paletteFor = 0;

  /// Gradient positions are constant; only the colours move. DESIGN.md, Signature 1:
  /// the result varies with the art but never with luck.
  const STOPS = [
    "0% 0%",
    "100% 0%",
    "100% 100%",
    "0% 100%",
    "50% 50%",
    "25% 0%",
    "75% 100%",
  ];

  const liveStyle = $derived.by(() => {
    if (!palette) return "";
    // Extracted colours are luminance-capped so *light* type survives them, which is
    // the right rule for a dark theme and exactly wrong for Daylight: the same field
    // that carries white type swallows near-black type. On a light theme the art
    // becomes a wash over --bg instead of a ground of its own.
    const alpha = themeIsDark ? 0.8 : 0.14;
    const layers = STOPS.map(
      (at, i) =>
        `radial-gradient(circle at ${at}, rgba(${palette[i + 1]}, ${alpha}) 0%, transparent 80%)`,
    );
    const base = themeIsDark
      ? `background-color: rgb(${palette[0]});`
      : `background-color: transparent;`;
    return `${base} background-image: ${layers.join(", ")};`;
  });

  /// Apply the theme by class, so the generated CSS does the work and there is no
  /// second copy of any colour in JS.
  function applyTheme() {
    systemDark = window.matchMedia("(prefers-color-scheme: dark)").matches;
    const id = theme === "system" ? (systemDark ? "ink" : "day") : theme;
    document.documentElement.className = `theme-${id}${
      reduceMotion ? " reduce-motion" : ""
    }`;
  }

  /// Pull the palette for whatever is on screen. Guarded by a generation counter: hover
  /// through a shelf and only the last answer is allowed to land.
  async function showLive(chapterId) {
    if (!liveBg || chapterId == null) {
      palette = null;
      return;
    }
    const mine = ++paletteFor;
    try {
      const colours = await invoke("palette", { chapterId });
      if (mine === paletteFor) palette = colours;
    } catch {
      // A cover we cannot read is a flat background, not an error dialog.
      if (mine === paletteFor) palette = null;
    }
  }

  async function load(chapter, keepPage = false) {
    // The one branch in the shell that knows there are two readers. Everything below
    // this line is the image reader; the text reader shares nothing with it but the
    // library, the position table and the chrome around both.
    openSeriesId = chapter.series_id ?? openSeries?.id ?? null;
    if (chapter.kind === "text") return loadText(chapter);

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
    // The reader canvas is black in every theme, and the live background is dropped
    // on the way in. DESIGN.md, The image reader view.
    palette = null;
    error = null;
    chapterMarks = new Set();
    invoke("bookmarks", { chapterId: chapter.id })
      .then((rows) => (chapterMarks = new Set(rows.map((m) => m.page))))
      .catch(() => {});
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

  /// Automatic backups on disk, and the pending import if one is being looked at.
  let saves = $state([]);
  let plan = $state(null);
  let backupBusy = $state(false);

  async function refreshBackups() {
    try {
      saves = await invoke("backups");
    } catch (e) {
      error = String(e);
    }
  }

  /// Write one wherever they ask. Save dialog rather than a fixed location: a backup
  /// they cannot find is a backup they do not have.
  async function exportBackup() {
    error = null;
    try {
      const path = await saveDialog({
        defaultPath: `panreader-${Math.floor(Date.now() / 1000)}.pnbk`,
        filters: [{ name: "PanReader backup", extensions: ["pnbk"] }],
      });
      if (!path) return;
      backupBusy = true;
      await invoke("export_backup", { path });
      await refreshBackups();
    } catch (e) {
      error = String(e);
    } finally {
      backupBusy = false;
    }
  }

  /// Never restore straight from a click. The dry run and the real restore are one code
  /// path in Rust, so what this shows is exactly what confirming does.
  async function previewBackup(path) {
    error = null;
    backupBusy = true;
    try {
      if (!path) {
        path = await openDialog({
          multiple: false,
          filters: [{ name: "PanReader backup", extensions: ["pnbk", "json", "gz"] }],
        });
        if (!path) return;
      }
      plan = { path, report: await invoke("preview_backup", { path }) };
    } catch (e) {
      error = String(e);
    } finally {
      backupBusy = false;
    }
  }

  async function applyBackup() {
    if (!plan) return;
    backupBusy = true;
    try {
      await invoke("import_backup", { path: plan.path });
      plan = null;
      await Promise.all([refreshLibrary(), refreshBackups()]);
      // A restore replaces the settings blob, so the shell has to re-read it.
      await loadSettings();
      watchScan();
    } catch (e) {
      error = String(e);
    } finally {
      backupBusy = false;
    }
  }

  /// Bytes, at the one precision a backup ever needs.
  const weigh = (bytes) => `${(bytes / 1024).toFixed(0)} kB`;
  const when = (seconds) =>
    new Date(seconds * 1000).toLocaleString(undefined, {
      dateStyle: "medium",
      timeStyle: "short",
    });

  async function refreshHistory() {
    try {
      [log, marks, tally] = await Promise.all([
        invoke("history", { limit: 200 }),
        invoke("bookmarks", { chapterId: null }),
        invoke("reading_stats"),
      ]);
    } catch (e) {
      error = String(e);
    }
  }

  /// The log, cut into days. Grouping in the template would re-derive it on every
  /// unrelated state change; this only runs when the log does.
  const byDay = $derived.by(() => {
    const days = [];
    for (const row of log) {
      const key = new Date(row.ended_at).toDateString();
      if (days.at(-1)?.key !== key) days.push({ key, rows: [] });
      days.at(-1).rows.push(row);
    }
    return days;
  });

  const DAY = 86_400_000;
  /// "Today" beats a date, and a date beats a relative age older than a week.
  function dayName(key) {
    const midnight = new Date().setHours(0, 0, 0, 0);
    const back = Math.round((midnight - new Date(key).setHours(0, 0, 0, 0)) / DAY);
    if (back <= 0) return "Today";
    if (back === 1) return "Yesterday";
    if (back < 7) return new Date(key).toLocaleDateString(undefined, { weekday: "long" });
    return new Date(key).toLocaleDateString(undefined, { month: "short", day: "numeric" });
  }

  const clock = (ms) =>
    new Date(ms).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });

  /// Minutes are unreadable past a couple of hours, hours are useless below one. Kept
  /// tight because it shares a row with five plain counts and must not wrap past them.
  const spell = (minutes) =>
    minutes < 60 ? `${minutes}m` : `${Math.floor(minutes / 60)}h ${minutes % 60}m`;

  async function forget(id) {
    try {
      await invoke("forget", { id: id ?? null });
      await refreshHistory();
    } catch (e) {
      error = String(e);
    }
  }

  /// Mark or unmark where the reader is. The image reader has a page and an offset into
  /// it; the text reader will pass a paragraph and a character instead, which is why
  /// both pairs are on the call and both are nullable.
  async function toggleBookmark() {
    if (chapterId === null) return;
    const spot = text ? block : page;
    const frac = text
      ? blockFrac
      : paged || !scroller
        ? 0
        : pageFrac(tops, scroller.scrollTop, page, canvasHeight());
    try {
      const now = await invoke("toggle_bookmark", {
        chapterId,
        page: spot,
        frac,
        paragraph: null,
        charOffset: null,
      });
      // A new Set, not a mutation: the derived flag is watching the reference.
      const next = new Set(chapterMarks);
      next[now ? "add" : "delete"](spot);
      chapterMarks = next;
    } catch (e) {
      error = String(e);
    }
  }

  async function dropBookmark(id) {
    try {
      await invoke("remove_bookmark", { id });
      marks = marks.filter((m) => m.id !== id);
    } catch (e) {
      error = String(e);
    }
  }

  async function noteBookmark(id, note) {
    try {
      await invoke("set_bookmark_note", { id, note });
    } catch (e) {
      error = String(e);
    }
  }

  const coverUrl = (chapterId) =>
    isMock ? mockCover(chapterId) : `${base}/c/${chapterId}/${COVER_W}`;

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

  /// A catalog could be added and never removed, which left the list a one-way door.
  async function dropCatalog(id) {
    try {
      await invoke("remove_catalog", { id });
      catalogs = await invoke("catalogs");
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

  /// Open a chapter of prose.
  ///
  /// Parsed once, in Rust, and handed over as blocks. Changing the font, the size, the
  /// measure or the theme below only ever restyles what is already here -- hard
  /// invariant 9, and the reason none of those settings calls this again.
  async function loadText(chapter) {
    error = null;
    openSeriesId = chapter.series_id ?? openSeries?.id ?? null;
    chapterId = chapter.id;
    chapterTitle = chapter.title;
    palette = null;
    chapterMarks = new Set();
    openedAt = performance.now();
    try {
      text = await invoke("open_text", { chapterId: chapter.id });
      chapterTitle = text.title || chapter.title;
      block = chapter.page ?? text.page ?? 0;
      blockFrac = chapter.page_frac ?? 0;
      invoke("bookmarks", { chapterId: chapter.id })
        .then((rows) => (chapterMarks = new Set(rows.map((m) => m.page))))
        .catch(() => {});
      // The blocks have to exist before there is anything to scroll to.
      await tick();
      goToBlock(block, blockFrac);
      hud.firstPaint = Math.round(performance.now() - openedAt);
    } catch (e) {
      error = String(e);
      text = null;
      chapterId = null;
    }
  }

  /// Land on a block, and a fraction into it.
  ///
  /// `scrollIntoView` rather than arithmetic: the browser knows where the block ended
  /// up after reflow and we do not, which is the whole point of letting it measure.
  function goToBlock(index, frac = 0) {
    const el = prose?.querySelector(`[data-b="${index}"]`);
    if (!el) return;
    if (textPaged) {
      // In columns, the block's offset within the scroller is a column boundary away;
      // snapping to the column that contains it is what a page turn means here.
      const page = Math.floor(el.offsetLeft / prose.clientWidth);
      prose.scrollLeft = page * prose.clientWidth;
    } else {
      prose.scrollTop = el.offsetTop + el.offsetHeight * frac - prose.clientHeight * 0.1;
    }
  }

  /// Where the reader is now: the first block whose bottom is still on screen.
  function readTextPosition() {
    if (!prose || !text) return;
    const blocks = prose.querySelectorAll("[data-b]");

    // At the end, the position is the end. Picking the first block still on screen is
    // right everywhere except the last screenful, where it stalls a few blocks short --
    // and a chapter that can never reach its last block is a chapter that never marks
    // itself read.
    const along = textPaged
      ? prose.scrollLeft + prose.clientWidth >= prose.scrollWidth - 2
      : prose.scrollTop + prose.clientHeight >= prose.scrollHeight - 2;
    if (along) {
      block = text.blocks - 1;
      blockFrac = 1;
      savePosition();
      return;
    }

    if (textPaged) {
      const left = prose.scrollLeft;
      const right = left + prose.clientWidth;
      for (const el of blocks) {
        if (el.offsetLeft + el.offsetWidth > left && el.offsetLeft < right) {
          block = Number(el.dataset.b);
          blockFrac = 0;
          break;
        }
      }
    } else {
      const top = prose.scrollTop + prose.clientHeight * 0.1;
      for (const el of blocks) {
        if (el.offsetTop + el.offsetHeight > top) {
          block = Number(el.dataset.b);
          blockFrac = Math.min(
            Math.max((top - el.offsetTop) / (el.offsetHeight || 1), 0),
            1,
          );
          break;
        }
      }
    }
    savePosition();
  }

  /// The next or previous chapter of the open series, without going back to the shelf.
  ///
  /// Reads from the list the chapter panel already fetched, so it costs nothing and is
  /// silently unavailable if a chapter was opened from history or a bookmark rather
  /// than from its series.
  const neighbours = $derived.by(() => {
    const at = seriesChapters.findIndex((c) => c.id === chapterId);
    if (at < 0) return { prev: null, next: null };
    return { prev: seriesChapters[at - 1] ?? null, next: seriesChapters[at + 1] ?? null };
  });

  function stepChapter(delta) {
    const to = delta < 0 ? neighbours.prev : neighbours.next;
    if (to) load(to);
  }

  /// A page turn in columns, or a screenful in scroll.
  function turnText(direction) {
    if (!prose) return;
    if (textPaged) {
      prose.scrollLeft += direction * prose.clientWidth;
    } else {
      prose.scrollBy({
        top: direction * prose.clientHeight * 0.9,
        behavior: reduceMotion ? "auto" : "smooth",
      });
    }
  }

  async function showSeries(row) {
    openSeries = row;
    showLive(row.cover_chapter_id);
    try {
      seriesChapters = await invoke("chapters", { seriesId: row.id });
      seriesCats = await invoke("categories_of", { seriesId: row.id });
    } catch (e) {
      error = String(e);
    }
  }

  function toLibrary() {
    showLive(openSeries?.cover_chapter_id ?? chapterId);
    chapterId = null;
    text = null;
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
    // Prose counts in blocks where pages count in pages, and both land in the same
    // column: one position table, one backup, one merge rule.
    const at = text ? block : page;
    // Paged mode has no within-page offset. The strip does, and losing it means
    // reopening a webtoon at the top of an eight thousand pixel page.
    const frac = text
      ? blockFrac
      : paged || !scroller
        ? 0
        : pageFrac(tops, scroller.scrollTop, at, canvasHeight());
    const total = text ? text.blocks : pageCount;
    const done = total > 0 && at >= total - 1;
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
          default_reading_mode: defaultMode,
          fit,
          downsample: sample,
          page_padding: pad,
          rotation: rot,
          rotation_lock: rotLock,
          double_page: spread,
          cover_alone: true,
          theme,
          live_background: liveBg,
          reduce_animations: reduceMotion,
          list_view: listView,
          auto_backup: autoBackup,
          backup_keep: Number(backupKeep) || 1,
          text_font: textFont,
          text_size: textSize,
          text_measure: textMeasure,
          text_leading: textLeading,
          text_paged: textPaged,
          text_paper: textPaper,
          text_vertical: textVertical,
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

  /// A display change reflows what is already parsed and keeps your place.
  ///
  /// The two lines that matter: nothing here calls `open_text` again, and nothing here
  /// measures a line of text in JavaScript. The browser reflows, and then we ask it
  /// where the block we were reading ended up.
  async function reflow() {
    persist();
    const was = block;
    const frac = blockFrac;
    await tick();
    goToBlock(was, frac);
  }

  function setTextSize(next) {
    textSize = Math.min(Math.max(next, 12), 40);
    reflow();
  }

  function setTextPaged(next) {
    textPaged = next;
    reflow();
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

  /// Pin the mode where it belongs.
  ///
  /// To the series when there is one, through the override column that `pr-db` already
  /// resolves ahead of the category and the default. Only a chapter opened with no
  /// series in hand falls back to moving the app-wide default, which is the one case
  /// where there is nothing narrower to write it to.
  function setMode(next) {
    mode = next;
    overridden = true;
    if (openSeriesId !== null) {
      invoke("set_series_mode", { seriesId: openSeriesId, mode: next }).catch(
        (e) => (error = String(e)),
      );
    } else {
      defaultMode = next;
      persist();
    }
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

    // Prose has its own small set. None of fit, rotation, downsampling or double pages
    // means anything to a paragraph, and binding them anyway is how a shared reader
    // ends up with settings that do nothing.
    if (text) {
      if (k === "Escape") toLibrary();
      else if (k === "b") toggleBookmark();
      else if (k === "t") setTextPaged(!textPaged);
      else if (k === "-") setTextSize(textSize - 1);
      else if (k === "+" || k === "=") setTextSize(textSize + 1);
      else if (k === "ArrowRight" || k === "PageDown" || k === " ") turnText(1);
      else if (k === "ArrowLeft" || k === "PageUp") turnText(-1);
      else if (k === "Home") goToBlock(0);
      else if (k === "End") goToBlock(text.blocks - 1);
      else if (k === "[") stepChapter(-1);
      else if (k === "]") stepChapter(1);
      else return;
      e.preventDefault();
      return;
    }

    if (k === "Escape") toLibrary();
    else if (k === "s") autoscroll = !autoscroll;
    else if (k === "b") toggleBookmark();
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
      // Clearing the override is a write too, or the series keeps the old one and the
      // reset lasts exactly as long as this chapter is open.
      if (openSeriesId !== null) {
        invoke("set_series_mode", { seriesId: openSeriesId, mode: null }).catch(
          (e) => (error = String(e)),
        );
      }
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

  /// Read the settings blob into the shell. Called at launch, and again after a
  /// restore: settings are one blob and a restore replaces it wholesale.
  async function loadSettings() {
    try {
      const saved = await invoke("settings");
      defaultMode = saved.default_reading_mode;
      mode = defaultMode;
      fit = saved.fit;
      sample = saved.downsample;
      pad = saved.page_padding;
      rot = saved.rotation;
      rotLock = saved.rotation_lock;
      spread = saved.double_page;
      theme = saved.theme ?? "ink";
      liveBg = saved.live_background ?? true;
      reduceMotion = saved.reduce_animations ?? false;
      listView = saved.list_view ?? false;
      autoBackup = saved.auto_backup ?? true;
      backupKeep = saved.backup_keep ?? 8;
      textFont = saved.text_font ?? "serif";
      textSize = saved.text_size ?? 19;
      textMeasure = saved.text_measure ?? 66;
      textLeading = saved.text_leading ?? 160;
      textPaged = saved.text_paged ?? false;
      textPaper = saved.text_paper ?? false;
      textVertical = saved.text_vertical ?? false;
    } catch (e) {
      console.warn("could not load settings:", e);
    }
  }

  onMount(async () => {
    vw = window.innerWidth;
    vh = window.innerHeight;
    // Settings first: the chapter open uses the persisted reading-mode default, so
    // loading them afterwards would detect against the wrong fallback once.
    await loadSettings();
    applyTheme();
    // Following the system means following it while the app is open, not only at
    // launch.
    window
      .matchMedia("(prefers-color-scheme: dark)")
      .addEventListener("change", () => theme === "system" && applyTheme());
    frames();
    await refreshCategories();
    await refreshLibrary();
    // Something to look at before anything is selected.
    showLive(resume[0]?.chapter_id ?? series[0]?.cover_chapter_id);
    if (busy) watchScan();
  });
</script>

<svelte:window on:keydown={onKey} on:resize={onResize} />

<div
  class="scroller"
  class:hidden={paged || text}
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

<!-- The text reader.
     Everything about it that matters is a CSS declaration: the measure is a max-width
     in `ch`, the pagination is `columns`, and the vertical mode is `writing-mode`. The
     browser does every measurement, which is hard invariant 10 -- measuring lines in
     JavaScript on every reflow is the one reliable way to miss the frame budget here. -->
{#if text}
  <div
    class="novel"
    class:paper={textPaper}
    class:columns={textPaged}
    class:vertical={textVertical}
    style="--measure:{textMeasure}ch; --prose-size:{textSize}px; --prose-leading:{textLeading /
      100}; --prose-face:var(--face-{textFont})"
  >
    <div
      class="prose"
      bind:this={prose}
      onscroll={readTextPosition}
      tabindex="-1"
      role="document"
    >
      <div class="column">
        {#each text.document.blocks as b, i (i)}
          {#if b.kind === "divider"}
            <hr data-b={i} />
          {:else if b.kind === "quote"}
            <blockquote data-b={i}>{@render runs(b.spans)}</blockquote>
          {:else if b.kind?.heading}
            <h3 data-b={i} class="h{b.kind.heading}">{@render runs(b.spans)}</h3>
          {:else}
            <p data-b={i}>{@render runs(b.spans)}</p>
          {/if}
        {/each}
      </div>
    </div>

    <!-- Reading chrome, hidden until asked for. Someone reading a novel is looking at
         one column of text for an hour; a toolbar in the corner of that is furniture. -->
    <button class="leave" onclick={toLibrary} title="Library [esc]" aria-label="Back to library">
      ←
    </button>

    <div class="typeset" class:open={textChrome}>
      <button class="chip" onclick={() => (textChrome = !textChrome)} aria-expanded={textChrome}>
        {textChrome ? "✕" : "Aa"}
      </button>
      {#if textChrome}
        <div class="set">
          <label>
            Size
            <input
              type="range"
              min="12"
              max="40"
              value={textSize}
              oninput={(e) => setTextSize(Number(e.currentTarget.value))}
            />
          </label>
          <label>
            Measure
            <input
              type="range"
              min="40"
              max="100"
              value={textMeasure}
              oninput={(e) => {
                textMeasure = Number(e.currentTarget.value);
                reflow();
              }}
            />
          </label>
          <label>
            Leading
            <input
              type="range"
              min="120"
              max="220"
              step="5"
              value={textLeading}
              oninput={(e) => {
                textLeading = Number(e.currentTarget.value);
                reflow();
              }}
            />
          </label>
          <div class="chips">
            {#each FACES as face (face.id)}
              <button
                class="chip"
                class:on={textFont === face.id}
                aria-pressed={textFont === face.id}
                onclick={() => {
                  textFont = face.id;
                  reflow();
                }}>{face.name}</button
              >
            {/each}
          </div>
          <div class="chips">
            <button
              class="chip"
              class:on={textPaged}
              aria-pressed={textPaged}
              onclick={() => setTextPaged(!textPaged)}>Pages [t]</button
            >
            <button
              class="chip"
              class:on={textPaper}
              aria-pressed={textPaper}
              onclick={() => {
                textPaper = !textPaper;
                persist();
              }}>Paper</button
            >
            <button
              class="chip"
              class:on={textVertical}
              aria-pressed={textVertical}
              onclick={() => {
                textVertical = !textVertical;
                reflow();
              }}>縦書き</button
            >
          </div>
        </div>
      {/if}
    </div>

    <!-- One line, always visible: which chapter, and how far in. A scrubber would be
         the image reader's koma strip, and prose has no pages to draw. -->
    <div class="thread">
      <b>{chapterTitle}</b>
      <span class="step">
        <button
          class="chip ghost"
          disabled={!neighbours.prev}
          title={neighbours.prev ? neighbours.prev.title : "First chapter"}
          onclick={() => stepChapter(-1)}>‹</button
        >
        <span class="meta">{Math.round(((block + 1) / text.blocks) * 100)}%</span>
        <button
          class="chip ghost"
          disabled={!neighbours.next}
          title={neighbours.next ? neighbours.next.title : "Last chapter"}
          onclick={() => stepChapter(1)}>›</button
        >
      </span>
    </div>
  </div>
{/if}

{#snippet runs(spans)}{#each spans as span, i (i)}{#if span.em && span.strong}<em
        ><strong>{span.text}</strong></em
      >{:else if span.em}<em>{span.text}</em>{:else if span.strong}<strong>{span.text}</strong
      >{:else}{span.text}{/if}{/each}{/snippet}

{#if error}
  <p class="error">{error}</p>
{/if}

{#if chapterId === null}
  <!-- Signature 1. Its own layer rather than a background on .library, so the
       cross-fade is a compositor opacity change and never repaints the shelf. -->
  <div class="live" style={liveStyle} aria-hidden="true"></div>

  <div class="shell">
    <!-- Nav only, collapsible to icons. DESIGN.md, Layout shell. -->
    <nav class="sidebar" class:icons={navIcons} aria-label="Sections">
      <button
        class="nav collapse"
        onclick={() => (navIcons = !navIcons)}
        aria-expanded={!navIcons}
        title={navIcons ? "Expand" : "Collapse"}
      >
        <span class="ico">{navIcons ? "»" : "«"}</span>
        {#if !navIcons}<span class="label">PanReader</span>{/if}
      </button>

      {#each NAV as item (item.id)}
        <button
          class="nav"
          class:on={section === item.id}
          aria-current={section === item.id ? "page" : undefined}
          title={navIcons ? item.name : null}
          onclick={() => {
            section = item.id;
            if (item.id === "history") refreshHistory();
            if (item.id === "settings") refreshBackups();
          }}
        >
          <span class="tick" aria-hidden="true"></span>
          <span class="ico">{item.icon}</span>
          {#if !navIcons}<span class="label">{item.name}</span>{/if}
        </button>
      {/each}
    </nav>

    <main class="main">
      {#if error}
        <p class="error" role="alert">{error}</p>
      {/if}

      {#if section === "library"}
        <header class="bar">
          <h1>Library</h1>
          <input
            class="search"
            placeholder="Search series"
            bind:value={query}
            oninput={onSearch}
          />
        </header>

        <div class="chips">
          <button class="chip" class:on={activeCat === null} onclick={() => filterBy(null)}>
            All
          </button>
          {#each cats as cat (cat.id)}
            <button
              class="chip"
              class:on={activeCat === cat.id}
              onclick={() => filterBy(cat.id)}
            >
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
              <input
                placeholder="New category"
                bind:value={newCat}
                onkeydown={(e) => e.key === "Enter" && addCategory()}
              />
              <button class="chip accent" onclick={addCategory}>Add</button>
            </div>
            {#each cats as cat (cat.id)}
              <div class="row">
                <span class="name">{cat.name}</span>
                <!-- A category with no mode leaves detection alone; that is the
                     default and the common case. -->
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

        <!-- The showcase: one large cover bleeding to the edges of a rounded panel,
             the title at --text-2xl, one primary action. DESIGN.md, Layout shell. -->
        {#if featured && !query && activeCat === null}
          <section class="showcase">
            {#if base && featured.cover_chapter_id !== null}
              <img
                class="art"
                src={coverUrl(featured.cover_chapter_id)}
                alt=""
                decoding="async"
              />
            {/if}
            <div class="veil" aria-hidden="true"></div>
            <div class="say">
              <span class="eyebrow">
                {featuredResume ? "Continue reading" : "In your library"}
              </span>
              <h2>{featured.title}</h2>
              <span class="meta">
                {#if featuredResume}
                  {featuredResume.chapter_title} · page {featuredResume.page + 1} of
                  {featuredResume.page_count}
                {:else}
                  {featured.chapter_count} chapter{featured.chapter_count === 1 ? "" : "s"}
                  {#if featured.unread > 0}· {featured.unread} unread{/if}
                {/if}
              </span>
              <div class="chips">
                <button
                  class="chip accent"
                  onclick={() =>
                    featuredResume
                      ? load({
                          id: featuredResume.chapter_id,
                          title: featuredResume.chapter_title,
                          page: featuredResume.page,
                          page_frac: featuredResume.page_frac,
                        })
                      : showSeries(featured)}
                >
                  {featuredResume ? "Resume" : "Open"}
                </button>
                <button class="chip" onclick={() => showSeries(featured)}>Chapters</button>
              </div>
            </div>
          </section>
        {/if}

        {#each rows as row (row.name)}
          {#if row.items.length}
            <section class="strip">
              <header class="strip-head">
                <h2 class="section">{row.name}</h2>
                <div class="chips">
                  <button class="chip" aria-label="Scroll {row.name} left"
                    onclick={(e) => nudge(e, -1)}>‹</button>
                  <button class="chip" aria-label="Scroll {row.name} right"
                    onclick={(e) => nudge(e, 1)}>›</button>
                </div>
              </header>
              <div class="rail">
                {#each row.items as row_item (row_item.key)}
                  <button
                    class="card"
                    class:on={openSeries?.id === row_item.series.id}
                    onclick={() =>
                      row_item.resume
                        ? load({
                            id: row_item.resume.chapter_id,
                            title: row_item.resume.chapter_title,
                            page: row_item.resume.page,
                            page_frac: row_item.resume.page_frac,
                          })
                        : showSeries(row_item.series)}
                  >
                    <div class="cover">
                      {#if base && row_item.coverId !== null}
                        <img
                          src={coverUrl(row_item.coverId)}
                          alt=""
                          loading="lazy"
                          decoding="async"
                        />
                      {/if}
                      {#if row_item.resume}
                        <div
                          class="progress"
                          style="width:{((row_item.resume.page + 1) /
                            row_item.resume.page_count) * 100}%"
                        ></div>
                      {:else if row_item.series.unread > 0 && row_item.series.unread < row_item.series.chapter_count}
                        <span class="badge">{row_item.series.unread}</span>
                      {/if}
                    </div>
                    <b>{row_item.series.title}</b>
                    <span class="meta">{row_item.note}</span>
                  </button>
                {/each}
              </div>
            </section>
          {/if}
        {/each}

        <header class="strip-head">
          <h2 class="section">
            {query || activeCat !== null ? "Results" : "All series"}
          </h2>
          <button
            class="chip"
            aria-pressed={listView}
            onclick={() => {
              listView = !listView;
              persist();
            }}>{listView ? "Grid" : "List"}</button
          >
        </header>

        {#if series.length === 0 && !busy}
          <p class="empty">
            {query
              ? `Nothing matches “${query}”.`
              : "Add a folder to start your library."}
          </p>
        {/if}

        <div class="shelf" class:list={listView}>
          {#each series as row (row.id)}
            <button
              class="card"
              class:on={openSeries?.id === row.id}
              onclick={() => showSeries(row)}
            >
              <div class="cover">
                {#if row.cover_chapter_id !== null && base}
                  <img
                    src={coverUrl(row.cover_chapter_id)}
                    alt=""
                    loading="lazy"
                    decoding="async"
                  />
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
      {/if}

      {#if section === "catalogs"}
        <header class="bar">
          <h1>{opds ? opds.feed.title || "Catalog" : "Catalogs"}</h1>
          {#if opds}
            <div class="chips">
              {#if opdsTrail.length}
                <button class="chip" onclick={opdsBack}>Back</button>
              {/if}
              <button
                class="chip ghost"
                onclick={() => {
                  opds = null;
                  opdsTrail = [];
                }}>Close</button
              >
            </div>
          {/if}
        </header>

        {#if !opds}
          <div class="row">
            <input
              placeholder="OPDS catalog URL (Komga, Kavita, Calibre-Web)"
              bind:value={catalogUrl}
              onkeydown={(e) => e.key === "Enter" && addCatalog()}
            />
            <button class="chip accent" onclick={addCatalog}>Add catalog</button>
            {#if opdsBusy}<span class="meta">working…</span>{/if}
          </div>

          {#if catalogs.length === 0}
            <p class="empty">Add an OPDS catalog to browse it.</p>
          {/if}

          <div class="chips">
            {#each catalogs as cat (cat.id)}
              <span class="pair">
                <button
                  class="chip"
                  onclick={() => {
                    opdsTrail = [];
                    openFeed(cat.url, false);
                  }}>{cat.name}</button
                >
                <button
                  class="chip danger"
                  title="Remove {cat.name}"
                  aria-label="Remove {cat.name}"
                  onclick={() => dropCatalog(cat.id)}>×</button
                >
              </span>
            {/each}
          </div>
        {:else}
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
              <button class="card" onclick={() => (nav ? openFeed(nav.href) : grab(entry))}>
                <div class="cover">
                  {#if entry.thumbnail}
                    <img src={entry.thumbnail} alt="" loading="lazy" decoding="async" />
                  {/if}
                </div>
                <b>{entry.title}</b>
                <span class="meta">{nav ? "Browse" : (entry.author ?? "Download")}</span>
              </button>
            {/each}
          </div>

          {#if opds.feed.next}
            <button class="chip" onclick={() => openFeed(opds.feed.next)}>Next page</button>
          {/if}
        {/if}
      {/if}

      {#if section === "history"}
        <header class="bar">
          <h1>History</h1>
          {#if log.length}
            <button class="chip danger" onclick={() => forget(null)}>Clear history</button>
          {/if}
        </header>

        <!-- Derived from the log every time this renders. Deleting history therefore
             resets these honestly instead of leaving a stale number behind. -->
        {#if tally}
          <div class="tally">
            <div><b>{tally.chapters}</b><span class="meta">chapters</span></div>
            <div><b>{tally.pages}</b><span class="meta">pages</span></div>
            <div><b>{spell(tally.minutes)}</b><span class="meta">reading</span></div>
            <div><b>{tally.days}</b><span class="meta">days</span></div>
            <div><b>{tally.streak}</b><span class="meta">day streak</span></div>
            <div><b>{tally.best_streak}</b><span class="meta">best streak</span></div>
          </div>
        {/if}

        {#if marks.length}
          <h2 class="section">Bookmarks</h2>
          <ul class="rows">
            {#each marks as mark (mark.id)}
              <li>
                <button
                  class="entry"
                  onclick={() =>
                    load({
                      id: mark.chapter_id,
                      title: mark.chapter_title,
                      page: mark.page,
                      page_frac: mark.page_frac,
                    })}
                >
                  <span class="what">
                    <b>{mark.series_title}</b>
                    <span class="meta">{mark.chapter_title} · page {mark.page + 1}</span>
                  </span>
                </button>
                <input
                  class="note"
                  value={mark.note}
                  placeholder="Note"
                  aria-label="Note on {mark.series_title}, page {mark.page + 1}"
                  onchange={(e) => noteBookmark(mark.id, e.currentTarget.value)}
                />
                <button
                  class="chip"
                  aria-label="Remove bookmark"
                  onclick={() => dropBookmark(mark.id)}>×</button
                >
              </li>
            {/each}
          </ul>
        {/if}

        {#each byDay as day (day.key)}
          <h2 class="section">{dayName(day.key)}</h2>
          <ul class="rows">
            {#each day.rows as row (row.id)}
              <li>
                <button
                  class="entry"
                  onclick={() =>
                    load({ id: row.chapter_id, title: row.chapter_title, page: row.last_page })}
                >
                  {#if base && row.cover_chapter_id !== null}
                    <img class="thumb" src={coverUrl(row.cover_chapter_id)} alt="" loading="lazy" />
                  {/if}
                  <span class="what">
                    <b>{row.series_title}</b>
                    <span class="meta">{row.chapter_title} · {row.pages} pages</span>
                  </span>
                  <span class="meta">{clock(row.ended_at)}</span>
                </button>
                <button class="chip" aria-label="Forget this" onclick={() => forget(row.id)}
                  >×</button
                >
              </li>
            {/each}
          </ul>
        {/each}

        {#if !log.length && !marks.length}
          <p class="meta empty">Nothing read yet. Open a chapter and it shows up here.</p>
        {/if}
      {/if}

      {#if section === "settings"}
        <header class="bar"><h1>Settings</h1></header>

        <h2 class="section">Library folders</h2>
        {#each libraryRoots as root (root)}
          <div class="row">
            <span class="name grow">{root}</span>
            <button
              class="chip danger"
              onclick={async () => {
                await invoke("remove_root", { path: root });
                refreshLibrary();
              }}>Remove</button
            >
          </div>
        {/each}
        <div class="row">
          <button class="chip accent" onclick={pickFolder}>Add folder…</button>
          <input
            placeholder="…or paste a path"
            bind:value={rootInput}
            onkeydown={(e) => e.key === "Enter" && addRoot()}
          />
          <button
            class="chip"
            onclick={async () => {
              await invoke("rescan");
              watchScan();
            }}>Rescan</button
          >
          {#if busy}<span class="meta">scanning…</span>{/if}
        </div>

        <h2 class="section">Theme</h2>
        <div class="chips">
          {#each THEMES as t (t.id)}
            <button
              class="chip"
              class:on={theme === t.id}
              aria-pressed={theme === t.id}
              onclick={() => {
                theme = t.id;
                applyTheme();
                persist();
              }}
            >
              {t.name}
            </button>
          {/each}
        </div>

        <h2 class="section">Appearance</h2>
        <div class="chips">
          <button
            class="chip"
            class:on={liveBg}
            aria-pressed={liveBg}
            onclick={() => {
              liveBg = !liveBg;
              if (!liveBg) palette = null;
              else showLive(openSeries?.cover_chapter_id ?? resume[0]?.chapter_id);
              persist();
            }}>Live background</button
          >
          <button
            class="chip"
            class:on={reduceMotion}
            aria-pressed={reduceMotion}
            onclick={() => {
              reduceMotion = !reduceMotion;
              applyTheme();
              persist();
            }}>Reduce motion</button
          >
        </div>

        <h2 class="section">Backup</h2>
        <p class="meta lede">
          What the rows mean, not a copy of the database: library, progress, history,
          bookmarks, categories, catalogs and settings. No pages, no covers.
        </p>
        <div class="row">
          <button class="chip accent" disabled={backupBusy} onclick={exportBackup}>
            Export…
          </button>
          <button class="chip" disabled={backupBusy} onclick={() => previewBackup(null)}>
            Restore from a file…
          </button>
          <button
            class="chip"
            class:on={autoBackup}
            aria-pressed={autoBackup}
            onclick={() => {
              autoBackup = !autoBackup;
              persist();
            }}>Automatic</button
          >
          {#if autoBackup}
            <label class="meta keep">
              keep
              <input
                type="number"
                min="1"
                max="99"
                bind:value={backupKeep}
                onchange={persist}
              />
            </label>
          {/if}
        </div>

        <!-- The dry run. Shown before anything is written, every time. -->
        {#if plan}
          <div class="plan">
            <b>{plan.path.split(/[\\/]/).pop()}</b>
            <p class="meta">
              {plan.report.series_added} new series and {plan.report.chapters_added} new chapters;
              {plan.report.series_matched} series and {plan.report.chapters_matched} chapters
              already here. {plan.report.positions_advanced} positions move forward,
              {plan.report.positions_kept} stay where they are.
              {plan.report.bookmarks_added} bookmarks and {plan.report.sessions_added} sessions
              added. Settings are replaced.
            </p>
            <div class="chips">
              <button class="chip accent" disabled={backupBusy} onclick={applyBackup}>
                Restore
              </button>
              <button class="chip" onclick={() => (plan = null)}>Cancel</button>
            </div>
          </div>
        {/if}

        {#each saves as save (save.path)}
          <div class="row">
            <span class="name grow">{when(save.taken_at)}</span>
            <span class="meta">{weigh(save.bytes)}</span>
            <button class="chip" disabled={backupBusy} onclick={() => previewBackup(save.path)}>
              Restore
            </button>
          </div>
        {/each}
      {/if}
    </main>

    <!-- Side panel: the chapter list, dismissible. Shared by both readers when the
         text reader lands, which is why it is a region and not a section of main. -->
    {#if openSeries}
      <aside class="panel" aria-label="Chapters">
        <header class="panel-head">
          <b>{openSeries.title}</b>
          <button class="chip ghost" onclick={() => (openSeries = null)} title="Close">
            ✕
          </button>
        </header>

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
            <button class="chapter" class:read={c.completed} onclick={() => load(c)}>
              <span class="name">{c.title}</span>
              <span class="meta">
                {c.page_count} pages
                {#if c.completed}· read{:else if c.page > 0}· page {c.page + 1}{/if}
              </span>
            </button>
          {/each}
        </div>
      </aside>
    {/if}

  <!-- Signature 2. A floating card, not a docked strip: inset, translucent, and the
       only element in the app carrying --shadow-float. Present on every library
       screen, absent inside the reader. -->
  {#if resume.length}
    {@const r = resume[0]}
    <button
      class="resume-bar"
      onclick={() =>
        load({
          id: r.chapter_id,
          title: r.chapter_title,
          page: r.page,
          page_frac: r.page_frac,
        })}
    >
      {#if base}
        <img class="thumb" src={coverUrl(r.chapter_id)} alt="" decoding="async" />
      {/if}
      <span class="what">
        <b>{r.series_title}</b>
        <span class="meta">{r.chapter_title}</span>
      </span>
      <span class="count">{r.page + 1} / {r.page_count}</span>
      <span class="rule" aria-hidden="true">
        <span style="width:{((r.page + 1) / r.page_count) * 100}%"></span>
      </span>
    </button>
  {/if}
  </div>

{/if}

<!-- Signature 3: koma progress. Not a percentage and not a scrubber. -->
{#if chapterId !== null && !text && pageCount > 0 && pageCount <= 400}
  <div class="koma" class:left={rtl} class:right={!rtl} aria-hidden="true">
    {#each { length: pageCount } as _, i (i)}
      <i class:on={i === page} class:done={i < page}></i>
    {/each}
  </div>
{/if}

<div class="hud" class:reading={chapterId !== null && !text}>
  <b>{chapterTitle}</b>
  <button onclick={toLibrary}>library [esc]</button>
  <button onclick={toggleBookmark}>{bookmarked ? "unmark" : "bookmark"} [b]</button>
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
  /* ------------------------------------------------------------------ text reader
   *
   * The whole layout engine of the text reader is here, and that is the point. The
   * measure is a max-width in `ch`, so it tracks the font; pagination is `columns`, so
   * the browser breaks the text; vertical Japanese is `writing-mode`. Nothing in
   * JavaScript measures a glyph. */
  /* `novel`, not `reading`: the debug HUD already carries `class:reading` as a state
     flag, so a bare rule of that name applied to it too and turned it into a
     full-screen blurred panel over the page. `styles.test.js` fails on that shape now. */
  .novel {
    position: fixed;
    inset: 0;
    z-index: 5;
    display: flex;
    background: var(--bg);
    color: var(--text);
    --face-serif: "Iowan Old Style", "Palatino Linotype", Palatino, Georgia,
      "Noto Serif JP", serif;
    --face-sans: var(--font-body);
    --face-mono: var(--font-data);
  }
  /* A warm ground for a long session. Not a fourth theme -- it never leaves this
     surface -- but it does have to redefine the tokens rather than just the background:
     the chrome inside the reader is drawn from --glass, --hairline and --text-muted,
     all of which are tuned for a dark ground and vanish on a light one. Redefining them
     here cascades to every control inside without touching the shell. */
  .novel.paper {
    --bg: #f3ece0;
    --text: #241f19;
    --text-muted: #6b6154;
    --hairline: rgba(36, 31, 25, 0.16);
    --glass: rgba(36, 31, 25, 0.06);
    --glass-hover: rgba(36, 31, 25, 0.12);
    --shadow-float: 0 12px 40px rgba(36, 31, 25, 0.18);
    background: var(--bg);
    color: var(--text);
  }

  .prose {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    overflow-x: hidden;
    padding: var(--s-7) var(--s-5);
    scrollbar-width: none;
  }
  .prose::-webkit-scrollbar {
    display: none;
  }
  .column {
    max-width: var(--measure);
    margin: 0 auto;
    font: var(--prose-size) / var(--prose-leading) var(--prose-face);
    /* Hyphenation matters far more in columns, where a short line has nowhere to go. */
    hyphens: auto;
    text-wrap: pretty;
  }

  /* Pagination. `columns` with the container's own width means one screenful is one
     column, so scrolling by clientWidth is a page turn and the browser decided where
     every break falls. */
  .novel.columns .prose {
    overflow-x: auto;
    overflow-y: hidden;
    scroll-behavior: smooth;
  }
  .novel.columns .column {
    height: 100%;
    max-width: none;
    column-width: var(--measure);
    column-gap: var(--s-7);
    column-fill: auto;
  }
  /* Raw Japanese. One declaration, and the columns above become horizontal bands of a
     vertical text -- which is why it was worth doing at all. */
  .novel.vertical .column {
    writing-mode: vertical-rl;
    height: 100%;
    max-width: none;
  }

  .column p,
  .column blockquote,
  .column h3 {
    margin: 0 0 var(--s-4);
    /* An orphan or a widow in a paged view is the difference between typeset and
       dumped, and it costs one declaration each. */
    orphans: 2;
    widows: 2;
  }
  .column blockquote {
    padding-left: var(--s-4);
    border-left: 2px solid var(--hairline);
    font-style: italic;
  }
  .column h3 {
    font-weight: 600;
    margin-top: var(--s-6);
    break-after: avoid-column;
  }
  .column .h1 {
    font-size: 1.6em;
  }
  .column .h2 {
    font-size: 1.35em;
  }
  /* A scene break, drawn rather than ruled: a hairline across the measure reads as a
     divider in a UI and as a mistake in a novel. */
  .column hr {
    border: 0;
    margin: var(--s-6) 0;
    text-align: center;
  }
  .column hr::before {
    content: "* * *";
    letter-spacing: 0.4em;
    opacity: 0.5;
  }

  /* Both bits of chrome sit at the same inset, one per corner, and neither takes a
     shadow: the prose underneath is the thing being looked at. */
  .leave {
    position: absolute;
    top: var(--s-4);
    left: var(--s-4);
    width: 32px;
    height: 32px;
    border: 0;
    border-radius: var(--r-full);
    background: var(--glass);
    color: var(--text-muted);
    font: var(--text-base) / 1 var(--font-data);
    cursor: pointer;
  }
  .leave:hover {
    background: var(--glass-hover);
    color: var(--text);
  }

  .typeset {
    position: absolute;
    top: var(--s-4);
    right: var(--s-4);
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: var(--s-2);
  }
  /* A sheet along the bottom rather than a panel in the corner. The corner is where
     the measure is, and typography controls that cover the text they are adjusting are
     controls you have to close to use. */
  .typeset .set {
    position: fixed;
    left: 50%;
    bottom: var(--s-6);
    transform: translateX(-50%);
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: center;
    gap: var(--s-3) var(--s-5);
    max-width: min(56rem, calc(100vw - 2 * var(--s-5)));
    padding: var(--s-3) var(--s-5);
    border: 1px solid var(--hairline);
    border-radius: var(--r-3);
    /* Nearly opaque, unlike the rest of the app's chrome: this one sits directly on
       body text, and prose showing through a control panel reads as a rendering bug. */
    background: color-mix(in srgb, var(--bg) 97%, transparent);
    backdrop-filter: blur(var(--blur-chrome));
    box-shadow: var(--shadow-float);
  }
  .typeset label {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    font: var(--text-xs) / 1 var(--font-data);
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }
  /* The app's `input` rule sets `flex: 1 1 280px` for text fields in horizontal rows.
     These labels stack, so that basis lands on the height and each slider comes out
     280px tall. */
  .typeset input[type="range"] {
    flex: none;
    width: 7rem;
    padding: 0;
    border: 0;
    background: transparent;
    accent-color: var(--accent);
  }

  .thread {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    display: flex;
    justify-content: space-between;
    gap: var(--s-3);
    padding: var(--s-2) var(--s-5);
    font: var(--text-sm) / 1 var(--font-body);
    color: var(--text-muted);
    pointer-events: none;
  }
  /* The bar itself passes clicks through to the prose; only its buttons take them. */
  .thread .step {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    pointer-events: auto;
  }
  .thread b {
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* Six numbers, equal weight, no chartjunk. They are counts, so they read as counts,
     in --font-data for the tabular figures. */
  .tally {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(7rem, 1fr));
    gap: var(--s-3);
    margin-bottom: var(--s-6);
  }
  .tally div {
    display: flex;
    flex-direction: column;
    gap: var(--s-1);
    padding: var(--s-4);
    border-radius: var(--r-2);
    background: var(--raised);
  }
  .tally b {
    font: var(--text-lg) / var(--leading-tight) var(--font-data);
    font-variant-numeric: tabular-nums;
    color: var(--text);
    /* "21h 24m" is the widest of the six and the only one that can break. A wrapped
       count reads as two counts. */
    white-space: nowrap;
  }

  .rows {
    list-style: none;
    margin: 0 0 var(--s-6);
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .rows li {
    display: flex;
    align-items: center;
    gap: var(--s-2);
  }
  /* The entry takes the width; its trailing controls do not grow. */
  .entry {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: var(--s-3);
    padding: var(--s-2) var(--s-3);
    border: 0;
    border-radius: var(--r-2);
    background: transparent;
    color: var(--text);
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .entry:hover {
    background: var(--glass);
  }
  .entry .thumb {
    width: 32px;
    height: 48px;
    object-fit: cover;
    border-radius: var(--r-1);
    flex: none;
  }
  .entry .what {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  /* Series and chapter names can hold CJK, so --font-body, never --font-display. */
  .entry .what b {
    font: 600 var(--text-base) / var(--leading-tight) var(--font-body);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .note {
    width: 12rem;
    flex: none;
  }
  /* A thing and the button that removes it, so they wrap as one. */
  .pair {
    display: inline-flex;
    align-items: center;
    gap: var(--s-1);
  }
  .lede {
    max-width: 60ch;
    margin: 0 0 var(--s-3);
  }
  .keep input {
    width: 4rem;
    margin-left: var(--s-2);
  }
  /* A restore is the one destructive thing in the app, so its preview is the one place
     that gets a panel rather than a line. */
  .plan {
    margin: var(--s-3) 0;
    padding: var(--s-4);
    border: 1px solid var(--hairline);
    border-radius: var(--r-2);
    background: var(--raised);
  }
  .plan p {
    max-width: 60ch;
    margin: var(--s-2) 0 var(--s-3);
  }

  /* DESIGN.md's token layer. The full themes-as-data generator is S2; these are the
     same names, so that work becomes a swap rather than a rewrite. */
  /* Signature 1. Fixed behind everything, cross-fading on --dur-base when the
     selection changes. It never animates its gradients -- only its opacity -- so the
     transition is a compositor job and the shelf above it is not repainted. */
  /* The showcase. One cover bleeding to the edges of a rounded panel, the title at the
     largest step, one primary action. DESIGN.md, Layout shell. */
  .showcase {
    position: relative;
    display: flex;
    align-items: flex-end;
    min-height: 260px;
    margin-bottom: var(--s-6);
    padding: var(--s-5);
    border-radius: var(--r-3);
    border: 1px solid var(--hairline);
    overflow: hidden;
  }
  /* The cover itself, bleeding to the panel edges -- not a blurred wash of it. The
     art is the point; a blur would make this a second live background. */
  .showcase .art {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    object-fit: cover;
    /* Covers are portrait and the panel is wide, so bias to the upper third, which is
       where cover art puts its subject. */
    object-position: 50% 25%;
  }
  /* Type sits on art, so it gets a scrim rather than hoping the art is dark. Left to
     right, because the type is on the left and the art should survive on the right. */
  .showcase .veil {
    position: absolute;
    inset: 0;
    background:
      linear-gradient(
        to right,
        var(--scrim) 0%,
        color-mix(in srgb, var(--scrim) 85%, transparent) 45%,
        transparent 100%
      ),
      linear-gradient(to top, var(--scrim) 0%, transparent 60%);
  }
  .showcase .say {
    position: relative;
    max-width: 60ch;
    display: flex;
    flex-direction: column;
    gap: var(--s-2);
  }
  .showcase .eyebrow {
    font: 600 var(--text-xs) / 1 var(--font-display);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-muted);
  }
  /* A series title, so --font-body. The mono face has no CJK coverage. */
  .showcase h2 {
    margin: 0;
    font: 600 var(--text-2xl) / var(--leading-tight) var(--font-body);
  }

  /* Horizontal rows. Hidden scrollbar, paired arrows, snap so a nudge lands on a card
     edge rather than mid-cover. */
  .strip {
    margin-bottom: var(--s-5);
  }
  .strip-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--s-3);
  }
  .rail {
    display: grid;
    grid-auto-flow: column;
    grid-auto-columns: 150px;
    gap: var(--s-3);
    overflow-x: auto;
    scroll-snap-type: x proximity;
    scrollbar-width: none;
    padding-bottom: var(--s-1);
  }
  .rail::-webkit-scrollbar {
    display: none;
  }
  .rail > .card {
    scroll-snap-align: start;
  }

  /* Grid and list share the card markup exactly, so the toggle flips one class rather
     than re-rendering every card. DESIGN.md, Quality floor. */
  .shelf.list {
    display: flex;
    flex-direction: column;
    gap: var(--s-1);
  }
  .shelf.list .card {
    flex-direction: row;
    align-items: center;
    gap: var(--s-3);
    padding: var(--s-1) var(--s-2);
    border-radius: var(--r-1);
  }
  .shelf.list .cover {
    width: 32px;
    flex: none;
    margin-bottom: 0;
  }
  .shelf.list b {
    margin-top: 0;
    flex: 1;
  }

  /* Four regions, and only the main area scrolls. DESIGN.md, Layout shell. */
  .shell {
    position: fixed;
    inset: 0;
    z-index: 1;
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    /* One row, and every region placed explicitly. The resume bar shares main's cell
       rather than being auto-placed, which would otherwise push the side panel into a
       second row it was never meant to have. */
    grid-template-rows: 100%;
    /* No background: depth is translucency over the live layer, so an opaque fill
       here would paint Signature 1 out. */
  }
  .sidebar {
    grid-column: 1;
    grid-row: 1;
    display: flex;
    flex-direction: column;
    gap: 2px;
    width: 208px;
    padding: var(--s-3) var(--s-2);
    border-right: 1px solid var(--hairline);
    overflow-y: auto;
    transition: width var(--dur-base) var(--ease);
  }
  .sidebar.icons {
    width: 56px;
  }
  /* kopuz's nav row, in our tokens: a rounded row that fills on hover and stays
     filled when active, with a short tick growing in on the leading edge. */
  .nav {
    position: relative;
    display: flex;
    align-items: center;
    gap: var(--s-3);
    padding: var(--s-3);
    border: 0;
    border-radius: var(--r-1);
    background: transparent;
    color: var(--text-muted);
    /* A label we author, in Latin, so the display face is right here. User content
       never gets it. */
    font: 500 var(--text-sm) / 1 var(--font-display);
    text-align: left;
    cursor: pointer;
    transition:
      background var(--dur-fast) var(--ease),
      color var(--dur-fast) var(--ease);
  }
  .nav:hover {
    background: var(--glass);
    color: var(--text);
  }
  .nav.on {
    background: var(--glass-hover);
    color: var(--text);
  }
  .nav .tick {
    position: absolute;
    left: 0;
    width: 2px;
    height: 0;
    border-radius: 0 2px 2px 0;
    background: var(--accent);
    transition: height var(--dur-base) var(--ease);
  }
  .nav:hover .tick {
    height: 14px;
  }
  .nav.on .tick {
    height: 22px;
  }
  .nav .ico {
    width: 20px;
    text-align: center;
    font-size: 15px;
    flex: none;
  }
  .nav.collapse {
    color: var(--text-muted);
    margin-bottom: var(--s-3);
  }

  .main {
    grid-column: 2;
    grid-row: 1;
    overflow-y: auto;
    overflow-x: hidden;
    padding: var(--s-5);
    /* Clearance for the floating resume bar, which overlaps rather than docks. */
    padding-bottom: 108px;
  }

  .panel {
    grid-column: 3;
    grid-row: 1;
    width: 320px;
    padding: var(--s-4);
    border-left: 1px solid var(--hairline);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: var(--s-3);
  }
  .panel-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--s-2);
  }
  /* A series title can hold CJK, so it never takes the mono display face. */
  .panel-head b {
    font: 600 var(--text-base) / var(--leading-tight) var(--font-body);
  }

  /* Below 900px the side panel collapses first, then the sidebar goes to icons. */
  @media (max-width: 900px) {
    .panel {
      display: none;
    }
  }
  @media (max-width: 680px) {
    .sidebar {
      width: 56px;
    }
    .sidebar .label {
      display: none;
    }
  }
  .live {
    position: fixed;
    inset: 0;
    z-index: 0;
    pointer-events: none;
    transition: opacity var(--dur-base) var(--ease);
  }
  .bar {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--s-4);
    flex-wrap: wrap;
  }
  .bar h1 {
    font: 600 var(--text-xl) / var(--leading-tight) var(--font-display);
    margin: 0 0 var(--s-4);
  }
  .bar .chips {
    margin-bottom: var(--s-4);
  }

  /* Signature 2. Inset from the window edges, and the only --shadow-float in the app.
     If everything is floating, nothing is. */
  .resume-bar {
    grid-column: 2;
    grid-row: 1;
    align-self: end;
    margin: var(--s-5);
    z-index: 3;
    position: relative;
    display: flex;
    align-items: center;
    gap: var(--s-4);
    height: 68px;
    padding: 0 var(--s-4);
    border: 1px solid var(--hairline);
    border-radius: var(--r-3);
    background: color-mix(in srgb, var(--bg) 92%, transparent);
    backdrop-filter: blur(var(--blur-chrome));
    box-shadow: var(--shadow-float);
    color: var(--text);
    font: inherit;
    text-align: left;
    cursor: pointer;
    overflow: hidden;
  }
  .resume-bar .thumb {
    height: 48px;
    width: 32px;
    object-fit: cover;
    border-radius: var(--r-1);
    flex: none;
  }
  .resume-bar .what {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    flex: 1;
  }
  /* Series and chapter names can hold CJK, so they take --font-body. The page count
     is ours and Latin, so it takes --font-data and gets tabular figures for free. */
  .resume-bar b {
    font: 600 var(--text-base) / var(--leading-tight) var(--font-body);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .resume-bar .count {
    font: var(--text-sm) / 1 var(--font-data);
    font-variant-numeric: tabular-nums;
    color: var(--text-muted);
    flex: none;
  }
  .resume-bar .rule {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    height: 2px;
    background: var(--glass);
  }
  .resume-bar .rule span {
    display: block;
    height: 100%;
    background: var(--progress);
  }

  /* Signature 3. One thin tick per page on the trailing edge, the current page solid.
     In right-to-left mode the stack moves to the left, because it follows the
     direction of reading. */
  .koma {
    position: fixed;
    top: 50%;
    transform: translateY(-50%);
    z-index: 3;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: var(--s-2) 6px;
    border-radius: var(--r-full);
    background: var(--scrim);
    backdrop-filter: blur(var(--blur-chrome));
    max-height: 70vh;
    pointer-events: none;
  }
  .koma.right {
    right: var(--s-3);
  }
  .koma.left {
    left: var(--s-3);
  }
  .koma i {
    display: block;
    width: 10px;
    height: 2px;
    border-radius: 1px;
    background: var(--glass-hover);
    transition: background var(--dur-fast) var(--ease);
  }
  .koma i.on {
    background: var(--text);
  }
  /* Not colour alone: a done page is also wider, so the state survives a reader who
     cannot separate the two tints. */
  .koma i.done {
    background: var(--progress);
    width: 14px;
  }
  :global(body) {
    margin: 0;
    background: var(--bg);
    color: var(--text);
    /* Body copy and anything that can hold CJK. Labels we author ourselves opt into
       --font-display; user content never does. DESIGN.md, Type. */
    font: var(--text-sm) / var(--leading-body) var(--font-body);
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
    background: color-mix(in srgb, var(--raised) 93%, transparent);
    backdrop-filter: blur(var(--blur-chrome));
    border: 1px solid var(--hairline);
    border-radius: var(--r-2);
    min-width: 210px;
  }
  .hud hr {
    border: 0;
    border-top: 1px solid var(--hairline);
    margin: 8px 0;
  }
  .hud b {
    color: var(--progress);
  }
  /* Only buttons that carry no class of their own. A bare `button:hover` rule
     outspecifies `.card` and was tinting cover cards on hover, which is exactly the
     frame DESIGN.md says a card must not have. */
  button:where(:not([class])) {
    font: inherit;
    background: var(--glass);
    color: var(--text);
    border: 1px solid var(--hairline);
    border-radius: var(--r-1);
    padding: var(--s-1) var(--s-2);
    cursor: pointer;
    transition: background var(--dur-fast) var(--ease);
  }
  button:where(:not([class])):hover:not(:disabled) {
    background: var(--glass-hover);
  }
  button:disabled {
    color: var(--text-muted);
    cursor: default;
  }
  .broken {
    position: absolute;
    left: 50%;
    bottom: 12px;
    transform: translateX(-50%);
    margin: 0;
    padding: 6px 10px;
    background: var(--danger);
    color: var(--ink-on-danger);
    border-radius: var(--r-1);
    white-space: nowrap;
  }
  input {
    font: inherit;
    flex: 1 1 280px;
    min-width: 0;
    padding: var(--s-2) var(--s-3);
    background: var(--glass);
    color: var(--text);
    border: 1px solid var(--hairline);
    border-radius: var(--r-1);
  }
  input::placeholder {
    color: var(--text-muted);
  }
  /* Rows of controls in settings and the catalog form. */
  .row {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    margin-bottom: var(--s-2);
    flex-wrap: wrap;
  }
  .row .grow {
    flex: 1;
  }
  .empty {
    padding: var(--s-7) 0;
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
    color: var(--ink-on-accent);
  }
  .chip.accent:hover {
    background: var(--accent-soft);
    color: var(--ink-on-accent);
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
  .search {
    font: inherit;
    display: block;
    width: 100%;
    max-width: 460px;
    padding: var(--s-2) var(--s-3);
    background: var(--glass);
    color: var(--text);
    border: 1px solid var(--hairline);
    border-radius: var(--r-1);
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
    color: var(--ink-on-accent);
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
    font: inherit;
    text-align: left;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 0;
    border: 0;
    background: none;
    color: var(--text);
    cursor: pointer;
  }
  /* The title, which can hold CJK and so never takes the mono display face.
     Clamped to two lines: an unclamped long title pushes its own meta line down and
     the grid row stops aligning with its neighbours. */
  .card b {
    font: 600 var(--text-sm) / var(--leading-tight) var(--font-body);
    margin-top: var(--s-2);
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  /* Rows align because every card is the same height, whatever its title does. */
  .shelf .card {
    align-content: start;
  }
  .chapter {
    font: inherit;
    text-align: left;
    display: flex;
    /* Title left, page count right: a chapter row is a line of a list, and the counts
       line up down the edge because they are tabular. */
    flex-direction: row;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--s-3);
    padding: var(--s-2) var(--s-3);
    background: var(--glass);
    color: var(--text);
    border: 1px solid var(--hairline);
    border-radius: var(--r-1);
    cursor: pointer;
    transition: background var(--dur-fast) var(--ease);
  }
  .chapter .name {
    font: 500 var(--text-sm) / var(--leading-tight) var(--font-body);
  }
  .chapter.read .name {
    color: var(--text-muted);
  }
  .chapter:hover {
    background: var(--glass-hover);
  }
  /* A card has no surface to tint, so hover and selection live on the cover itself:
     the art lifts slightly and the selected one is ringed. */
  .card:hover .cover {
    outline: 1px solid var(--hairline);
  }
  .card.on .cover {
    outline: 2px solid var(--accent);
    outline-offset: 0;
  }
  .chapters {
    display: flex;
    flex-direction: column;
    gap: var(--s-1);
  }
  .meta {
    color: var(--text-muted);
  }
  /* The instrumentation only belongs on screen while something is being read. */
  .hud:not(.reading) {
    display: none;
  }
  /* Above everything, including the text reader. An error nobody can see is worse than
     no error handling at all: the app looks like it did nothing. */
  .error {
    z-index: 20;
    position: fixed;
    left: 12px;
    bottom: 12px;
    max-width: 60ch;
    margin: 0;
    padding: var(--s-3);
    background: var(--danger);
    color: var(--ink-on-danger);
    border-radius: var(--r-1);
  }
</style>
