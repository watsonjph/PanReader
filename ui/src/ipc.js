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
    },
  ],
  chapters: () =>
    Array.from({ length: 12 }, (_, i) => ({
      id: 200 + i,
      title: `Chapter ${i + 1}`,
      number: i + 1,
      page_count: 18 + i,
      path: `D:/manga/ch${i + 1}.cbz`,
      page: i === 0 ? 8 : 0,
      page_frac: 0,
      completed: i < 3,
    })),
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
