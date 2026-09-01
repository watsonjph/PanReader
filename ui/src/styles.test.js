/// Structural checks on App.svelte's stylesheet.
///
/// Not a substitute for looking at the screen. These cover the one failure mode that
/// looking at the screen has repeatedly missed: a class that is fine everywhere it was
/// written and wrong somewhere else in the same file. Two of those have shipped --
/// `.row` tinting the settings rows, and `.reading` turning the debug HUD into a
/// full-screen blurred panel over the page -- and both were invisible until someone
/// happened to open the right screen.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const source = readFileSync(new URL("./App.svelte", import.meta.url), "utf8");
const cut = source.indexOf("<style>");
const template = source.slice(0, cut);
const styles = source.slice(cut);

/** `class:foo={...}` — a boolean state flag toggled on an existing element. */
const flags = [...template.matchAll(/class:([A-Za-z0-9_-]+)/g)].map((m) => m[1]);

/** `.foo { ... }` at the top level of the style block — a rule that styles foo alone. */
const bareRules = [...styles.matchAll(/^ {2}\.([A-Za-z0-9_-]+)\s*\{/gm)].map((m) => m[1]);

describe("App.svelte styles", () => {
  it("has no state flag that also owns a bare rule", () => {
    // A state flag lands on an element that already has its own class, so a bare rule
    // of the same name applies to every element carrying the flag -- including the ones
    // it was never written for. Scope the rule (`.thing.flag`) or rename it.
    const collisions = flags.filter((f) => bareRules.includes(f));
    expect(collisions).toEqual([]);
  });

  it("declares each bare rule once", () => {
    // Two `.foo { }` blocks in one component do not merge in any useful way: the later
    // one silently wins on every property they share, from wherever it happens to sit.
    const seen = new Map();
    for (const name of bareRules) seen.set(name, (seen.get(name) ?? 0) + 1);
    expect([...seen].filter(([, n]) => n > 1).map(([name]) => name)).toEqual([]);
  });

  it("styles nothing that the markup never uses", () => {
    // Svelte's own unused-selector warning covers most of this, and it does not fail a
    // build. Dead style is how a component grows rules nobody dares delete.
    const used = new Set([
      ...[...template.matchAll(/class="([^"{]*)"/g)].flatMap((m) => m[1].split(/\s+/)),
      ...flags,
      // Interpolated: `class="chip {extra}"` and friends.
      ...[...template.matchAll(/class="[^"]*\{[^}]*\}[^"]*"/g)].flatMap((m) =>
        m[0].match(/[A-Za-z0-9_-]+/g),
      ),
    ]);
    expect(bareRules.filter((name) => !used.has(name))).toEqual([]);
  });
});
