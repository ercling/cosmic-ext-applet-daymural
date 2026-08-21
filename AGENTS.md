# Repository Guide

## Project

`cosmic-bing-wallpaper` is a Rust 2024/libcosmic panel applet for the COSMIC
desktop. It fetches Bing's daily UHD image, maintains a local catalogue and
thumbnail cache, applies wallpapers through `cosmic-bg`, optionally shuffles
them, and can derive the COSMIC accent colour from the current wallpaper.

This is one self-contained applet binary, not a daemon. Timers and background
work exist only while `cosmic-panel` is running.

## Build, Test, and Run

Prefer the `justfile`; it exports the `PKG_CONFIG_PATH` required on this
development machine, where linuxbrew's `pkg-config` otherwise hides the system
`xkbcommon` metadata.

```bash
just check        # cargo fmt --check, clippy -D warnings, all tests
just build        # release build
just install      # per-user install under ~/.local; no sudo
just uninstall
just flatpak-sources
just flatpak-prefetch
just flatpak-build-offline
just flatpak-install
just flatpak-uninstall
```

For a raw Cargo command, set:

```bash
export PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig
```

Then use `cargo test`, `cargo clippy --all-targets -- -D warnings`, or
`cargo fmt`. `rustfmt` and `clippy` are installed in `~/.local/bin` to match the
system Rust toolchain (currently Rust 1.97.1).

Run `just check` before handing off code changes. Add or update hermetic tests
for every behavior change; iced view construction itself is the established
exception. Tests must use injected paths or `Config::with_custom_path` rooted
in a `tempfile::TempDir` and must never touch real user configuration, state,
wallpapers, or Bing.

Do not casually run the applet binary. A cold standalone start with an empty
`~/Pictures/BingWallpaper` performs a real Bing fetch and applies a wallpaper
after a short delay. Production logging is silent by default; use
`RUST_LOG=cosmic_bing_wallpaper=debug` when intentional runtime diagnosis is
needed.

## Repository Map

- `src/main.rs`: localization initialization and applet entry point.
- `src/app.rs`: `cosmic::Application`, message loop, popup lifecycle, refresh
  and timer orchestration, catalogue/config restoration, and accent jobs.
- `src/view.rs`: popup UI plus pure navigation, formatting, dropdown, and
  tooltip helpers.
- `src/tooltip.rs`: styled Wayland popup tooltips; these are deliberately not
  iced overlays or upstream `Core::applet_tooltip`.
- `src/bing.rs`: Bing response parsing, URL/filename conversion, HTTP fetch,
  and atomic downloads.
- `src/catalogue.rs`: persisted image catalogue, merge, rebuild, navigation,
  and retention pruning.
- `src/thumbs.rs`: 480x270 cache, source-identity sidecars, failed-decode
  records, and cache reconciliation.
- `src/fsutil.rs`: shared sibling-temp-plus-rename atomic-write helpers.
- `src/config.rs`: applet configuration via `cosmic-config`.
- `src/wallpaper.rs`: `cosmic-bg` reads/writes, do-not-clobber decisions, and
  lock-screen state pokes.
- `src/lockwatch.rs`: logind lock/resume subscription and the greeter-cache
  workaround.
- `src/accent.rs`: colour extraction and the guarded asynchronous accent
  snapshot/write/restore state machine.
- `src/schedule.rs`: pure refresh, shuffle, retention, and fetch-count math.
- `src/localize.rs`: Fluent loader, project `fl!` macro, locale selection, and
  translation guards.
- `src/testutil.rs`: test-only HTTP/JPEG and popup-surface helpers.
- `data/`: desktop entry, app-ID icon, and AppStream metainfo installed by
  both packaging routes.
- `io.github.ercling.CosmicBingWallpaper.json`: developer Flatpak manifest and
  the authoritative scoped sandbox contract.
- `flatpak/`: pinned Cargo source generator and its `uv` wrapper; generated
  `cargo-sources.json` is deliberately ignored.
- `.github/workflows/`: native Rust checks and the Freedesktop 25.08 Flatpak
  build/bundle job.
- `tests/fixtures/`: hermetic external-data fixtures.
- `docs/plans/`: design evidence and implementation plans. Read the relevant
  plan before changing a subtle subsystem.
- `examples/`: ignored, independent reference clones. Do not modify or expect
  the parent repository to track them.

`README.md` is the user-facing behavior and installation contract.
`CLAUDE.md` is a detailed legacy architecture handoff. It contains valuable
historical investigation and exact upstream source references, but this file
is the active repository instruction source for Codex. Keep durable design
detail in `docs/plans/` and keep this guide concise.

## Dependency and Compatibility Rules

The committed `Cargo.lock` pins both git dependencies. `cosmic-bg-config` also
has a `rev` in `Cargo.toml`; `libcosmic` deliberately uses the bare repo URL so
its source id matches cosmic-bg-config's transitive `cosmic-config`. Adding a
`?rev=` source id splits the repo and breaks offline Flatpak vendoring.
libcosmic moves quickly: inspect the locked revision or resolved source rather
than coding from remembered APIs.
Do not unpin, update, or add production dependencies as an incidental fix.

Preserve the applet's current Rust edition/toolchain compatibility. Keep the
`zune-jpeg` feature workaround and the transitive `cosmic-config` lockfile pin
unless the underlying dependency facts have been re-verified.

## Core Behavioral Invariants

### Refresh, catalogue, and timers

- Keep network/download/thumbnail work off the UI thread.
- The async refresh task works from a snapshot, but merge, prune, save, and
  auto-apply happen in `RefreshFinished` against live UI-thread state. Never
  move merge/prune into the async task; a long fetch makes its snapshot stale.
- Refresh and shuffle use one-shot generation-counter timers. A reschedule
  replaces the prior timer atomically, and stale ticks must be ignored.
- Auto-apply follows the do-not-clobber rule: if the user selected a wallpaper
  outside this applet, background refresh must not replace it.
- `Catalogue::prune` must protect the currently applied image. Failure to
  enumerate the image directory is not evidence that every image disappeared;
  do not persist an empty history in that case.
- A valid-but-empty catalogue must still rebuild from the image directory.

### Files and thumbnails

- Use `fsutil::write_atomic` or the shared sibling-temp helpers. Do not
  hand-roll another temp-then-rename path.
- Thumbnail cache validity is exact source identity (mtime and size), not mtime
  ordering. A repaired source must be retried; an unchanged decode failure must
  not be retried on every refresh.
- In backfill, free skips occur before budget use: entries doomed by the same
  retention predicate as prune (except the applied image), already-cached
  entries, and already-recorded decode failures. Every actual decode attempt
  consumes budget, successful or not, and no entry is paid for twice.
- Defer a thumbnail reconciliation sweep while either thumbnail producer is
  running. Each producer performs its own final sweep after its catalogue view
  is safe.
- Startup thumbnail generation must not depend on a network refresh; a
  non-empty restored catalogue may otherwise wait about a day or remain
  offline forever.

### Popup stack and UI

`window.popup` may have at most one child Wayland popup at a time. A tooltip and
a dropdown are siblings on the same xdg-shell stack, whose destroy order is
protocol-sensitive.

- Dropdowns must use `popup_dropdown` and route surface actions through the
  ledger-aware `Message::DropdownSurface` path.
- Popup tooltips must use `crate::tooltip::tooltip` through
  `view::popup_tooltip`. Do not replace them with clipped iced overlays, and do
  not add a tooltip to the panel button.
- Every popup parented to `window.popup` needs a ledger-aware message. Do not
  restore a blind `Message::Surface` forwarder.
- Never clone `cosmic::surface::Action`; popup creation relies on
  `Arc::try_unwrap` and a surviving clone silently prevents creation.
- Preserve the ordered tooltip-destroy/drop/suppression interlock and the
  close-debt accounting. `PopupClosed` also arrives after self-initiated
  destroys and can arrive after a newer popup session starts.
- `dropdowns_open` is deliberately a saturating count biased toward open, not
  a boolean. A false positive merely suppresses tooltips; a false zero can map
  an illegal sibling popup.
- Do not decrement the count on a `DestroyPopup` request; only the eventual
  close proves a mapped popup disappeared.
- The remaining crash when the compositor dismisses an open dropdown chain is
  a pinned-libcosmic ancestor-first destroy-order bug. Read
  `docs/plans/completed/20260810-popup-destroy-order-crash.md` and the popup
  section of `CLAUDE.md` before changing this ledger.
- Disabled icon buttons require explicit inner-icon opacity; the theme's
  disabled `Button::Icon` styling does not visibly dim these SVGs.

### Accent state machine

Accent matching is opt-in and must preserve a user's theme choice. Before
changing it, read the accent section of `CLAUDE.md` and
`docs/plans/completed/20260808-accent-from-wallpaper.md`.

- The wallpaper contributes hue only. Tone/chroma come from each mode's own
  builder palette, followed by fixed-hue gamut mapping and a contrast guard.
- Theme writes never run inline in `Application::update`; they are blocking
  tasks guarded so at most one is in flight.
- Snapshot persistence must succeed before the first theme write it may need
  to undo. Record `accent_last_written` only after the write lands; failed
  writes and failed record persists follow the existing rollback/repair paths.
- Treat in-memory accent state as authoritative during normal operation.
  Watcher payloads can be late: never adopt snapshot/last-written fields from
  them, and confirm external toggles with a fresh disk read.
- Before writing, compare current builder accents with the last value this
  applet wrote (or the enable-time stand-in). A mismatch is a user choice:
  disarm without overwriting it. A computed value equal to the recorded value
  is a no-op.
- Write only the builder's `accent` key, then transact only serialized theme
  keys whose bytes change. A virgin theme directory is the intentional
  full-write exception.
- Recompute through the established async path after successful applies,
  startup reconciliation, and thumbnail-pass completion. Do not decode full
  UHD images in the accent path.

### Lock-screen workaround

The lock/resume poke deliberately writes a semantically equivalent but
value-different `wallpapers` list so cosmic-config delivers an update to
cosmic-greeter. Identical rewrites are deduplicated and do not heal its cache.
The toggle normalizes first-entry-per-output while preserving first entries and
order, or appends a duplicate of the last entry. Empty/unreadable state must
never be written. Do not remove this as "not our bug" without re-verifying the
upstream issue and deployed COSMIC behavior.

## Internationalization

All user-visible strings use this crate's compile-time `fl!` macro. Do not add
inline UI strings.

- `i18n/en/cosmic_bing_wallpaper.ftl` is the human-authored source catalogue.
- Adding or renaming an id requires updating all 73 locale catalogues. Each
  locale must preserve the English placeables; guard tests enforce both rules.
- Localized label collections are functions returning `Vec<String>`, not
  constants or `LazyLock`, so they observe the selected language.
- Tests intentionally stay pinned to English. `main` alone calls `localize()`;
  the `fl!` macro must not initialize desktop language selection.
- Language changes must go through `localize::select_languages` so bidi
  isolation is disabled again after Fluent replaces its bundles. Do not call
  the loader's `select` directly.
- When adding a locale, add its directory and update the sorted
  `COSMIC_LOCALES` list. Desktop-entry `Comment[locale]` values are separate
  from Fluent.

## Working Method

- Preserve unrelated working-tree changes and make focused patches.
- Prefer pure decision helpers and dependency/path injection for stateful
  behavior. Test success, failure, stale-event, and rollback paths where they
  apply.
- Declarative packaging files are embedded in `src/app.rs` tests with
  `include_str!`. Cross-check their identities, commands, paths, permissions,
  and workflow inputs there, with both success and deliberate-drift cases;
  leave syntax validation to the format-specific external tools.
- Keep UI-thread work small. File/network/theme operations that may block
  belong in async or blocking-pool tasks with completion messages checked
  against live state.
- Update the user-facing README when behavior, settings, installation, or
  limitations change. Update the relevant plan when implementation diverges
  from recorded design.
- Multi-output panels run one applet process per output, but exactly one active
  leader owns shared background, destructive, lock-screen, and accent work.
  Followers proxy refresh/apply coordination, persist ordinary settings one
  key at a time, and hydrate fresh state before takeover. Preserve the gates
  and mailbox rules recorded in
  `docs/plans/completed/20260817-single-instance-leader.md`.

## Code Review Rules

- Flag any test or diagnostic path that can contact Bing or modify real COSMIC
  config, theme, state, wallpapers, or files outside an injected tempdir.
- Flag blocking I/O in the UI update path.
- Flag unguarded stale async completions or timer ticks.
- Flag popup actions that bypass the ledger, clone a surface action, or weaken
  the one-child invariant.
- Flag catalogue/thumbnail cleanup that can race an in-flight producer or
  delete the currently applied wallpaper.
- Flag accent changes that can overwrite a user-selected colour, lose the
  restore snapshot, or persist a full stale config entry from a non-owner.
- Flag a new English Fluent id without matching locale/placeable updates.
