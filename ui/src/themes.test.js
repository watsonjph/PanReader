import { describe, expect, it } from "vitest";
import themes from "../../data/themes.json";

/// DESIGN.md's quality floor: body text meets 4.5:1. A theme is thirteen lines of
/// JSON, which is exactly why it is easy to add one that reads badly -- so the floor
/// is checked here rather than left to whoever notices it on screen.

const lin = (c) => {
  c /= 255;
  return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
};

/** Rec. 709 relative luminance of a #rrggbb string. */
const luminance = (hex) => {
  const h = hex.replace("#", "");
  const [r, g, b] = [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16));
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
};

const contrast = (a, b) => {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
};

const entries = Object.entries(themes).filter(([id]) => id !== "//");

describe("every theme in data/themes.json", () => {
  it.each(entries)("%s reads at the quality floor", (_id, t) => {
    expect(contrast(t.text, t.bg), "body text on the app base").toBeGreaterThanOrEqual(4.5);
    expect(contrast(t.text, t.raised), "body text on a raised surface").toBeGreaterThanOrEqual(4.5);
    // --text-muted is for non-essential text, so it takes the large-text floor, but it
    // still has to be legible rather than decorative.
    expect(contrast(t["text-muted"], t.bg), "muted text on the app base").toBeGreaterThanOrEqual(3);

    // The primary action is a filled accent pill. Amber wants dark ink; a theme that
    // pairs it with light ink fails here rather than on someone's screen.
    expect(
      contrast(t["ink-on-accent"], t.accent),
      "ink on the primary action",
    ).toBeGreaterThanOrEqual(4.5);
    expect(
      contrast(t["ink-on-accent"], t["accent-soft"]),
      "ink on the primary action, hovered",
    ).toBeGreaterThanOrEqual(4.5);
    expect(contrast(t["ink-on-danger"], t.danger), "ink on danger").toBeGreaterThanOrEqual(4.5);
  });

  it.each(entries)("%s defines every token the CSS expects", (_id, t) => {
    // The thirteen colours DESIGN.md calls the contract, plus the elevation group that
    // turned out to be theme-dependent too. A missing one resolves to nothing at all
    // in CSS, which fails silently.
    for (const key of [
      "bg", "raised", "text", "text-muted", "surface", "progress",
      "accent-soft", "accent", "accent-alt", "accent-deep",
      "highlight", "highlight-dark", "danger",
      "glass", "glass-hover", "hairline", "scrim", "blur-chrome",
      "shadow-float", "ink-on-accent", "ink-on-danger",
    ]) {
      expect(t, `${key} is missing`).toHaveProperty(key);
    }
    expect(typeof t.dark).toBe("boolean");
    expect(t.name).toBeTruthy();
  });
});
