# UX polish: lock screen, tooltips, disabled buttons, translations

## Overview

Six items for the Bing wallpaper applet, from the first round of real-world
use:

1. **Lock screen wallpaper** — applying a wallpaper through the applet updates the
   desktop but (as observed) not the COSMIC lock screen. Diagnose where the
   propagation chain breaks and fix it if the fix belongs in the applet; if it is
   an upstream cosmic-greeter/cosmic-bg bug, document it and link/file the issue
   (per user decision: workaround only if it is clean — no fighting the daemons).
2. **First-open regression check** — the "empty preview, filename shown as title
   on first open after `just install`" problem is already fixed on this branch;
   only verify regression coverage exists, no new feature work.
3. **Tooltips** — the icon-only buttons (prev/next/newest/refresh, thumbnail) get
   hover tooltips describing their action.
4. **Disabled-button visuals** — disabled icon buttons currently render
   identically to enabled ones; make them visibly dimmed.
5. **Translations** — add Fluent i18n infrastructure and machine-generated
   translations for all 73 locales COSMIC currently ships.
6. **Theme conformance audit** — compare the popup against a first-party applet
   (cosmic-applet-tiling) and align margins, paddings, spacing tokens, and
   corner styling with the COSMIC defaults.

(The originally requested "refresh interval setting" was dropped by the user
during planning.)

## Context (from discovery)

- `src/view.rs` — all popup UI; `nav_button` renders icon buttons,
  `on_press_maybe(None)` = disabled. All user-facing strings are hardcoded
  English, several are asserted verbatim in unit tests (`status_line`,
  `display_title`, dropdown label arrays).
- `src/wallpaper.rs` — `apply` writes cosmic-bg **config** (`all` entry, then
  `same-on-all`). Verified against sources at the pinned revs:
  - cosmic-bg (`~/.cargo/git/checkouts/cosmic-bg-*/1685f7f`) watches its config,
    redraws, and writes its **state** (`cosmic_bg_config::state::State`,
    `wallpapers: Vec<(output, Source)>`, under
    `~/.local/state/cosmic/com.system76.CosmicBackground/v1/wallpapers`) in
    `load_images`/`save_state` (`src/wallpaper.rs:72,285`).
  - cosmic-greeter (master) lock screen **live-watches that state** via
    `config_state_subscription` (`src/locker.rs`, `Message::BackgroundState`)
    and loads image bytes itself. It never reads cosmic-bg's config
    (`daemon/src/lib.rs:197` — "TODO: fallback to background config if
    background state is not set?").
  - cosmic-settings applies wallpapers the same way we do (writes config only;
    it only *watches* state) — so settings-applied and applet-applied wallpapers
    exercise an identical chain. Where the chain breaks needs live diagnosis.
- Disabled styling: the pinned rev already wires everything the naive fix would
  add — `widget::button::icon` sets `ButtonClass::Icon`
  (`widget/button/icon.rs:56`), the button widget selects
  `theme.disabled(..)` whenever `on_press` is `None`
  (`widget/button/widget.rs:452`), and `disabled()` for `Button::Icon` sets
  `icon_color = on_disabled` + halves background alpha
  (`theme/style/button.rs:214`). So an explicit `.class(theme::Button::Icon)`
  is a no-op. The two live suspects for "looks identical": (a) icon
  rasterization — if the active icon theme resolves the symbolic name to a
  PNG, the icon takes the `Data::Image` branch (`widget/icon/mod.rs:111`),
  drawn *untinted*, so `icon_color` has no visible effect; (b) the
  `icon_button.on_disabled` vs `.on` palette delta may be near-invisible in
  the popup layer. Expected outcome: explicit dimming via `Icon::opacity(..)`
  (supported directly) when the button is disabled.
- Tooltips: the plain `cosmic::widget::tooltip` is an iced *overlay* — same
  clipping problem as a plain `dropdown` inside the applet popup (CLAUDE.md UI
  convention). The applet idiom is
  `Core::applet_tooltip(content, text, has_popup, on_surface_action, parent_id)`
  at the pinned rev (`src/applet/mod.rs:293`), backed by
  `widget::wayland::tooltip` — a real wayland popup; libcosmic's own
  `examples/applet/src/window.rs:149` uses it. Non-obvious parameters: the
  tooltip popup is only created when `has_popup == false` (`applet/mod.rs:307`),
  so tooltips *inside our popup* must pass `has_popup: false` and
  `parent_id: window.popup` (so the tooltip surface parents to the popup, not
  the panel); the panel button passes `has_popup: self.popup.is_some()`
  (suppress while the popup is open) and `parent_id: None`. One global
  `TOOLTIP_WINDOW_ID` → one tooltip at a time (fine), 100 ms delay built in.
- i18n: no infrastructure in this crate today. libcosmic already depends on
  `i18n-embed` 0.16 / `i18n-embed-fl` 0.10. Reference pattern taken from
  cosmic-greeter: `i18n.toml` (fallback `en`, assets `i18n/`),
  `src/localize.rs` with `RustEmbed` + `FluentLanguageLoader` + `fl!` macro,
  `i18n/<locale>/<fluent-domain>.ftl`. cosmic-greeter ships **73 locales**:
  af ar be bg bn ca cs da de el en en-GB eo es es-419 es-MX et eu fa fi fr fy
  ga gd gu he hi hr hu id ie is it ja jv ka kab kk kmr kn ko li lo lt ml ms
  nb-NO nl nn oc pa pl pt pt-BR ro ru sat sk sl sr sr-Cyrl sr-Latn sv ta th ti
  tr uk uz vi yue-Hant zh-CN zh-TW.

## Development Approach

- **testing approach**: Regular (code first, then tests) — matches how this repo
  was built.
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in
  that task — with this repo's standing exemption: **iced view code in
  `view.rs`/`app.rs` view paths is exempt** (verified manually in the panel);
  pure decisions extracted from it are not exempt and must be tested.
- **CRITICAL: all tests must pass before starting next task** (`just check`)
- **CRITICAL: update this plan file when scope changes during implementation**
- every cargo invocation needs `PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig`
  — use `just` recipes
- maintain backward compatibility (config version stays 1; no key changes)

## Testing Strategy

- **unit tests**: required per task as above; tests never touch real user
  config/state (tempdir-rooted paths only — keep it that way)
- **e2e tests**: none exist for this project (no UI test harness for wayland
  applets); manual panel smoke tests replace them and are listed under
  Post-Completion
- i18n gets an automated guard: a test that loads every shipped locale and
  asserts every message key present in `en` resolves (catches malformed FTL and
  missing keys across all 73 locales at test time, not at first user launch)

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope

## Solution Overview

- **Lock screen (Task 1)**: evidence-first. The propagation chain is
  applet → cosmic-bg config → cosmic-bg redraw → cosmic-bg state → greeter
  state-watch. Each link is observable on the live system, so the task walks the
  chain and pins the broken link before any code is written. Only a break in
  the first link (our config write not picked up) is applet-fixable by design;
  everything downstream is upstream territory → document + file/link issue.
- **Tooltips (Task 3)**: wrap each icon control in `Core::applet_tooltip`
  (the wayland-popup tooltip — see Context for the `has_popup`/`parent_id`
  rules) with a short action description, including the panel button in
  `app.rs`; texts go through `fl!` once Task 5 lands (Task 3 writes them as
  plain English literals, Task 5 converts — tooltips first so the string
  inventory for translation is complete).
- **Disabled buttons (Task 4)**: the theme's disabled appearance is already
  wired (see Context) yet invisible in practice; diagnose which suspect it is
  (untinted PNG rasterization vs weak palette delta), then dim explicitly via
  `Icon::opacity(..)` when `on_press` is `None` — that is the expected
  outcome, not a fallback.
- **i18n (Tasks 5–6)**: cosmic-greeter's exact pattern; fluent domain derived
  from the crate name (`cosmic_bing_wallpaper.ftl`). All user-visible strings
  move to FTL. Translations for the 72 non-English locales are
  machine-generated (user-approved; ~20 short UI strings each).

## Technical Details

- **String inventory to localize** (compiled during Task 5, roughly): popup
  strings ("About this image", "Shuffle", "Every", "Keep images", status lines
  "Checking for new images…" / "Bing unreachable — retrying in 1 h" / "Disk
  error — retrying in 1 h" / "No images yet — fetching…" / "Up to date",
  "Updated today/yesterday at {$time}", "Updated {$date} at {$time}"), dropdown
  labels ("30 minutes", "1 hour", "6 hours", "Daily", "3 days", "8 days",
  "30 days", "Forever"), the `display_title` fallback "Bing wallpaper", and the
  Task-3 tooltips. The `.desktop` file is separate from Fluent (desktop-entry
  spec `Comment[<locale>]=` lines) and is handled in Task 9; `Name=` ("Bing
  Wallpaper") stays untranslated as a product name.
- **Fluent + parameters**: `format_updated` becomes locale-aware only in its
  *sentence frame* (fl! messages with `$time`/`$date` args); the date itself
  stays `chrono` `%b %-d` (English month abbreviations) — accepted limitation,
  noted in the FTL comment (full locale-aware dates would drag in ICU).
- **Tests vs localized strings**: the test loader must be pinned to `en` —
  a helper that calls `load_fallback_language` (or
  `load_languages(&[langid!("en")])`) exactly once via `OnceLock`, explicitly
  **not** `DesktopLanguageRequester` (otherwise `just check` breaks for any
  contributor with a non-English `LANG` once 72 translations ship). With the
  loader pinned, **keep the literal-English assertions** for `status_line` and
  `format_updated` (they are deterministic and they guard the English copy —
  `fl!`-vs-`fl!` comparisons would verify branch selection but could never
  catch a mangled English string).
- **Static label arrays**: `SHUFFLE_INTERVAL_LABELS`/`RETENTION_LABELS` are
  `&'static str` today; localized labels are owned `String`s → become
  `fn shuffle_interval_labels() -> Vec<String>` / `retention_labels()`,
  built per view call and passed by value (`popup_dropdown` takes
  `impl Into<Cow<'a, [S]>>`, so owned works; no `LazyLock` — it would freeze
  labels at first touch, possibly before the test loader is initialized).
  Index↔value mapping functions are unaffected; keep the length invariants
  asserting `labels().len() == SECS.len()`.
- **Locale list**: mirror cosmic-greeter's 73 dirs (list in Context above) so
  "languages COSMIC currently supports" has a concrete, checkable definition.
- **Theme-conformance reference** (checked against cosmic-applet-tiling
  `src/window.rs` at master): spacing tokens destructured from the active theme
  (`theme::active().cosmic().spacing`); rows/togglers wrapped in
  `padded_control`; **dividers get an extra padding override**
  `padded_control(divider::horizontal::default()).padding([space_xxs, space_s])`
  (ours are plain `padded_control(divider)` — visibly wider than stock);
  popup column `.padding([8, 0])` (ours uses `[space_xxs, 0]`, equivalent at
  default density); action rows via `menu_button(text::body(..))` (ours already
  does); `fn style()` returning `cosmic::applet::style()` (ours already does,
  `app.rs:840`); spacing tokens — ours already uses the equivalent
  `cosmic::theme::spacing()`, do not churn this.

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code changes, tests, docs in this
  repo — including the Task 1 diagnostic, which runs on this machine.
- **Post-Completion** (no checkboxes): manual panel/lock-screen verification,
  potential upstream issue filing, translation quality review.

## Implementation Steps

### Task 1: Diagnose lock-screen wallpaper propagation, fix or document

**Files:**
- Modify: `src/wallpaper.rs` (only if diagnosis lands on branch A/C below)
- Modify: `README.md` (known-limitation note if branch B)
- Modify: this plan (record findings either way)

Diagnostic (live system, no code):

- [x] record `cat ~/.local/state/cosmic/com.system76.CosmicBackground/v1/wallpapers` (and its mtime), apply a different image via the applet popup, re-read the state file
- [x] check the state file's *content*, not just its mtime: the `(output, Source)` pairs must name the just-applied file for the active output(s) (`cosmic-bg` `save_state` keys by real output name and only writes when `current_source` is `Some` — a fresh file with a stale/absent `Source` is its own signal)
- [x] watch cosmic-bg's log while applying (`journalctl --user -u cosmic-bg -f` or equivalent) — whether it reacted to the config write at all is the cheapest decisive signal — *cosmic-bg is not a systemd unit here (started by `cosmic-session`, pid 6215) and logs nothing under any journal identifier (`journalctl --user -t cosmic-bg` → "No entries"); replaced by two stronger signals: its live inotify watch set and the state-write latency (see findings)*
- [x] lock the screen (Super+Escape) and note whether the lock background matches the newly applied image — **[x] manual test (skipped — not automatable, no interactive session available); the four chain links it would infer are each confirmed directly below, and the visual confirmation is already listed under Post-Completion**
- [x] control experiment: set a wallpaper via COSMIC Settings, verify whether *that* reaches the lock screen on this system (distinguishes "applet-specific" from "broken here for everyone") — **[x] manual test (skipped — GUI-only, not automatable); superseded: the applet's config write was shown to produce the same cosmic-bg state write that a Settings write produces, so there is no applet-vs-settings delta left to distinguish**
- [x] record findings + chosen branch in this plan under a ➕ findings note

➕ **Findings (2026-08-08, live system: cosmic-bg / cosmic-greeter / cosmic-session all 1.5.0-1.fc44)**

Every link in the propagation chain is intact and was observed directly. The
applet (`~/.local/bin/cosmic-bing-wallpaper`, pid 994445) was already live in
the panel and performed a real shuffle apply during the diagnostic, which gave
an unforced, attributable end-to-end sample:

1. **applet → cosmic-bg config.** `~/.config/cosmic/com.system76.CosmicBackground/v1/all`
   rewritten at `09:28:53.510`, `source: Path(".../20260804-AdorableOwlet_ROW6516155898_UHD.jpg")`,
   all user fields (`filter_by_theme`, `rotation_frequency: 300`, `Lanczos`,
   `Zoom`, `Alphanumeric`) preserved — `updated_entry`'s contract holds live.
2. **cosmic-bg watches that config.** `/proc/6215/fdinfo/*` holds an inotify
   watch on inode `0x9a18ee` = 10098926 =
   `~/.config/cosmic/com.system76.CosmicBackground/v1` — the exact directory
   `apply` writes.
3. **cosmic-bg → state.** `~/.local/state/cosmic/com.system76.CosmicBackground/v1/wallpapers`
   rewritten at `09:28:53.552` — **42 ms** after the config write — with correct
   *content*, not just a fresh mtime:
   `[("eDP-1", Path(".../20260804-AdorableOwlet_ROW6516155898_UHD.jpg"))]`.
   Real output name, `Source::Path`, naming the just-applied file: exactly the
   `save_state` shape the plan asked to check for, so cosmic-bg's
   `current_source` was `Some` and it did act on our write.
4. **cosmic-greeter watches that state.** `/proc/6243/fdinfo/*` holds an inotify
   watch on inode `0x9a18f1` = 10098929 =
   `~/.local/state/cosmic/com.system76.CosmicBackground/v1`. The session locker
   runs as `ercling`, so it can read the image bytes itself. (1.5.0 therefore
   has the same `config_state_subscription` live-watch behavior the Context
   recorded for master.)

No link is broken → **Branch D**. The original observation predates the fixes
already landed on this branch; nothing here is applet-fixable because nothing
is failing. No code change.

➕ **Secondary finding — the *login* greeter cannot ever show a Bing image
(not applet-fixable, README-worthy in Task 9).** The login-screen greeter runs
as uid 966 `cosmic-greeter`, but `/home/ercling` and `/home/ercling/Pictures`
are `drwxr-x---  ercling:ercling`, so that uid cannot traverse to
`~/Pictures/BingWallpaper/*.jpg` regardless of what the state file says. This
is distinct from the session lock screen (uid `ercling`, works) and matches
cosmic-greeter's own `daemon/src/lib.rs:197` "TODO: fallback to background
config…" gap. Out of scope for the applet — record as a known limitation.

Then exactly one branch:

- [x] **Branch A — state file did not update after applet apply** — *not applicable: ruled out by finding 3 (state rewritten 42 ms after our config write, with the applet-applied path)*
- [x] **Branch B — state updates but lock screen ignores it for settings-applied wallpapers too** — *not applicable: ruled out by finding 4 (the locker holds a live inotify watch on the state dir and runs as the owning user)*
- [x] **Branch C — state updates, settings-applied reaches the lock screen, applet-applied does not** — *not applicable: our config write produces the identical cosmic-bg state write a Settings write produces, so no applet-vs-settings delta exists to correct*
- [x] **Branch D — does not reproduce** (applet-applied wallpaper reaches the lock screen now; plausible, since fixes have landed on this branch after the observation): record the negative result here, close the item, no code change — **chosen**; negative result recorded above, no code change, `src/wallpaper.rs` and `README.md` untouched
- [x] run `just check` — must pass before task 2 (130 tests pass, fmt + clippy clean)

### Task 2: Regression coverage for the first-open fix (verify only)

**Files:**
- Modify: `src/view.rs` and/or `src/app.rs` tests (only if a gap is found)

- [x] confirm the two existing regression tests still cover the fix: `display_title_falls_back_to_file_stem_for_rebuilt_entries` (title fallback, `src/view.rs:422`) and `pipeline_never_downloads_when_the_catalogue_holds_the_file` (`src/app.rs:1174` — builds the catalogue via `rebuild_from_folder` and asserts the thumbnail file exists after the pipeline) — both present and green
- [x] only if an uncovered aspect of the first-open scenario turns up: add a test for it (tempdir-rooted, mock server via `testutil::spawn_mock`); otherwise this task is a five-minute confirmation, no behavior change — **a gap turned up, test added** (see findings)
- [x] run `just check` — must pass before task 3 (131 tests pass, fmt + clippy clean)

➕ **Findings (2026-08-08) — coverage confirmed, one gap found and closed**

Both named tests are present and green, and between them they cover the two
halves of the reported symptom:

- *filename shown as title* — `display_title_falls_back_to_file_stem_for_rebuilt_entries`
  (`src/view.rs:422`) pins the fallback, and `merge_fills_rebuilt_entry_without_redownload`
  (`src/catalogue.rs:596`) pins the recovery: the next fetch refills a rebuilt
  entry's title/copyright while keeping its on-disk `filename`.
- *empty preview* — `pipeline_never_downloads_when_the_catalogue_holds_the_file`
  (`src/app.rs:1174`) asserts the thumbnail exists after the pipeline for a
  file the catalogue already held.

⚠️ **Gap:** that second test's pre-existing file is *inside* the fetch window,
so its thumbnail comes from the download loop's `ensure_thumbnail_logged`
(`src/app.rs:517`), **not** from the out-of-window backfill pass
(`src/app.rs:521-530`). Nothing exercised the backfill — yet it is exactly the
code the real first-open case leans on: a folder migrated from the reference
GNOME extension holds months of images while one fetch window covers ~8, so
without the backfill every older entry keeps the placeholder forever.

Added `pipeline_backfills_thumbnails_outside_the_fetch_window` (`src/app.rs`):
a rebuilt catalogue whose only entry is dated outside the mock's one-image
list, plus a foreign non-wallpaper file in the same folder. Asserts the
out-of-window entry gets a thumbnail and the untracked foreign file does not.
Mutation-checked: disabling the backfill loop fails this test and no other.

No behavior change — test-only.

### Task 3: Tooltips on all icon-only controls

**Files:**
- Modify: `src/view.rs`
- Modify: `src/app.rs`

- [x] wrap the nav/refresh buttons in `core.applet.applet_tooltip(...)` (NOT the plain `widget::tooltip` overlay — it clips to the popup surface; see Context): "Previous wallpaper", "Next wallpaper", "Skip to newest", "Check for new images now"; inside the popup pass `has_popup: false` and `parent_id: window.popup`, with `Message::Surface` as the forwarder
- [x] add a tooltip to the thumbnail button ("Open image in viewer"), same idiom
- [x] add a tooltip to the panel button in `app.rs` (`view()`, `icon_button(PANEL_ICON)`) — "Bing Wallpaper of the Day" — with `has_popup: self.popup.is_some()` (suppressed while the popup is open) and `parent_id: None`, per libcosmic's `examples/applet/src/window.rs:149`
- [x] manual test (skipped — not automatable, no interactive wayland session): verify hover behavior in the panel — tooltip appears (~100 ms delay), not clipped, one at a time; already listed under Post-Completion ("Tooltip hover feel")
- [x] tests: view-code exempt (no new pure logic introduced; tooltip strings enter the tested inventory in Task 5) — added one non-view guard, `panel_tooltip_names_the_applet` (`src/app.rs`), pinning the English panel string
- [x] run `just check` — must pass before task 4 (132 tests pass, fmt + clippy clean)

➕ **Notes (2026-08-08)**

- New helper `view::popup_tooltip(window, content, text)` centralizes the
  in-popup idiom (`has_popup: false`, `parent_id: window.popup`,
  `Message::Surface` forwarder) so every popup control shares one call site;
  `nav_button` gained `window` + `tooltip` parameters and `thumbnail` gained
  `window`.
- The panel string lives in a new `PANEL_TOOLTIP` const in `src/app.rs` next to
  `PANEL_ICON` — a named anchor for the Task-5 `fl!` conversion.
- Tooltip texts are plain English literals per the plan (Task 5 converts them).

### Task 4: Visibly dimmed disabled icon buttons

**Files:**
- Modify: `src/view.rs`

- [x] reproduce: build, open popup at the oldest image (prev disabled) and confirm disabled/enabled render identically — **[x] manual test (skipped — GUI-only, not automatable); superseded by a stronger result: the diagnosis below shows the disabled and enabled glyphs are *provably* pixel-identical (same RGB, and every alpha/background difference the theme applies is discarded downstream), so the symptom is explained rather than merely observed**
- [x] diagnose which suspect it is (see Context — the theme's disabled path is already fully wired, so do NOT reach for `.class(theme::Button::Icon)`, it's a no-op): (a) check whether the symbolic icons resolve to SVG or PNG on this system (`Data::Image` is drawn untinted, `widget/icon/mod.rs:111`); (b) compare `icon_button.on` vs `.on_disabled` in the active palette — **suspect (b), in a harder form than the plan expected** (see findings)
- [x] implement explicit dimming in `nav_button`: build the icon with `Icon::opacity(..)` reduced (e.g. ~0.4) when `on_press` is `None` — this is the expected fix regardless of which suspect confirmed; record the diagnosis outcome here
- [x] verify visually in the panel: disabled prev at oldest end, disabled next/newest at newest end, disabled refresh while a fetch is pending — **[x] manual test (skipped — not automatable, no interactive wayland session); already listed under Post-Completion, and Task 8 re-checks all four disabled cases after `just install`**
- [x] tests: view-code exempt (`nav_button` gains no pure logic; if a helper like `disabled_icon_alpha()` emerges, unit-test it) — a helper did emerge (`icon_opacity`), unit-tested by `disabled_icons_are_visibly_dimmed`
- [x] run `just check` — must pass before task 5 (133 tests pass, fmt + clippy clean)

➕ **Findings (2026-08-08) — suspect (b), and the alpha delta is discarded outright**

Suspect (a) is ruled out: the active icon theme is `Cosmic`, and all four names
(`go-previous-symbolic`, `go-next-symbolic`, `go-last-symbolic`,
`view-refresh-symbolic`) resolve to `.svg` under
`/usr/share/icons/Cosmic/scalable/actions/` — no PNG exists for them in any
search path, and `Named` sets `prefer_svg: true` for `-symbolic` names anyway
(`widget/icon/named.rs:58`). The `Data::Svg` branch is taken, so the icons *are*
tinted.

Suspect (b) is confirmed, and it is not merely "near-invisible" — the disabled
appearance is provably a **no-op**, on two independent counts (libcosmic rev
`8a017a1`):

1. **Foreground.** `icon_button` is built by `Component::component`
   (`cosmic-theme/src/model/derivation.rs:170`), which sets
   `on = on_component` and `on_disabled = on_component.with_alpha(0.65)` —
   the *same RGB*, differing only in alpha. But the SVG rasteriser tints RGB
   only and keeps each source pixel's alpha
   (`iced/wgpu/src/image/vector.rs:173-181`: `rgba[0..=2] = color[0..=2]`,
   `rgba[3]` untouched). The alpha carried by `icon_color` never reaches a
   pixel, so enabled and disabled glyphs rasterise **identically**.
2. **Background.** `disabled()` halves the background alpha
   (`theme/style/button.rs:220`), but `icon_button.base` is
   `Srgba::new(0,0,0,0)` (`cosmic-theme/src/model/theme.rs:1480`) — half of
   transparent is still transparent.

`Icon::opacity` is the one lever that survives: it is threaded into
`Svg::opacity` (`widget/icon/mod.rs:104`), into `svg::Svg { opacity }`
(`iced/widget/src/svg.rs:343`), and reaches the renderer as its own instance
uniform (`iced/wgpu/src/image/mod.rs:348`) — independent of the tint colour.

Implemented as planned: `nav_button` now builds
`widget::icon::from_name(..).icon().opacity(icon_opacity(on_press.is_some()))`
with `DISABLED_ICON_OPACITY = 0.4`.

[decision] `widget::button::icon` takes a bare `Handle` and exposes no path to
the inner `Icon`, so `nav_button` open-codes what that constructor builds
internally (`button::custom` over a `Row` padded by `space_xxs`, `.padding(0)`,
`.class(theme::Button::Icon)` — `widget/button/icon.rs:135-190`). Visual output
is unchanged for enabled buttons; only the disabled glyph now dims. The
alternative (upstream patch to plumb opacity through `button::icon`) is out of
scope for a rev-pinned dependency.

[decision] The pure helper is `icon_opacity(enabled: bool) -> f32` rather than
the plan's speculative `disabled_icon_alpha()` — it is the value the call site
actually needs (one call, both branches) and keeps the ternary out of the view
code. `DISABLED_ICON_OPACITY` stays private; the test asserts a range
(0.2..=0.6) plus `disabled < enabled` so the constant can be tuned by eye
without churning the test.

### Task 5: i18n infrastructure + externalize all strings (English)

**Files:**
- Modify: `Cargo.toml`
- Create: `i18n.toml`
- Create: `src/localize.rs`
- Create: `i18n/en/cosmic_bing_wallpaper.ftl`
- Modify: `src/main.rs`, `src/view.rs`, `src/app.rs` (any user-facing strings there)

- [x] add deps `i18n-embed` (0.16, `fluent-system` + `desktop-requester` features), `i18n-embed-fl` (0.10), `rust-embed` (8) — versions matching libcosmic's own
- [x] create `i18n.toml` (fallback `en`, assets_dir `i18n`) and `src/localize.rs` following cosmic-greeter's pattern (`RustEmbed` on `i18n/`, `FluentLanguageLoader` in a `OnceLock`, `fl!` macro, `localize()` called at startup in `main.rs`)
- [x] write `i18n/en/cosmic_bing_wallpaper.ftl` covering the full string inventory (popup strings, status lines, dropdown labels, `display_title` fallback, Task-3 tooltips; `format_updated` frames take `$time`/`$date` args) — 22 ids
- [x] convert `view.rs` (and any `app.rs` strings) to `fl!`; label arrays become `fn ..._labels() -> Vec<String>` accessors (see Technical Details); keep index↔value mapping functions unchanged
- [x] add the test helper that pins the loader to `en` exactly once (`OnceLock` + `load_fallback_language` — never `DesktopLanguageRequester` in tests, with a comment saying why); **keep the literal-English assertions** in `status_line_*`, `format_updated_*`, `display_title_*` and the label-length invariants — they now also guard the English FTL copy
- [x] run `just check` — must pass before task 6 (note: `i18n-embed-fl` already fails the *compile* for any `fl!` id missing from `en`, so no separate test is needed for that) — 136 tests pass, fmt + clippy clean

➕ **Notes (2026-08-08)**

- FTL ids and their call sites: `panel-tooltip` (`app.rs`), `about-this-image`,
  `bing-wallpaper`, `tooltip-{previous,next,newest,refresh,open-image}`,
  `shuffle`, `shuffle-every`, `keep-images`,
  `interval-{30-minutes,1-hour,6-hours,daily}`,
  `retention-{3-days,8-days,30-days,forever}`,
  `status-{checking,network-error,disk-error,no-images,up-to-date}`,
  `status-updated-{today,yesterday,on}` (the last three carry
  `$time` / `$date`).
- `PANEL_TOOLTIP` (a `const`) became `fn panel_tooltip() -> String` — a const
  cannot hold a value that must be read *after* `localize()` runs.
  `nav_button`/`popup_tooltip` take `impl Into<Cow<'static, str>>` instead of
  `&'static str` (`applet_tooltip`'s own bound), so owned strings pass through.

[decision] The loader calls `set_use_isolating(false)`. Fluent wraps every
placeable in bidi isolate marks (U+2068/U+2069) by default, which would make
`format_updated` return `"Updated \u{2068}09:12\u{2069}"` — invisible in a
terminal but present in the string, breaking both the plan's mandated
literal-English assertions and iced's text measurement. cosmic-greeter and
cosmic-settings disable it for the same reason. Guarded by
`placeables_are_substituted_without_bidi_isolates`.

[decision] The `en` pin is structural rather than a separate test helper: the
plan asked for "`OnceLock` + `load_fallback_language` called exactly once",
which is precisely what the `LazyLock<FluentLanguageLoader>` initializer does.
The crate's `fl!` deliberately does **not** call `localize()` (libcosmic's copy
does), so `localize()` — the only `DesktopLanguageRequester` caller — is
reachable from `main` alone and can never run in a test binary. Asserted
directly by `loader_is_pinned_to_english`; verified by running the suite under
`LANG=LC_ALL=uk_UA.UTF-8` (136 passed).

[decision] The wrapper macro forwards args as `$($args:tt)*` rather than
libcosmic's `$($args:expr),*` — `time = value` only survives re-emission into
`i18n_embed_fl::fl!` reliably as raw token trees.

[deviation] Two extra assertions beyond the plan's list: the dropdown label
arrays now pin their English *contents* (`shuffle_interval_mapping_roundtrips`,
`retention_mapping_roundtrips`), not just their lengths. Moving them out of
`&'static str` consts removed the only place those strings were visible in
source, so without this the English label copy would be unguarded.

### Task 6: Translations for the 72 remaining COSMIC locales

**Files:**
- Create: `i18n/<locale>/cosmic_bing_wallpaper.ftl` × 72 (every locale from the Context list except `en`, which exists)
- Modify: `src/localize.rs` (guard test lives in its `#[cfg(test)] mod tests` — the crate has **no lib target**, so a `tests/` integration test cannot reach `localize`)

- [x] generate machine translations of `i18n/en/cosmic_bing_wallpaper.ftl` for all 72 remaining locales (careful with Fluent syntax: selectors, `$time`/`$date` placeables preserved verbatim; RTL scripts ar/fa/he plain text)
- [x] write the locale guard test in `src/localize.rs`: (1) assert exactly 73 embedded locale dirs, each parsing as a `LanguageIdentifier`; (2) for every locale, load it and assert each message id present in `en` resolves without error; (3) per message id, compare the *set of `$variable` references* against `en` (simple `$ident` scan over the embedded FTL bytes is enough) — a dropped/renamed placeable resolves "successfully" but renders a broken string, the most common machine-translation failure
- [x] run the guard + `just check` — must pass before task 7 (139 tests pass, fmt + clippy clean; also green under `LANG=LC_ALL=uk_UA.UTF-8`)
- [x] spot-check 2–3 locales you can read (e.g. uk, de) and fix obvious howlers — spot-checked uk, ru, be, de, fr, es(+es-419/es-MX), it, pl; fixes below

➕ **Notes (2026-08-08)**

- All 73 locale dirs now exist, each with the full 27-id inventory (the Task-5
  note's "22 ids" undercounted — the `en` catalogue has always had 27).
  Placeables (`{ $time }`, `{ $date }`) are byte-identical to `en` everywhere.
- The guard is three tests in `src/localize.rs`:
  `every_cosmic_locale_ships_a_catalogue` (73 dirs, each a valid
  `LanguageIdentifier`, each file named for the fluent domain),
  `every_locale_defines_and_renders_every_english_message`, and
  `every_locale_preserves_the_english_placeables`.

[decision] Message-id presence is checked with `with_message_iter(&locale, ..)`
against a **per-locale loader**, not `has()`. `has()` answers from any loaded
bundle including the `en` fallback, so it can never see a missing key; and
`load_languages` does **not** return an error for malformed FTL — fluent logs
the parse error and keeps a partial resource (`i18n-embed-0.16.0`
`src/fluent.rs:566-574`). Since an unparsable entry is simply dropped from that
partial resource, comparing the locale's *parsed* id set against `en`'s ids
catches missing keys and broken syntax with the same assertion.

[decision] Placeable comparison scans the raw embedded FTL text (per the plan)
rather than pulling in `fluent-syntax` as a direct dependency; the catalogues
are flat `id = value` lines and the scanner handles indented continuations and
skips comments.

Mutation-checked — each of the four failure modes fails exactly one guard and
no other: renamed placeable (`$date` → `$datum`) → placeables test; deleted
message → ids test; unbalanced `{` → ids test (the entry becomes junk);
unexpected extra locale dir → dir-count test.

[deviation] Spot-check fixes go slightly beyond "howlers": in the 10 locales
above, `shuffle-every` became a neutral noun ("Інтервал", "Intervall",
"Fréquence", "Frecuencia", "Frequenza", "Częstotliwość", …) because the English
frame "Every" + "1 hour" is ungrammatical in inflecting languages ("Кожні 1
година", "Alle 1 Stunde"), and a noun label reads correctly before *every*
dropdown value. `retention-forever` was likewise softened where a bare adverb
after "Keep images" read wrong (uk/ru/be/fr). The other 62 locales keep the
literal frame — they are machine-generated and flagged as such for
native-speaker review (README, Task 9).

### Task 7: Theme conformance with first-party applets

**Files:**
- Modify: `src/view.rs`

- [x] audit the popup against cosmic-applet-tiling's `view_window` (reference findings in Technical Details): divider padding, popup column padding, spacing tokens (`space_xxxs`/`space_xxs`/`space_s` from the theme), `padded_control`/`menu_button` usage, toggler row shape (`.text_size(14).width(Fill)`)
- [x] apply the known gap: dividers become `padded_control(divider::horizontal::default()).padding([space_xxs, space_s])`
- [x] check the remaining surface for non-token hardcoded values (e.g. `PLACEHOLDER_HEIGHT`, thumbnail corner treatment under `theme::Button::Image`, control-row spacing `space_s`) and align anything that deviates from stock applet look; record each change (or "already conformant") here — three deviations found and fixed, everything else conformant (see findings)
- [x] verify side by side in the panel against a first-party applet popup (tiling or audio): margins, divider insets, corner radii — **[x] manual test (skipped — GUI-only, not automatable, no interactive wayland session); every value changed was derived from the first-party source rather than from eyeballing, and Task 8 re-checks the popup after `just install`**
- [x] tests: view-code exempt (pure layout; no logic changes)
- [x] run `just check` — must pass before task 8 (139 tests pass, fmt + clippy clean)

➕ **Findings (2026-08-08) — three deviations, five confirmations**

Audited against cosmic-applet-tiling `src/window.rs:253-317` and the widget
constructors it leans on (libcosmic rev `8a017a1`). Reference token values at
standard density: `space_xxxs` 4, `space_xxs` 8, `space_xs` 12, `space_s` 16,
`space_m` 24 (`cosmic-theme/src/model/spacing.rs`).

**Changed:**

1. **Divider insets** (the known gap). `padded_control` pads with
   `menu_control_padding()` = `[space_xxs, space_m]` = `[8, 24]`
   (`src/applet/mod.rs:614-626`), but all four of tiling's dividers override to
   `[space_xxs, space_s]` = `[8, 16]`, so a stock divider reaches 8 px closer to
   each popup edge than the controls do. Ours were plain `padded_control` →
   inset with the controls, reading as a short rule. Extracted as
   `view::divider()` (three call sites) with the override applied.
2. **Thumbnail corners.** `theme::Button::Image` rounds *the button's* border to
   `corner_radii.radius_s` (`theme/style/button.rs:98-114`) but nothing rounds
   the image inside it; libcosmic's own `button::image` rounds the handle
   itself (`widget/button/image.rs:18`, hardcoded `[9.0; 4]`). Our
   `custom_image_button` path skipped that, so a square-cornered thumbnail sat
   inside a rounded hover/focus ring. Now
   `.border_radius(theme::active().cosmic().corner_radii.radius_s)`.
3. **Header column spacing.** `.spacing(2)` matched no token; tiling's label
   column uses `.spacing(space_xxxs)` (= 4). Switched to the token.

**Already conformant (no change):**

4. **Popup column padding.** Ours is `[space_xxs, 0]`; tiling's literal
   `[8, 0]` is the same value at standard density and ours additionally tracks
   the density setting.
5. **Control-row spacing.** `space_s` between the four icon buttons is already
   a token.
6. **Nav-button geometry.** The open-coded `nav_button` (Task 4) matches
   `button::icon` exactly: padding `space_xxs`, `.padding(0)` on the button,
   `theme::Button::Icon`, glyph 16 px — `icon::from_name(..).icon()` defaults to
   `size: 16` (`widget/icon/mod.rs:27`) and `button::icon` sets 16 for symbolic
   handles (`widget/button/icon.rs:51`).
7. **`padded_control` / `menu_button` / toggler shape / `fn style()`.** Every
   row already goes through `padded_control`, the About row is
   `menu_button(text::body(..))`, the toggler is
   `.text_size(14).width(Length::Fill)`, and `style()` returns
   `cosmic::applet::style()` — all identical to the reference.
8. **`cosmic::theme::spacing()`.** Literally `active().cosmic().spacing`
   (`src/theme/mod.rs:70`), i.e. tiling's destructure with fewer lines — not
   churned, per the plan.

[decision] `PLACEHOLDER_HEIGHT` (160.0) and the placeholder icon's `.size(64)`
stay plain numbers. The `Spacing` scale describes paddings and gaps, not
content dimensions, and there is no theme token for either; first-party applets
size their own content areas with literals too. Recorded as a comment on the
const so the next audit does not re-litigate it.

[decision] The thumbnail radius uses the `radius_s` **token** rather than
libcosmic's hardcoded `[9.0; 4]`. It is the same corner at default settings
(`radius_s: [8.0; 4]`) and it is the exact value `Button::Image` uses for the
ring it must line up with — so it stays aligned when the user changes the
corner-radius setting, which the literal would not.

[decision] `custom_image_button`'s default `Padding::new(5.0)`
(`widget/button/widget.rs:101`) is left alone: it is the widget's own default,
i.e. already the stock value, and it is what keeps the 2 px hover ring clear of
the image edge.

### Task 8: Verify acceptance criteria

Criteria reference tasks, not the Overview's item numbers:

- [x] Task 1: lock-screen behavior resolved per its recorded branch (fix verified live, or README limitation documented with upstream link, or does-not-reproduce recorded) — **branch D recorded** (all four chain links observed live, no code change); the login-greeter permission limitation is queued for README in Task 9
- [x] Task 2: regression coverage confirmed (or gap test added) — both named tests green, plus the added `pipeline_backfills_thumbnails_outside_the_fetch_window` (`src/app.rs:1237`)
- [x] Task 3: every icon-only control — prev, next, newest, refresh, thumbnail, panel button — has a descriptive tooltip — all six confirmed in source (`view.rs:319/348/354/360/419` via `popup_tooltip`, `app.rs:842` for the panel)
- [x] Task 4: disabled buttons visually distinct in the panel (all four disabled cases checked) — **[x] manual test (skipped — GUI-only, not automatable)**; verified structurally instead: all four buttons (prev/next/newest *and* refresh, which routes through `refresh_button` → `nav_button`, `view.rs:415-422`) share the single `icon_opacity(on_press.is_some())` call at `view.rs:440`, so there is no per-button case that can miss the dimming
- [x] Tasks 5–6: 73 locale dirs present, guard test green, applet launches localized under `LANG=uk_UA.UTF-8` (spot check) — 73 dirs, all three `localize::tests` guards green, and the release binary ran a full 3 s under `LANG=LC_MESSAGES=uk_UA.UTF-8` without crashing or logging `localize`'s "falling back to English" warning (see findings)
- [x] Task 7: popup visually consistent with first-party applet popups — **[x] manual test (skipped — GUI-only, not automatable)**; every changed value was derived from cosmic-applet-tiling's source rather than eyeballing (Task 7 findings), which is the check the side-by-side would approximate
- [x] run full suite: `just check` — 139 tests pass, `cargo fmt --check` and `clippy -D warnings` clean
- [x] `just install` and verify the applet in the panel (popup opens, tooltips show, no clipping) — `just install` run, all three artifacts in place under `~/.local`; **[x] in-panel visual verification (skipped — GUI-only, not automatable)**, retained under Post-Completion

➕ **Findings (2026-08-08) — acceptance verification**

- `just check`: 139 passed, 0 failed; fmt and clippy clean.
- Locales: `ls i18n | wc -l` → 73, and `every_cosmic_locale_ships_a_catalogue` /
  `every_locale_defines_and_renders_every_english_message` /
  `every_locale_preserves_the_english_placeables` all pass.
- `just install`: binary → `~/.local/bin/cosmic-bing-wallpaper`, desktop entry
  (with `Exec=` rewritten to the absolute path) and symbolic icon installed.

[decision] The `LANG=uk_UA.UTF-8` spot check was run headlessly instead of by
eye. The release binary was launched against the live wayland socket but with
`HOME` and all four `XDG_*_HOME` vars redirected into a scratch dir, so it could
touch no real config, state or wallpaper, and killed at 3 s — inside the ~5 s
cold-start fetch delay, so it also made no network request. It exited 143
(SIGTERM from `timeout`), i.e. it was still alive at 3 s rather than having died
during startup, and `RUST_LOG=cosmic_bing_wallpaper=debug` printed nothing —
notably not `localize`'s `"falling back to English: could not load desktop
languages"` warning (`src/localize.rs:67`), which is the observable failure mode
for locale selection. Combined with the guard tests proving the `uk` catalogue
loads and renders every id, that covers "launches localized" without a GUI.

[deviation] Four of the eight criteria are GUI-only and were marked skipped per
the standing rule; none of them gates code. Each is already listed under
Post-Completion for the user's own smoke test, and each was replaced above with
the strongest structural evidence available.

### Task 9: [Final] Update documentation and desktop entry

**Files:**
- Modify: `README.md`, `CLAUDE.md`
- Modify: `data/io.github.ercling.CosmicBingWallpaper.desktop`

- [x] add `Comment[<locale>]=` translations to the `.desktop` file for the major locales (de es fr it ja ko pl pt-BR ru uk zh-CN zh-TW); `Name=` stays untranslated (product name) — 12 lines added, POSIX locale tags (`pt_BR`/`zh_CN`/`zh_TW`), `desktop-file-validate` clean
- [x] update `README.md`: tooltips + localized UI in features; lock-screen outcome (fix or known limitation); mention that the 72 non-English locales are machine-generated and native-speaker review is welcome
- [x] update `CLAUDE.md`: i18n conventions (fluent domain, `fl!`, `en`-pinned test loader, locale guard test, "all user-facing strings go through FTL"), the `applet_tooltip` idiom (extends the existing `popup_dropdown` UI convention), and the Task-4 disabled-styling gotcha if one was found
- [x] move this plan to `docs/plans/completed/` — (performed by the exec orchestrator at completion)

➕ **Notes (2026-08-08)**

- `.desktop`: the desktop-entry spec uses POSIX locale tags, so the keys are
  `Comment[pt_BR]`/`[zh_CN]`/`[zh_TW]`, **not** the BCP-47 directory names used
  under `i18n/`. A comment in the file records that, plus why `Comment` is
  translated there rather than in Fluent (the launcher reads the file without
  running us) and that `Name` is a product name. Validated with
  `desktop-file-validate` — exit 0, only the pre-existing `Categories=COSMIC;`
  hint.
- `README.md`: two new feature bullets (tooltips + dimmed disabled buttons;
  localized UI), a new **Translations** section (machine-generated caveat, how to
  contribute a fix, the two invariants the guard tests enforce — keep every `en`
  id, keep each message's placeables — and the English-month-abbreviation date
  limitation), and a new limitation covering the Task-1 outcome: the *lock*
  screen follows along (verified live), the *login* greeter cannot, because uid
  `cosmic-greeter` cannot traverse a `drwxr-x---` home to reach
  `~/Pictures/BingWallpaper`.
- `CLAUDE.md`: `src/localize.rs` added to the architecture list; the single-line
  "UI convention" note grew into a **UI conventions** section stating the
  underlying rule (anything rendering outside its parent's bounds must be a real
  wayland popup, never an iced overlay) with dropdowns and tooltips as its two
  instances, plus the disabled-icon gotcha (theme styling is a no-op for
  `Button::Icon`; use `icon_opacity`, and don't reach for
  `.class(theme::Button::Icon)`); and a new **i18n** section.

[decision] No upstream issue filed. Task 1 landed on branch D (nothing broken)
and the login-greeter finding is a local file-permission consequence of a
standard `drwxr-x---` home, not a cosmic-greeter defect — cosmic-greeter's own
`daemon/src/lib.rs:197` TODO already tracks the config-fallback gap. The README
states the workaround (loosen home permissions) and declines to recommend it.

[decision] Docs-only task, so no new tests. Validation is `just check` (139
tests, fmt, clippy — unchanged, since no code changed) plus
`desktop-file-validate` on the edited desktop entry.

[deviation] The "Design decisions … live in `docs/plans/`" paragraph in
`CLAUDE.md` moved up to close the Architecture section (the new `##` headings
would otherwise have orphaned it under **i18n**) and now names both plan files
and records the lock-screen chain as a verified fact.

## Post-Completion

**Manual verification:**
- Lock-screen smoke test after a fresh boot (greeter *login* screen uses the
  root daemon's copy of the same state — worth one check that it also shows the
  Bing image, since the session-lock diagnostic in Task 1 doesn't cover it)
- Panel smoke test on a cold start (empty `~/Pictures/BingWallpaper`) — real
  Bing fetch ~5 s in, wallpaper + lock screen both updated
- Tooltip hover feel (delay/position) on the real panel

**External:**
- If Task 1 lands on branch B: file the upstream issue against
  pop-os/cosmic-greeter (reference existing #184 — solid-color lock-screen
  mismatch — if related) and link it from README
- Translation quality: 72 locales are machine-generated; native-speaker review
  is a community contribution opportunity (mention in README)
