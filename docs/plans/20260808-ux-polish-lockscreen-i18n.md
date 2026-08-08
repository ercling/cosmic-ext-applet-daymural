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

- [ ] wrap the nav/refresh buttons in `core.applet.applet_tooltip(...)` (NOT the plain `widget::tooltip` overlay — it clips to the popup surface; see Context): "Previous wallpaper", "Next wallpaper", "Skip to newest", "Check for new images now"; inside the popup pass `has_popup: false` and `parent_id: window.popup`, with `Message::Surface` as the forwarder
- [ ] add a tooltip to the thumbnail button ("Open image in viewer"), same idiom
- [ ] add a tooltip to the panel button in `app.rs` (`view()`, `icon_button(PANEL_ICON)`) — "Bing Wallpaper of the Day" — with `has_popup: self.popup.is_some()` (suppressed while the popup is open) and `parent_id: None`, per libcosmic's `examples/applet/src/window.rs:149`
- [ ] verify hover behavior in the panel: tooltip appears (~100 ms delay), not clipped, one at a time; note outcome here
- [ ] tests: view-code exempt (no new pure logic introduced; tooltip strings enter the tested inventory in Task 5)
- [ ] run `just check` — must pass before task 4

### Task 4: Visibly dimmed disabled icon buttons

**Files:**
- Modify: `src/view.rs`

- [ ] reproduce: build, open popup at the oldest image (prev disabled) and confirm disabled/enabled render identically
- [ ] diagnose which suspect it is (see Context — the theme's disabled path is already fully wired, so do NOT reach for `.class(theme::Button::Icon)`, it's a no-op): (a) check whether the symbolic icons resolve to SVG or PNG on this system (`Data::Image` is drawn untinted, `widget/icon/mod.rs:111`); (b) compare `icon_button.on` vs `.on_disabled` in the active palette
- [ ] implement explicit dimming in `nav_button`: build the icon with `Icon::opacity(..)` reduced (e.g. ~0.4) when `on_press` is `None` — this is the expected fix regardless of which suspect confirmed; record the diagnosis outcome here
- [ ] verify visually in the panel: disabled prev at oldest end, disabled next/newest at newest end, disabled refresh while a fetch is pending
- [ ] tests: view-code exempt (`nav_button` gains no pure logic; if a helper like `disabled_icon_alpha()` emerges, unit-test it)
- [ ] run `just check` — must pass before task 5

### Task 5: i18n infrastructure + externalize all strings (English)

**Files:**
- Modify: `Cargo.toml`
- Create: `i18n.toml`
- Create: `src/localize.rs`
- Create: `i18n/en/cosmic_bing_wallpaper.ftl`
- Modify: `src/main.rs`, `src/view.rs`, `src/app.rs` (any user-facing strings there)

- [ ] add deps `i18n-embed` (0.16, `fluent-system` + `desktop-requester` features), `i18n-embed-fl` (0.10), `rust-embed` (8) — versions matching libcosmic's own
- [ ] create `i18n.toml` (fallback `en`, assets_dir `i18n`) and `src/localize.rs` following cosmic-greeter's pattern (`RustEmbed` on `i18n/`, `FluentLanguageLoader` in a `OnceLock`, `fl!` macro, `localize()` called at startup in `main.rs`)
- [ ] write `i18n/en/cosmic_bing_wallpaper.ftl` covering the full string inventory (popup strings, status lines, dropdown labels, `display_title` fallback, Task-3 tooltips; `format_updated` frames take `$time`/`$date` args)
- [ ] convert `view.rs` (and any `app.rs` strings) to `fl!`; label arrays become `fn ..._labels() -> Vec<String>` accessors (see Technical Details); keep index↔value mapping functions unchanged
- [ ] add the test helper that pins the loader to `en` exactly once (`OnceLock` + `load_fallback_language` — never `DesktopLanguageRequester` in tests, with a comment saying why); **keep the literal-English assertions** in `status_line_*`, `format_updated_*`, `display_title_*` and the label-length invariants — they now also guard the English FTL copy
- [ ] run `just check` — must pass before task 6 (note: `i18n-embed-fl` already fails the *compile* for any `fl!` id missing from `en`, so no separate test is needed for that)

### Task 6: Translations for the 72 remaining COSMIC locales

**Files:**
- Create: `i18n/<locale>/cosmic_bing_wallpaper.ftl` × 72 (every locale from the Context list except `en`, which exists)
- Modify: `src/localize.rs` (guard test lives in its `#[cfg(test)] mod tests` — the crate has **no lib target**, so a `tests/` integration test cannot reach `localize`)

- [ ] generate machine translations of `i18n/en/cosmic_bing_wallpaper.ftl` for all 72 remaining locales (careful with Fluent syntax: selectors, `$time`/`$date` placeables preserved verbatim; RTL scripts ar/fa/he plain text)
- [ ] write the locale guard test in `src/localize.rs`: (1) assert exactly 73 embedded locale dirs, each parsing as a `LanguageIdentifier`; (2) for every locale, load it and assert each message id present in `en` resolves without error; (3) per message id, compare the *set of `$variable` references* against `en` (simple `$ident` scan over the embedded FTL bytes is enough) — a dropped/renamed placeable resolves "successfully" but renders a broken string, the most common machine-translation failure
- [ ] run the guard + `just check` — must pass before task 7
- [ ] spot-check 2–3 locales you can read (e.g. uk, de) and fix obvious howlers

### Task 7: Theme conformance with first-party applets

**Files:**
- Modify: `src/view.rs`

- [ ] audit the popup against cosmic-applet-tiling's `view_window` (reference findings in Technical Details): divider padding, popup column padding, spacing tokens (`space_xxxs`/`space_xxs`/`space_s` from the theme), `padded_control`/`menu_button` usage, toggler row shape (`.text_size(14).width(Fill)`)
- [ ] apply the known gap: dividers become `padded_control(divider::horizontal::default()).padding([space_xxs, space_s])`
- [ ] check the remaining surface for non-token hardcoded values (e.g. `PLACEHOLDER_HEIGHT`, thumbnail corner treatment under `theme::Button::Image`, control-row spacing `space_s`) and align anything that deviates from stock applet look; record each change (or "already conformant") here
- [ ] verify side by side in the panel against a first-party applet popup (tiling or audio): margins, divider insets, corner radii
- [ ] tests: view-code exempt (pure layout; no logic changes)
- [ ] run `just check` — must pass before task 8

### Task 8: Verify acceptance criteria

Criteria reference tasks, not the Overview's item numbers:

- [ ] Task 1: lock-screen behavior resolved per its recorded branch (fix verified live, or README limitation documented with upstream link, or does-not-reproduce recorded)
- [ ] Task 2: regression coverage confirmed (or gap test added)
- [ ] Task 3: every icon-only control — prev, next, newest, refresh, thumbnail, panel button — has a descriptive tooltip
- [ ] Task 4: disabled buttons visually distinct in the panel (all four disabled cases checked)
- [ ] Tasks 5–6: 73 locale dirs present, guard test green, applet launches localized under `LANG=uk_UA.UTF-8` (spot check)
- [ ] Task 7: popup visually consistent with first-party applet popups
- [ ] run full suite: `just check`
- [ ] `just install` and verify the applet in the panel (popup opens, tooltips show, no clipping)

### Task 9: [Final] Update documentation and desktop entry

**Files:**
- Modify: `README.md`, `CLAUDE.md`
- Modify: `data/io.github.ercling.CosmicBingWallpaper.desktop`

- [ ] add `Comment[<locale>]=` translations to the `.desktop` file for the major locales (de es fr it ja ko pl pt-BR ru uk zh-CN zh-TW); `Name=` stays untranslated (product name)
- [ ] update `README.md`: tooltips + localized UI in features; lock-screen outcome (fix or known limitation); mention that the 72 non-English locales are machine-generated and native-speaker review is welcome
- [ ] update `CLAUDE.md`: i18n conventions (fluent domain, `fl!`, `en`-pinned test loader, locale guard test, "all user-facing strings go through FTL"), the `applet_tooltip` idiom (extends the existing `popup_dropdown` UI convention), and the Task-4 disabled-styling gotcha if one was found
- [ ] move this plan to `docs/plans/completed/`

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
