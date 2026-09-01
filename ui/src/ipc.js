/// The one place the frontend talks to Rust.
///
/// In the app this is Tauri's `invoke`, unchanged. In a plain browser -- `pnpm dev`
/// with no Tauri around it -- there is no backend, every call rejects, and the UI
/// renders as an empty shell you cannot look at. So dev builds outside Tauri fall back
/// to a small fixture instead.
///
/// This is a development affordance, not a mock layer to test against: it is compiled
/// out of production builds, and nothing in it is allowed to encode behaviour. If a
/// question can only be answered by the fixture, the answer is worthless.
import { invoke as tauriInvoke } from "@tauri-apps/api/core";

const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** Deterministic stand-in covers, so layout can be judged without a library. */
const cover = (seed) => {
  const hues = [18, 210, 340, 96, 265, 40, 160, 300];
  const h = hues[seed % hues.length];
  const svg =
    `<svg xmlns="http://www.w3.org/2000/svg" width="200" height="300">` +
    `<rect width="200" height="300" fill="hsl(${h} 45% 22%)"/>` +
    `<circle cx="100" cy="120" r="52" fill="hsl(${h} 60% 40%)"/>` +
    `<rect y="240" width="200" height="60" fill="hsl(${h} 50% 14%)"/></svg>`;
  return `data:image/svg+xml;utf8,${encodeURIComponent(svg)}`;
};

/** A stand-in page, shaped like a printed manga page so layout maths means something. */
const page = (index) => {
  const h = 255 - ((index * 7) % 40);
  const svg =
    `<svg xmlns="http://www.w3.org/2000/svg" width="978" height="1400">` +
    `<rect width="978" height="1400" fill="hsl(30 8% ${Math.round(h / 3)}%)"/>` +
    `<rect x="60" y="60" width="858" height="1280" fill="none" ` +
    `stroke="hsl(30 10% 45%)" stroke-width="4"/>` +
    `<text x="489" y="720" font-family="monospace" font-size="200" fill="hsl(30 10% 55%)" ` +
    `text-anchor="middle">${index + 1}</text></svg>`;
  return `data:image/svg+xml;utf8,${encodeURIComponent(svg)}`;
};

const SERIES = [
  "Yotsuba&!",
  "よつばと！ 第2巻",
  "Berserk",
  "Solo Leveling",
  "Vinland Saga",
  "Blame!",
  "A Very Long Series Title That Has To Be Truncated Somewhere",
  "Oyasumi Punpun",
  "Vagabond",
  "20th Century Boys",
].map((title, i) => ({
  id: i + 1,
  title,
  // One novel on the shelf, so the shell has both readers to route to.
  kind: i === 7 ? "text" : "image",
  path: `D:/manga/${title}`,
  chapter_count: 3 + (i % 9),
  unread: i % 4,
  cover_chapter_id: 100 + i,
  // Descending, so "Recently added" has something to order.
  added_at: 1_780_000_000 - i * 86_400,
}));

const FIXTURES = {
  tile_base: () => "pan://localhost",
  settings: () => ({
    default_reading_mode: "rtl",
    fit: "page",
    downsample: 1,
    page_padding: 0,
    rotation: 0,
    rotation_lock: false,
    double_page: false,
    cover_alone: true,
    theme: "ink",
    live_background: true,
    reduce_animations: false,
    list_view: false,
    auto_backup: true,
    backup_keep: 8,
    text_font: "serif",
    text_size: 19,
    text_measure: 66,
    text_leading: 160,
    text_paged: false,
    text_paper: false,
    text_vertical: false,
  }),
  save_settings: () => null,
  roots: () => ["D:/manga"],
  scanning: () => false,
  catalogs: () => [{ id: 1, url: "https://demo.komga.org/opds/v1.2", name: "Komga demo" }],
  categories: () => [
    { id: 1, name: "Reading", reading_mode: null, series_count: 4 },
    { id: 2, name: "Webtoons", reading_mode: "webtoon", series_count: 2 },
  ],
  categories_of: () => [1],
  search: ({ query, category }) =>
    SERIES.filter(
      (s) =>
        (!query || s.title.toLowerCase().includes(query.toLowerCase())) &&
        (category === null || category === undefined || category === 1),
    ),
  continue_reading: () => [
    {
      chapter_id: 100,
      series_id: 1,
      series_title: "Yotsuba&!",
      chapter_title: "Chapter 12",
      number: 12,
      page: 8,
      page_frac: 0.25,
      page_count: 24,
      kind: "image",
    },
  ],
  chapters: ({ seriesId }) =>
    Array.from({ length: 12 }, (_, i) => ({
      id: 200 + i,
      title: `Chapter ${i + 1}`,
      number: i + 1,
      page_count: 18 + i,
      path: `D:/manga/ch${i + 1}.cbz`,
      page: i === 0 ? 8 : 0,
      page_frac: 0,
      completed: i < 3,
      locator: "",
      // The novel on the shelf, so both readers are reachable from the fixture.
      kind: SERIES.find((x) => x.id === seriesId)?.kind ?? "image",
    })),
  backups: () => {
    const now = Math.floor(Date.now() / 1000);
    return [0, 1, 2, 5].map((back) => ({
      path: `C:/Users/you/AppData/Roaming/panreader/backups/panreader-${now - back * 86400}.pnbk`,
      taken_at: now - back * 86400,
      bytes: 184_320 + back * 2_100,
    }));
  },
  export_backup: ({ path }) => path,
  preview_backup: () => ({
    series_added: 3,
    series_matched: 7,
    chapters_added: 41,
    chapters_matched: 260,
    positions_advanced: 12,
    positions_kept: 4,
    bookmarks_added: 6,
    sessions_added: 88,
    categories_added: 1,
    catalogs_added: 0,
    roots_added: 0,
  }),
  import_backup: () => FIXTURES.preview_backup(),
  history: () => {
    const day = 86_400_000;
    const now = Date.now();
    return [0, 0.2, 1, 1.4, 3, 9].map((back, i) => ({
      id: i + 1,
      chapter_id: 100 + i,
      series_id: (i % 4) + 1,
      series_title: SERIES[i % 4].title,
      chapter_title: `Chapter ${20 - i}`,
      number: 20 - i,
      cover_chapter_id: 100 + i,
      started_at: now - back * day - 25 * 60_000,
      ended_at: now - back * day,
      pages: 6 + i * 3,
      last_page: 6 + i * 3,
      kind: "image",
    }));
  },
  reading_stats: () => ({
    chapters: 42,
    pages: 913,
    minutes: 1_284,
    days: 17,
    streak: 4,
    best_streak: 11,
  }),
  forget: () => null,
  bookmarks: () => [
    {
      id: 1,
      chapter_id: 100,
      series_id: 1,
      series_title: "Yotsuba&!",
      chapter_title: "Chapter 12",
      page: 8,
      page_frac: 0.25,
      paragraph: null,
      char_offset: null,
      note: "the cicada page",
      created_at: 1_780_000_000,
      kind: "image",
    },
    {
      id: 2,
      chapter_id: 103,
      series_id: 4,
      series_title: "Solo Leveling",
      chapter_title: "Chapter 3",
      page: 14,
      page_frac: 0,
      paragraph: null,
      char_offset: null,
      note: "",
      created_at: 1_780_090_000,
      kind: "image",
    },
  ],
  toggle_bookmark: () => true,
  remove_bookmark: () => null,
  set_bookmark_note: () => null,
  /// The image reader, with stand-in pages.
  ///
  /// Not here to test decoding -- there is none -- but so the reader renders at all
  /// outside Tauri. A reader you cannot open in `pnpm dev` is a reader whose layout
  /// bugs are found by the person using the app, which is how the debug HUD once
  /// spread itself over the whole page.
  open_chapter: ({ displayW }) => {
    const w = displayW || 1200;
    const h = Math.round(w * (1400 / 978));
    return {
      reading: { mode: "rtl", source: "default" },
      display_w: w,
      total_h: h * 24,
      pages: Array.from({ length: 24 }, (_, index) => ({
        index,
        w,
        h,
        y: index * h,
        tiles: 1,
        tile_h: h,
        readable: true,
      })),
    };
  },
  warm: () => null,
  stats: () => ({}),
  open_text: () => {
    const para = (text) => ({ kind: "para", spans: [{ text }] });
    const blocks = [
      { kind: { heading: 2 }, spans: [{ text: "Chapter One" }] },
      para(
        "The night was clear and the road ran straight for a long way, and she " +
          "walked it without hurrying, because there was nothing at the end of it " +
          "that would not wait.",
      ),
      {
        kind: "para",
        spans: [
          { text: "She had been told, " },
          { text: "once", em: true },
          { text: ", that the town kept no records older than the fire." },
        ],
      },
      { kind: "divider", spans: [] },
      { kind: "quote", spans: [{ text: "Nothing is ever only itself." }] },
    ];
    // Enough of it to make scrolling and column paging mean something.
    for (let i = 0; i < 60; i++) {
      blocks.push(
        para(
          `Paragraph ${i + 1}. ` +
            "The lamps came on one at a time along the length of the street, and " +
            "each one made the dark between them a little more particular.",
        ),
      );
    }
    return {
      title: "Chapter One",
      document: { blocks },
      blocks: blocks.length,
      characters: blocks.reduce(
        (n, b) => n + b.spans.reduce((m, s) => m + s.text.length, 0),
        0,
      ),
      page: 0,
      paragraph: null,
      char_offset: null,
    };
  },
  palette: () => [
    "26,22,30",
    "90,44,38",
    "54,70,90",
    "88,80,40",
    "40,66,58",
    "70,40,66",
    "36,52,72",
    "82,60,34",
  ],
};

/** Covers are served over pan:// in the app; in the browser they are data URIs. */
export const isMock = !inTauri && import.meta.env.DEV;

export function mockCover(chapterId) {
  return cover(Number(chapterId) || 0);
}

export function mockPage(index) {
  return page(Number(index) || 0);
}

export function invoke(command, args = {}) {
  if (inTauri || !import.meta.env.DEV) return tauriInvoke(command, args);

  const fixture = FIXTURES[command];
  if (!fixture) {
    return Promise.reject(
      new Error(`no dev fixture for "${command}" -- add one in ui/src/ipc.js`),
    );
  }
  // A tick of latency, so loading order bugs show up here rather than only in the app.
  return new Promise((resolve) => setTimeout(() => resolve(fixture(args)), 20));
}
