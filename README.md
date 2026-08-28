# PanReader

A fast manga and manhwa reader for desktop. Rust core, Tauri v2 shell.

**Status: Phase 0, feasibility spike.** No library, no settings, no design. The only
question this build answers is whether a 60,000px webtoon strip scrolls smoothly
through a webview, and whether a 200-page CBZ opens fast enough.

## Layout

```
crates/pr-image     decode, DCT-scaled decode, tile, encode
crates/pr-archive   folder / CBZ -> ordered page bytes, natural sort
crates/pr-app       Tauri shell, tile cache, pan:// protocol
ui/                 Svelte 5 + plain CSS, virtualized scroller + frame HUD
```

## Run

```bash
pnpm --dir ui install
cargo run -p pr-app --example make_fixtures --release   # synthetic test content
cargo tauri dev
```

Point it at real content instead of the fixtures:

```bash
PANREADER_CBZ=/path/to/chapter.cbz PANREADER_STRIP=/path/to/webtoon_folder cargo tauri dev
```

`1` loads the CBZ, `2` loads the strip, `s` starts a 4000px/s auto-flick. The HUD
reports frame p50/p99, dropped frames, first-paint latency, and Rust-side decode,
encode and cache numbers.

## Phase 0 targets

| Measure | Target |
|---|---|
| 64,000px strip, fast flick | 120fps sustained, no visible pop-in |
| Memory, strip fully scrolled | < 400MB |
| 200-page CBZ, first painted page | < 400ms |

## Check before committing

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
