# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build / test / run

This machine's linuxbrew `pkg-config` shadows the system one and misses
`/usr/lib64/pkgconfig` (breaks the `xkbcommon` probe in `smithay-client-toolkit`).
**Every cargo invocation needs**
`PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig` — the `justfile`
exports it, so prefer `just` recipes; otherwise export it yourself:

```bash
just check        # cargo fmt --check + clippy --all-targets -D warnings + cargo test
just build        # cargo build --release
just install      # install binary + .desktop + icon into ~/.local (no sudo)
just uninstall

# raw cargo (export PKG_CONFIG_PATH first):
export PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Logging goes through `tracing` with an env-filter and is silent by default;
set `RUST_LOG` to see it, e.g. `RUST_LOG=cosmic_bing_wallpaper=debug` (or
`RUST_LOG=info` for everything) when running the binary or a test.

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

- `src/main.rs` — entry point: `localize()` then `cosmic::applet::run::<Window>(())`.
- `src/localize.rs` — Fluent i18n: `rust-embed`ded `i18n/<locale>/cosmic_bing_wallpaper.ftl`,
  the `LANGUAGE_LOADER` `LazyLock`, the crate's own `fl!` macro, `localize()`,
  and the locale guard tests. See "i18n" below.
- `src/app.rs` — the `cosmic::Application` impl (`Window`): message loop, popup
  open/close, startup restore (catalogue + config, no network), the refresh
  pipeline split in two: `run_refresh`/`fetch_and_download` (async, off the UI
  thread, injectable dirs + base URL for tests: fetch list → download missing →
  thumbnails) and the `RefreshFinished` handler (UI thread, against *live*
  state: merge → prune protecting the live current wallpaper → save →
  auto-apply per the "don't clobber" rule → reschedule — never merge/prune in
  the async task, its snapshot goes stale during a long fetch), one-shot
  generation-counter timers for refresh and shuffle (a stale tick is ignored, so
  rescheduling atomically replaces the pending timer), `state_dir()`/`catalogue_path()`,
  and the tested pure decisions `refresh_success_plan`/`config_diff`.
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
  `current_source`/`is_ours`/`should_auto_apply` back the don't-clobber rule,
  `download_dir()`. `apply`/`current_source` have no automated coverage (the
  cosmic-bg config context cannot be rooted in a tempdir from this crate — see
  the comment on `apply`); they are covered by the manual smoke test only.
- `src/schedule.rs` — pure timing math: `next_refresh` (reference-exact,
  including the out-of-range reset to 60 s and the +300 s fudge),
  `shuffle_interval` (sanitizes hand-edited values — `0`/tiny must never
  strobe), `fetch_count(retention_days)`, `retention_reduced`.
- `src/testutil.rs` — test-only loopback HTTP mock server (`spawn_mock`) and
  in-memory JPEG factory; all network branches are tested hermetically, nothing
  ever reaches the real Bing.

Design decisions, live-verified Bing/cosmic-bg facts, and per-task
implementation notes live in `docs/plans/` (`20260807-cosmic-bing-wallpaper-applet.md`
for the applet itself, `20260808-ux-polish-lockscreen-i18n.md` for tooltips /
disabled styling / i18n / theme conformance — each in `docs/plans/` or, once
archived, `docs/plans/completed/`). Gotchas recorded there worth knowing: the
`zune-jpeg` `log`-feature workaround in `Cargo.toml`, the transitive
`cosmic-config` pin living only in `Cargo.lock`, and the live-verified
lock-screen propagation chain (applet → cosmic-bg config → cosmic-bg state →
greeter state-watch — intact; only the *login* greeter can't read `~/Pictures`,
a uid/permissions gap, not an applet bug).

## UI conventions

Anything that renders *outside* its parent's bounds must be a real wayland
popup, never an iced overlay — an overlay is clipped to the applet popup
surface. Two cases, same rule:

- **Dropdowns** use `widget::dropdown::popup_dropdown(..)` with the
  `Message::Surface(cosmic::surface::Action)` forwarder (see the
  interval/retention rows in `view.rs`).
- **Tooltips** use `Core::applet_tooltip(..)`, not `widget::tooltip`. Inside the
  popup go through `view::popup_tooltip(window, content, text)`, which pins the
  two non-obvious arguments: `has_popup: false` (the tooltip surface is only
  created when that is `false`) and `parent_id: window.popup` (parent to the
  popup, not the panel). The panel button in `app.rs` is the mirror image:
  `has_popup: self.popup.is_some()` (suppressed while the popup is open) and
  `parent_id: None`.

Disabled icon buttons: the theme's own disabled styling is a **no-op** for
`Button::Icon` — `on_disabled` differs from `on` in alpha only, and the SVG
rasteriser tints RGB while keeping source alpha; the background it half-fades
is already fully transparent. Dim explicitly instead:
`icon::from_name(..).icon().opacity(icon_opacity(enabled))` in `view.rs`
(`nav_button` open-codes what `button::icon` builds, since that constructor
exposes no path to the inner `Icon`). Don't "fix" this with
`.class(theme::Button::Icon)` — it is already set.

## i18n

Every user-visible string goes through the crate's `fl!` macro; none are
written inline. Ids live in `i18n/en/cosmic_bing_wallpaper.ftl` (the fluent
domain is the crate name) and `i18n-embed-fl` resolves them **at compile time**,
so a typo or a missing id is a build error. `i18n/` holds all 73 locales COSMIC
ships; the 72 non-English ones are machine-generated. Notes:

- `localize()` (the only `DesktopLanguageRequester` caller) is reachable from
  `main` alone — the crate's `fl!` deliberately does *not* call it, unlike
  libcosmic's copy. That is what pins the test binary to `en`, so tests keep
  asserting literal English strings; keep it that way (`loader_is_pinned_to_english`).
- The loader sets `set_use_isolating(false)` — otherwise every placeable comes
  back wrapped in U+2068/U+2069.
- Localized label arrays must be functions returning `Vec<String>`
  (`shuffle_interval_labels`/`retention_labels`), never consts or `LazyLock` —
  a static would freeze the labels before the language is selected.
- Adding a locale means adding a directory *and* bumping `COSMIC_LOCALES` in
  `localize.rs`; the three guard tests there assert the dir count, that every
  locale defines exactly `en`'s ids (catching both missing keys and broken
  fluent syntax, which fluent otherwise only logs), and that every message keeps
  `en`'s `$variable` set.
- `data/…desktop`'s `Comment[<locale>]=` lines are separate from Fluent
  (desktop-entry spec, POSIX locale tags); `Name=` stays untranslated.

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
