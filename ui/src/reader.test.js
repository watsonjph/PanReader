import { describe, expect, it } from "vitest";
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

/// Real shapes: a Yotsuba page, its printed spread, and a webtoon segment.
const PAGE = { y: 0, w: 978, h: 1400, tiles: 2, tile_h: 1024, index: 0 };
const SPREAD = { w: 2100, h: 1400 };
const WINDOW = { vw: 1405, vh: 939 }; // the window the orientation bug appeared in

describe("tileRects", () => {
  it("partitions a page exactly, with a short last tile", () => {
    const rects = tileRects(PAGE);
    expect(rects).toHaveLength(2);
    expect(rects[0]).toEqual({ t: 0, top: 0, bottom: 1024 });
    expect(rects[1]).toEqual({ t: 1, top: 1024, bottom: 1400 });

    let end = 0;
    for (const r of rects) {
      expect(r.top).toBe(end); // no gap, which would be a visible seam
      end = r.bottom;
    }
    expect(end).toBe(PAGE.h); // and no overrun past the page
  });

  it("gives a passthrough page a single tile as tall as itself", () => {
    const rects = tileRects({ ...PAGE, tiles: 1, tile_h: 1400 });
    expect(rects).toEqual([{ t: 0, top: 0, bottom: 1400 }]);
  });
});

describe("fitScale", () => {
  it("fits the whole page by its tighter axis", () => {
    // 939/1400 binds before 1405/978.
    expect(fitScale("page", 978, 1400, 1405, 939)).toBeCloseTo(939 / 1400, 6);
  });

  it("honours the single-axis modes and leaves original alone", () => {
    expect(fitScale("width", 978, 1400, 1405, 939)).toBeCloseTo(1405 / 978, 6);
    expect(fitScale("height", 978, 1400, 1405, 939)).toBeCloseTo(939 / 1400, 6);
    expect(fitScale("original", 978, 1400, 1405, 939)).toBe(1);
  });
});

describe("turnFor", () => {
  const base = { rot: 0, rotLock: false, fit: "page", ...WINDOW };

  it("never turns a portrait page, however much better it would fit", () => {
    // The shipped bug. Turning gains 43% here, which is why a bare fit comparison
    // rotated the whole volume onto its side.
    const gain =
      fitScale("page", 1400, 978, WINDOW.vw, WINDOW.vh) /
      fitScale("page", 978, 1400, WINDOW.vw, WINDOW.vh);
    expect(gain).toBeGreaterThan(1.2);
    expect(turnFor({ ...base, w: 978, h: 1400 })).toBe(0);
  });

  it("turns a wide spread when the window is tall", () => {
    expect(turnFor({ ...base, ...SPREAD, vw: 800, vh: 1400 })).toBe(90);
  });

  it("leaves a wide spread alone when it already fits", () => {
    expect(turnFor({ ...base, ...SPREAD })).toBe(0);
    expect(turnFor({ ...base, ...SPREAD, vw: 1920, vh: 1080 })).toBe(0);
  });

  it("locking suppresses the automatic turn but keeps the manual angle", () => {
    const tall = { ...base, ...SPREAD, vw: 800, vh: 1400 };
    expect(turnFor({ ...tall, rotLock: true })).toBe(0);
    expect(turnFor({ ...tall, rot: 180, rotLock: true })).toBe(180);
    // Unlocked, the automatic quarter turn stacks onto the manual angle.
    expect(turnFor({ ...tall, rot: 180 })).toBe(270);
  });
});

describe("pageTops", () => {
  const pages = [
    { y: 0, h: 1400 },
    { y: 1400, h: 1400 },
    { y: 2800, h: 1400 },
  ];

  it("is seamless at zero padding", () => {
    expect(pageTops(pages, 0, 1)).toEqual([0, 1400, 2800]);
    // Every page starts exactly where the previous one ended.
    expect(pageTops(pages, 0, 2)).toEqual([0, 700, 1400]);
  });

  it("adds padding as whole pixels without disturbing the rounding", () => {
    expect(pageTops(pages, 16, 1)).toEqual([0, 1416, 2832]);
    expect(stripHeight(4200, 3, 16, 1)).toBe(4200 + 32);
    expect(stripHeight(4200, 3, 0, 1)).toBe(4200);
  });
});

describe("pageAt", () => {
  const tops = [0, 1416, 2832];

  it("finds the page covering an offset, including its exact top edge", () => {
    expect(pageAt(tops, 0)).toBe(0);
    expect(pageAt(tops, 1415)).toBe(0);
    expect(pageAt(tops, 1416)).toBe(1);
    expect(pageAt(tops, 2831)).toBe(1);
    expect(pageAt(tops, 99999)).toBe(2);
  });

  it("clamps rather than returning a negative index above the first page", () => {
    expect(pageAt(tops, -500)).toBe(0);
    expect(pageAt([], 10)).toBe(0);
  });
});

describe("navigation direction", () => {
  it("advances leftward in right-to-left and rightward in left-to-right", () => {
    expect(pageStep("ArrowLeft", true)).toBe(1);
    expect(pageStep("ArrowRight", true)).toBe(-1);
    expect(pageStep("ArrowLeft", false)).toBe(-1);
    expect(pageStep("ArrowRight", false)).toBe(1);
  });

  it("moves forward on the vertical keys regardless of direction", () => {
    for (const rtl of [true, false]) {
      expect(pageStep(" ", rtl)).toBe(1);
      expect(pageStep("PageDown", rtl)).toBe(1);
      expect(pageStep("PageUp", rtl)).toBe(-1);
    }
  });

  it("ignores keys the reader does not own", () => {
    expect(pageStep("a", true)).toBe(0);
    expect(pageStep("Escape", false)).toBe(0);
  });

  it("maps click zones to the same directions, with a dead middle", () => {
    expect(clickStep(10, 1200, true)).toBe(1);
    expect(clickStep(1190, 1200, true)).toBe(-1);
    expect(clickStep(10, 1200, false)).toBe(-1);
    expect(clickStep(1190, 1200, false)).toBe(1);
    expect(clickStep(600, 1200, true)).toBe(0);
  });
});
