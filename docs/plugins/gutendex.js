// A working PanReader source, and the reference for writing one.
//
// Reads Project Gutenberg through Gutendex, its public JSON API. Everything it serves
// is public domain, there is no login and no scraping, so this is a source you can
// point the app at and actually read from today.
//
// The whole interface is below. A source is one object with six methods; there is no
// build step, no imports, and no framework. Drop this file in through
// Sources -> Install from file, or serve the folder it lives in and add index.json as
// a repository.

export default {
  // Stable forever. Library entries are filed under this, so renaming it orphans them.
  id: "gutendex",
  name: "Project Gutenberg",
  version: "1.0.0",
  lang: "en",
  // "novel" feeds the text reader, "manga" feeds the image reader. The only place the
  // two diverge is `content`, at the bottom of this file.
  kind: "novel",
  nsfw: false,

  // Every host this source may reach. The host performs all requests and refuses any
  // URL not covered here, so this list is the entire outward reach of this file.
  // Subdomains are included, which is why `www.gutenberg.org` works.
  hosts: ["gutendex.com", "gutenberg.org"],

  async popular(page) {
    return list(`https://gutendex.com/books?page=${page}`);
  },

  async latest(page) {
    // Gutendex has no "recent" sort, so the honest thing is to give the same order
    // rather than invent one and have it silently mean nothing.
    return list(`https://gutendex.com/books?page=${page}`);
  },

  async search(page, query) {
    const q = encodeURIComponent(query);
    return list(`https://gutendex.com/books?search=${q}&page=${page}`);
  },

  async details(entryId) {
    const book = JSON.parse(await pan.fetch(`https://gutendex.com/books/${entryId}`));
    return entry(book);
  },

  // One chapter, because a Gutenberg book is one file. A source for a site that
  // paginates would return one of these per chapter, newest last.
  async chapters(entryId) {
    return [{ id: entryId, title: "Read", number: 1 }];
  },

  // The one method where the two readers differ.
  //   novel -> { html }   the host normalizes it through pr-text
  //   manga -> { pages: [url] }   the host fetches and decodes each through pr-image
  async content(chapterId) {
    const book = JSON.parse(await pan.fetch(`https://gutendex.com/books/${chapterId}`));
    const html =
      book.formats["text/html"] ??
      book.formats["text/html; charset=utf-8"] ??
      book.formats["text/html; charset=us-ascii"];
    if (!html) throw new Error("this book has no HTML edition");
    return { html: await pan.fetch(html) };
  },
};

// ---------------------------------------------------------------- helpers

async function list(url) {
  const page = JSON.parse(await pan.fetch(url));
  return {
    entries: (page.results ?? []).map(entry),
    // Paging is whatever the API says it is; guessing from a count is how a browse
    // ends up with an empty last page.
    hasNext: Boolean(page.next),
  };
}

function entry(book) {
  return {
    id: String(book.id),
    title: book.title ?? "Untitled",
    author: (book.authors ?? []).map((a) => a.name).join(", "),
    cover: book.formats?.["image/jpeg"],
    description: (book.subjects ?? []).slice(0, 6).join(" · "),
  };
}
