# Sources

A source is one JavaScript file. No build step, no imports, no framework.
[`gutendex.js`](gutendex.js) is a working one — read it first; it is the whole interface
in sixty lines.

## What the repository field accepts

A direct URL to a **JSON index**, not a web page and not a repository's front page.
Two shapes are understood, sniffed from the file rather than the filename:

```jsonc
// Ours
{ "schema": 1,
  "sources": [
    { "id": "example", "name": "Example", "version": "1.0.0", "lang": "en",
      "kind": "novel", "bundle": "example.js", "sha256": "..." }
  ] }
```

```jsonc
// LNReader's plugins.min.json — a bare array
[ { "id": "example", "name": "Example", "site": "https://example.com",
    "lang": "English", "version": "1.0.0", "url": "plugins/english/example.js" } ]
```

`bundle` and `url` resolve against the index's own URL, so a relative path works.
`sha256` is optional and checked when present.

### What it does not accept

- **A Mihon, Tachiyomi or Aniyomi repository.** Those extensions are compiled Android
  APKs and cannot run here at all — it is not a format we could add support for. To read
  Mihon sources, run [Suwayomi](https://github.com/Suwayomi/Suwayomi-Server) and add its
  OPDS endpoint (`/api/opds/v1.2`) under **Catalogs** instead. PanReader will say this if
  you paste one.
- A GitHub page, a releases page, or a repository root. It must be the raw JSON.

## Trying one without hosting anything

**Sources → Install from file…** and pick `gutendex.js`. That is the fastest loop while
writing one: edit, reinstall, browse.

To test the repository path instead, serve this folder and add the index:

```bash
python -m http.server 8000 --directory docs/plugins
```

Then add `http://localhost:8000/index.json` as a repository.

## The interface

```js
export default {
  id, name, version, lang,
  kind: "manga" | "novel",   // which reader it feeds
  nsfw: false,
  hosts: ["example.com"],    // everything it may reach, and nothing else is reachable

  async popular(page)                // -> { entries: [Entry], hasNext: bool }
  async latest(page)                 // -> { entries, hasNext }
  async search(page, query)          // -> { entries, hasNext }
  async details(entryId)             // -> Entry
  async chapters(entryId)            // -> [{ id, title, number }]
  async content(chapterId)           // -> { html } or { pages: [url] }
};
```

`Entry` is `{ id, title, author?, cover?, description? }`. `id` is yours to choose and
must be stable: library entries are filed under it.

## What a source can and cannot do

It gets **one** function beyond the language itself:

```js
const body = await pan.fetch(url, { headers });
```

The host performs that request, refuses any URL outside `hosts`, applies the rate limit,
and attaches any cookies a site issued after a challenge. A source cannot open a socket,
read a file, see a cookie, or reach the network any other way — there is no `fetch`, no
`XMLHttpRequest`, no `require` and no `process` in the isolate. It also gets a CPU
deadline and a memory ceiling, and is killed if it exceeds either.

So a source is a parser. That is the whole design.

## A note on LNReader plugins

Their **index** is read (the shim above), but their **bundles do not run yet**. LNReader
plugins import `@libs/fetch`, `cheerio` and friends, and this host resolves no module
specifiers — so loading one fails on its first `import`. Making them work needs a module
resolver providing those names, which is real work and is not done. Write a source, or
use Suwayomi for Mihon's, until it is.
