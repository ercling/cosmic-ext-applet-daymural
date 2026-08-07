# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build / test / run

This machine's linuxbrew `pkg-config` shadows the system one and misses
`/usr/lib64/pkgconfig` (breaks the `xkbcommon` probe in `smithay-client-toolkit`).
**Every cargo invocation needs**
`PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig` — the `justfile`
exports it, so prefer `just` recipes; otherwise export it yourself:

```bash
just check        # cargo fmt --check + clippy -D warnings + cargo test
just build        # cargo build --release
just install      # install binary + .desktop + icon into ~/.local (no sudo)
just uninstall

# raw cargo (export PKG_CONFIG_PATH first):
export PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

`rustfmt`/`clippy` binaries live in `~/.local/bin` (extracted from Fedora RPMs to
match system rustc 1.97.1 — the distro rustc package ships without them).

Running the binary standalone (`cargo run` / `target/release/cosmic-bing-wallpaper`)
opens a floating applet window; **beware**: a cold start (empty
`~/Pictures/BingWallpaper`) triggers a real Bing fetch and a wallpaper apply ~5 s
in. The normal home is the COSMIC panel (add via Settings → Desktop → Panel after
`just install`).

Tests never touch real user config/state — they inject paths or
`Config::with_custom_path` rooted in a `tempfile::TempDir`. Keep it that way.

## Architecture

Single self-contained libcosmic applet binary, no daemon — timers only run while
the panel runs. Both git deps (libcosmic, cosmic-bg-config) are **rev-pinned** in
`Cargo.toml` with `Cargo.lock` committed; libcosmic APIs move fast, verify against
the pinned rev before coding against remembered names.

- `src/main.rs` — entry point: `cosmic::applet::run::<Window>(())`.
- `src/app.rs` — the `cosmic::Application` impl (`Window`): message loop, popup
  open/close, startup restore (catalogue + config, no network), the async refresh
  pipeline (`run_refresh`: fetch list → download missing + thumbnails → merge →
  prune → auto-apply per the "don't clobber" rule → reschedule), one-shot
  generation-counter timers for refresh and shuffle (a stale tick is ignored, so
  rescheduling atomically replaces the pending timer), `state_dir()`/`catalogue_path()`.
- `src/view.rs` — popup UI (thumbnail, title/copyright, About link, prev/next/
  newest/refresh controls, shuffle + retention rows, status footer) plus the pure
  display helpers (`displayed`/`prev_target`/`next_target`/`newest_target`,
  `format_updated`, dropdown index↔value mappings) which *are* unit-tested; iced
  view code itself is exempt from tests.
- `src/bing.rs` — Bing API types + parsing (fixture:
  `tests/fixtures/hpimagearchive.json`), title/copyright derivation
  (`split_copyright` — Bing's own `title` field is the literal `"Info"`), pure
  URL/filename builders and the inverse `parse_filename`, reqwest client +
  `fetch_image_list` + atomic `.part`-then-rename `download_image`.
- `src/thumbs.rs` — 480×270 thumbnail cache in the state dir; the UI never
  decodes the full ~5 MB UHD file.
- `src/catalogue.rs` — `ImageEntry`/`Catalogue`: JSON persistence (atomic write),
  merge-with-dedupe by `urlbase`, retention prune (never deletes the currently
  applied file), `rebuild_from_folder` (filename ↔ urlbase mapping is
  deterministic both ways, so rebuilds dedupe against the next fetch with no
  re-downloads), navigation helpers.
- `src/config.rs` — `AppletConfig` (shuffle on/off, interval, retention) via
  cosmic-config under app ID `io.github.ercling.CosmicBingWallpaper`, version 1,
  write-on-change setters, watch subscription for external edits.
- `src/wallpaper.rs` — cosmic-bg config writer: `updated_entry` mutates only
  `source`, `apply` writes the `all` entry *before* flipping `same-on-all`,
  `current_source`/`is_ours` back the don't-clobber rule, `download_dir()`.
- `src/schedule.rs` — pure timing math: `next_refresh` (reference-exact,
  including the out-of-range reset to 60 s and the +300 s fudge),
  `next_shuffle_delay`, `fetch_count(retention_days)`, `should_auto_apply`,
  `retention_reduced`.

Design decisions, live-verified Bing/cosmic-bg facts, and per-task
implementation notes live in `docs/plans/` (see
`20260807-cosmic-bing-wallpaper-applet.md`, archived under `docs/plans/completed/`
once done). Gotchas recorded there worth
knowing: the `zune-jpeg` `log`-feature workaround in `Cargo.toml`, and the
transitive `cosmic-config` pin living only in `Cargo.lock`.

## examples/bing-wallpaper-gnome-extension

An independent clone of `git@github.com:neffo/bing-wallpaper-gnome-extension` (version 53)
with its own `.git` directory. **Reference material only — do not modify it, and do not
expect changes here to be tracked by the parent repo.** It is a GJS/GNOME Shell extension
(not COSMIC), so treat it as a source of behavioral patterns rather than code to port.

The patterns worth reading before designing the equivalent COSMIC applet:

- `utils.js` — the data layer. Bing's image-of-the-day is fetched from
  `https://www.bing.com/HPImageArchive.aspx?format=js&n=8`; the downloaded image
  catalogue is persisted as JSON in a GSettings key and manipulated by pure helpers
  (`getImageList` / `setImageList` / `mergeImageLists`, favourite and hidden flags,
  `dateFromLongDate` for Bing's `YYYYMMDDHHMM` timestamps).
- `extension.js` — the panel indicator (`BingWallpaperIndicator`), and all of the
  scheduling logic. Note the two independent timers: `_restartTimeout` (refresh, driven
  off Bing's own update time, backing off to `TIMEOUT_SECONDS_ON_HTTP_ERROR` = 1h on HTTP
  failure) and `_restartShuffleTimeout` (rotate the wallpaper from already-downloaded
  images). Wallpaper is applied by writing `picture-uri` into the
  `org.gnome.desktop.background` schema — this is the part with no COSMIC analogue and
  will need `cosmic-bg` config instead.
- `prefs.js` + `ui/prefsadw.ui` + `schemas/*.gschema.xml` — settings are declared once in
  the GSettings schema and bound to a libadwaita UI file.

Its own commands (run from inside that directory, only if you need to exercise the
reference):

```bash
npm run lint          # eslint *.js
./buildzip.sh         # compile schemas + gettext catalogues, produce the extension zip
./install.sh          # buildzip, unzip into ~/.local/share/gnome-shell/extensions, enable
```
