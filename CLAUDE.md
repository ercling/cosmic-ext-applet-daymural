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
  and the tested pure decision `refresh_success_plan`.
  The out-of-window thumbnail backfill at the end of `fetch_and_download` is
  governed by the `Backfill` policy struct (budget + retention + applied file,
  injected by tests). Its rule: **the budget buys decodes, and no entry is ever
  paid for twice.** Three free skips come first — an entry the imminent prune
  will delete (`ImageEntry::within_retention`, the *same* test `prune` applies,
  applied file exempt), one already cached (`thumbs::is_cached`), and one that
  already failed to decode (`thumbs::decode_failed`) — then every real
  `image::open` costs budget whether it succeeds or not, and a failure is
  recorded. Weakening any one of the three re-opens a starvation or an
  unbounded-retry bug fixed before; do not "optimize" the ordering.
  That backfill (`backfill_thumbnails`) is also run **on its own at startup**
  (`run_thumbnail_pass`, armed by `start_thumbnail_pass_over` from `init`):
  previews come from the cache, a non-empty catalogue is not a cold start, and
  the first refresh can be ~24 h out — or never, offline — so thumbnail
  generation must never depend on a fetch. While either producer is running
  (`refresh_pending` / `thumbnail_pass_pending`, i.e. `may_sweep_thumbnails`)
  the prune's `thumbs::reconcile` sweep is deferred: fetched entries only join
  the catalogue at `RefreshFinished`, so an unconditional sweep deletes what
  the in-flight pass just wrote. Every pass ends in a sweep of its own.
- `src/view.rs` — popup UI (thumbnail, title/copyright, About link, prev/next/
  newest/refresh controls, shuffle + retention rows, status footer) plus the pure
  display helpers (`displayed`/`prev_target`/`next_target`/`newest_target`,
  `format_updated`, dropdown index↔value mappings) which *are* unit-tested; iced
  view code itself is exempt from tests.
- `src/tooltip.rs` — the hover tooltip, as its own wayland popup: upstream's
  `Core::applet_tooltip` plumbing (positioner, 100 ms delay, one shared surface
  id) re-implemented so the tooltip *surface* can be styled — upstream paints it
  in the popup's own background colour, which made the label unreadable over the
  popup. Both `view.rs` and `app.rs` build their tooltips through it.
- `src/bing.rs` — Bing API types + parsing (fixture:
  `tests/fixtures/hpimagearchive.json`), title/copyright derivation
  (`split_copyright` — Bing's own `title` field is the literal `"Info"`), pure
  URL/filename builders and the inverse `parse_filename`, reqwest client +
  `fetch_image_list` + atomic `.part`-then-rename `download_image`.
- `src/thumbs.rs` — 480×270 thumbnail cache in the state dir; the UI never
  decodes the full ~5 MB UHD file. Each cache slot has a `<thumb>.meta`
  sidecar holding the source's *identity* (mtime + size) and the outcome
  (`cached`/`failed`), compared for **exact equality** — never an mtime ordering,
  which cannot answer "unchanged?" and "changed?" with one test and made the
  suite flaky at timestamp ties. So `is_cached`/`decode_failed` are one
  predicate (`slot`), an undecodable image is opened once rather than once per
  refresh, and a repaired file is retried. Only `image::open` failing writes a
  `failed` verdict — a state-dir write failure never condemns a decodable
  image. Cleanup is `reconcile(live_filenames, state_dir)`: a *sweep* of the
  thumbs dir (not a removal list), so artefacts a prune-racing backfill wrote
  are still collected.
- `src/fsutil.rs` — shared atomic-write mechanics: `temp_sibling(dest, suffix)`
  and `write_atomic(dest, suffix, write)` (write to a temp sibling, then
  rename). Used by `bing::download_image`, `Catalogue::save` and
  `thumbs::ensure_thumbnail` — never hand-roll another temp-then-rename.
- `src/catalogue.rs` — `ImageEntry`/`Catalogue`: JSON persistence (atomic write),
  merge-with-dedupe by `urlbase`, retention prune (never deletes the currently
  applied file), `rebuild_from_folder` (filename ↔ urlbase mapping is
  deterministic both ways, so rebuilds dedupe against the next fetch with no
  re-downloads), navigation helpers. Two guards keep a transient from erasing
  the history: `prune` is a no-op while `images_dir` cannot be enumerated (an
  absent folder is not evidence that its files are gone — otherwise every entry
  looks vanished and the startup sweep *persists* that), and `load_or_rebuild`
  rescans the folder for an **empty** catalogue as well as an unusable one (a
  valid-but-empty JSON would otherwise load fine forever).
- `src/config.rs` — `AppletConfig` (shuffle on/off, interval, retention) via
  cosmic-config under app ID `io.github.ercling.CosmicBingWallpaper`, version 1,
  write-on-change setters, watch subscription for external edits.
- `src/wallpaper.rs` — cosmic-bg config writer: `updated_entry` mutates only
  `source`, `apply` writes the `all` entry *before* flipping `same-on-all`,
  `current_wallpaper`/`is_ours`/`should_auto_apply` back the don't-clobber rule,
  `download_dir()`. Only the cosmic-bg *context* plumbing inside
  `apply`/`current_wallpaper` is uncovered (the context cannot be rooted in a
  tempdir from this crate — see the comment on `apply`); the three-way state
  mapping is the pure, tested `classify`.
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
greeter state-watch — our end is intact and stays that way).

⚠️ **Both of that plan's lock-screen conclusions were wrong; corrected in place
there (2026-08-08, read against the installed cosmic-greeter 1.5.0 sources).**
Two independent facts, worth knowing before anyone "fixes" `wallpaper.rs` over
a lock-screen report:

- **The lock screen genuinely does not follow (observed by the user, then
  traced).** Not our chain — cosmic-greeter's locker cache. Each cosmic-bg state
  write clears `surface_images` and rebuilds (`src/locker.rs:1023-1027`); the
  rebuild silently `continue`s past any surface missing from `surface_names`
  (`src/common.rs:148-150`); unlocking removed those ids
  (`src/locker.rs:1004`, `1133`); locking re-inserts the names
  (`src/locker.rs:968`) but never rebuilds — so `view_window` serves the bundled
  `res/background.jpg` (`src/locker.rs:1167-1172`). First lock after login is
  right; later locks are the default; a lock that is up when a state write lands
  flips to the real wallpaper. cosmic-bg rewrites state every
  `rotation_frequency` seconds even for a single-file source
  (`cosmic-bg/src/wallpaper.rs:320-352`), so the churn is constant. Note the
  skip is **silent** — an empty journal is not evidence the wallpaper arrived,
  which is exactly the wrong inference made once already. Filed upstream as
  [cosmic-greeter#511](https://github.com/pop-os/cosmic-greeter/issues/511)
  (their #460/#497 are the same symptom without a repro). Don't "fix" this here.
- **Permissions are not the login-screen gate; the display manager is.** The
  greeter process never opens the image: `cosmic-greeter-daemon` runs as root and
  reads each user's cosmic-bg state *and* the bytes inside `run_as_user`
  (`daemon/src/main.rs` — HOME swap + `initgroups`/`setegid`/`seteuid`), then
  ships them as RON over the system bus for the greeter to render from memory
  (`bg_path_data`, `src/common.rs`). So a `drwxr-x---` home is no barrier and
  loosening home permissions is not a workaround to suggest. What decides it is
  greetd + `cosmic-greeter.service` + `cosmic-greeter-daemon.service` being the
  login path (on this machine all three are disabled and GDM is the DM).

## UI conventions

Anything that renders *outside* its parent's bounds must be a real wayland
popup, never an iced overlay — an overlay is clipped to the applet popup
surface. Two cases, same rule:

- **Dropdowns** use `widget::dropdown::popup_dropdown(..)` with the
  `Message::Surface(cosmic::surface::Action)` forwarder (see the
  interval/retention rows in `view.rs`).
- **Tooltips** go through `crate::tooltip::tooltip(..)`, never `widget::tooltip`
  (an overlay) and no longer `Core::applet_tooltip` — see `src/tooltip.rs` for
  why upstream's copy is unusable here (its surface is painted in the *same*
  colour as the popup, so the label had no readable background). Inside the
  popup call `view::popup_tooltip(window, content, text)`, which pins
  `parent_id: window.popup` (parent to the popup, not the panel).
  **The panel button deliberately has no tooltip** (`app::Window::view`):
  status applets don't announce their own name on hover, and ours was the only
  tray icon doing it. libcosmic's applet example does wrap the panel button —
  don't "restore" it from there, and don't reintroduce a `panel-tooltip`
  message id.

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
  back wrapped in U+2068/U+2069. **It only affects bundles that already exist**,
  and `select`/`load_languages` swap in brand-new ones with fluent's
  `use_isolating: true` default, so it must be re-applied after *every* language
  load: go through `localize::disable_bidi_isolation` / `select_languages`, never
  call `select` directly.
- Localized label arrays must be functions returning `Vec<String>`
  (`shuffle_interval_labels`/`retention_labels`), never consts or `LazyLock` —
  a static would freeze the labels before the language is selected.
- **Adding or renaming a message id means editing all 73 catalogues**, not just
  `i18n/en/`: `every_locale_defines_every_english_message` asserts each locale
  defines *exactly* `en`'s ids, so a lone English addition turns `just check`
  into 72 failures. Same for placeables — a new `{ $variable }` must appear in
  every locale's copy of that message
  (`every_locale_preserves_the_english_placeables`), and no locale may invent a
  `{ reference }` `en` does not have.
- Adding a locale means adding a directory *and* listing it in `COSMIC_LOCALES`
  in `localize.rs` (a sorted array, compared as a set — a locale *swapped* for
  another is caught too). The guard tests there also assert that every locale
  defines exactly `en`'s ids (catching both missing keys and broken fluent
  syntax, which fluent otherwise only logs), that placeables survive
  translation, and that every id is actually rendered by `app.rs`/`view.rs`
  (`every_message_id_is_referenced_by_the_ui` — dropping a tooltip must not
  leave 73 orphaned strings behind).
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
