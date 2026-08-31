// Generates ui/src/themes.css from data/themes.json.
//
// DESIGN.md: themes are data, and a theme block is never hand-written in a stylesheet.
// Build time rather than runtime so the default theme is painted with the first frame;
// injecting it from JS would flash the wrong colours on every cold start. The *live*
// palette is a separate mechanism and is injected at runtime, because it is genuinely
// dynamic -- it changes with whatever cover is on screen.
//
//   node gen-themes.mjs        (run by predev and prebuild)

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const source = join(here, "..", "data", "themes.json");
const target = join(here, "src", "themes.css");

const themes = JSON.parse(readFileSync(source, "utf8"));

/** The thirteen colour tokens DESIGN.md calls the contract, plus the elevation group. */
const block = (theme) =>
  Object.entries(theme)
    .filter(([key]) => !["name", "dark"].includes(key))
    .map(([key, value]) => `  --${key}: ${value};`)
    .join("\n");

const entries = Object.entries(themes).filter(([id]) => id !== "//");
const [defaultId] = entries[0];

const css = [
  "/* Generated from data/themes.json by ui/gen-themes.mjs. Do not edit.",
  " * Add a theme by adding an object to that file. */",
  "",
  ...entries.map(([id, theme]) =>
    // The first theme is also the bare :root, so a document with no theme class still
    // resolves every variable rather than rendering unstyled.
    `${id === defaultId ? ":root,\n" : ""}.theme-${id} {\n  color-scheme: ${
      theme.dark ? "dark" : "light"
    };\n${block(theme)}\n}`,
  ),
  "",
].join("\n");

writeFileSync(target, css, "utf8");

const names = entries.map(([id, t]) => `${id} (${t.name})`).join(", ");
console.log(`themes.css: ${entries.length} themes -- ${names}`);
