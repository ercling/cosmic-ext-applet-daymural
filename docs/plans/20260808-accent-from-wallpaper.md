# Accent Colour from Wallpaper (opt-in)

## Overview

Add an **opt-in, off-by-default** feature that derives the COSMIC accent colour
from the currently applied Bing wallpaper and writes it into the system theme,
restoring the user's previous accent exactly when switched off.

- Problem: COSMIC has no accent-from-wallpaper yet (planned upstream:
  [cosmic-settings#343](https://github.com/pop-os/cosmic-settings/issues/343));
  this applet already knows every wallpaper change, so it is the natural stopgap.
- The colour must stay legible: cosmic-theme protects accent *text* but not
  accent *fills* — a mid-luminance free colour is the documented failure case
  (see `docs/plans/20260808-accent-from-wallpaper-notes.md` §4).
- Must be cheap to retire once COSMIC ships it natively: off by default,
  reversible (snapshot/restore), self-contained in one new module, and writing
  **only the `accent` key** of the user's builder config (never pinning other
  keys user-locally).

### Chosen algorithm: **hue transplant** (decided 2026-08-08)

Extract the dominant *vibrant* hue from the cached 480×270 thumbnail, then
transplant that hue onto a per-mode tone taken from COSMIC's own palette
accents, gamut-map by chroma reduction, and verify with a real WCAG ratio:

```
thumbnail pixels ──► chroma-weighted hue histogram (Oklch) ──► dominant hue
                                                                  │  (None if the image is
                                                                  ▼   effectively grey)
light builder: hue + mean (L, C) of its palette's 8 chromatic accents ─► gamut map ─► WCAG guard ─► Srgb
dark  builder: hue + mean (L, C) of its palette's 8 chromatic accents ─► gamut map ─► WCAG guard ─► Srgb
                                                                  │
                                                                  ▼
                    set_accent on both builders + full write of both derived themes
```

Why this beat the alternatives (researched against
`examples/GNOME-Auto-Accent-Colour` and the Rust ecosystem):

| Approach | Verdict |
| --- | --- |
| **Hue transplant** (chosen) | Exact hue match to the wallpaper; mid-luminance trap avoided *by construction* (tone comes from COSMIC's own tuned accents); per-mode variants free; pure functions, no new deps. Cost: wallpaper's own saturation/lightness is discarded. |
| GNOME-style snap to the 9 `cosmic_palette` accents (`accent_blue`…`accent_warm_grey`) | Safest, but only 9 possible accents — a distinctive teal sunset lands on generic blue. The GNOME extension only snaps because GNOME *names* presets; COSMIC takes an arbitrary `Srgb`, so the coarseness buys nothing. |
| Free colour + clamp/verify | Truest match but the most hand-tuned constants (band edges, chroma cap, per-mode delta); edge-case whack-a-mole the palette-derived tone band avoids. |
| `material-colors` crate (Material You HCT/Celebi) | cosmic-theme derives its whole ramp from a *single seed Srgb*, so the tonal machinery collapses to picking one colour — a large dependency in a rev-pinned minimal-dep repo for ~nothing. |

What we keep from the GNOME extension's algorithm (`extension.js:121-154`):
the *dominant-with-grey-fallback* shape — its `saturation < 5% → slate` rule
becomes our `mean chroma below threshold → accent_warm_grey`; its palette
cache keyed by image identity is already covered by our thumbnail cache. What
we drop: full-image decode (we use the 480×270 thumbnail), the subprocess
(we're native), and preset snapping (see table).

## Context (from discovery)

- Notes file with the legibility analysis, reversibility rules and testability
  notes: `docs/plans/20260808-accent-from-wallpaper-notes.md`. ⚠️ Its §2 write
  recipe is superseded by "Theme write" below (plan review 2026-08-08 found the
  whole-entry `write_entry` on the builder unsafe — palette flip + key pinning).
- `src/app.rs:563` `on_apply_success` — the choke point for the three
  *runtime* apply paths: refresh auto-apply (`src/app.rs:516`), manual apply
  (`:1077`), shuffle (`:1119`). It returns `()` today; Task 6 changes its
  signature. **Startup does not pass through it** — `init` sets `current`
  directly from `wallpaper::current_wallpaper()`, so startup reconciliation is
  its own hook.
- `src/thumbs.rs` — `thumbnail_path`/`is_cached`/`decode_failed` provide the
  480×270 source image and its verdicts. `ensure_thumbnail` retries a `Failed`
  slot with a full ~5 MB decode every call (`src/thumbs.rs:82-84` short-circuits
  only `Cached`) — the accent path must check `is_cached`/`decode_failed` first
  and never call `ensure_thumbnail` unconditionally (CLAUDE.md: weakening the
  `decode_failed` guard re-opens an unbounded-retry bug).
- `src/config.rs:23` — `AppletConfig` derives `Eq` and `Window::set_config`
  compares with `==` (`src/app.rs:229`): new colour fields must stay `Eq`,
  hence `[u8; 3]`, never f32 tuples.
- `src/view.rs:372` `shuffle_toggler` — the toggler-row idiom to copy for the
  new setting row.
- WCAG + Oklch maths: the `palette` crate (re-exported as
  `cosmic::cosmic_theme::palette`) ships `Wcag21RelativeContrast` and
  `Oklcha`/`IntoColor` (cosmic-theme itself uses them, `model/theme.rs:10-12`).
  Do **not** move the `src/tooltip.rs` test helpers.
- `cosmic-theme` pinned rev `8a017a1` facts (all re-verified):
  - `ThemeBuilder.palette: CosmicPalette` (an enum — use `.as_ref()` for the
    inner), per-mode `accent_blue`…`accent_warm_grey` fields are `Srgba`
    (`model/cosmic_palette.rs:153-177`).
  - `ThemeBuilder::dark()/light()` (`model/theme.rs:955-966`);
    `Default for ThemeBuilder` uses **`DARK_PALETTE`** (`:922-925`).
  - `get_entry` starts from `Self::default()` and **silently skips**
    `NoConfigDirectory` errors (`cosmic-config-derive/src/lib.rs`), so a light
    builder read with an absent `palette` key returns the dark palette on the
    **Ok** path — the palette must be probed/forced explicitly (see Theme write).
  - `write_entry` writes **every** field in one transaction; the derive also
    generates single-key `set_<field>` setters. `set_accent` writes the bare
    `Option<Srgb>` **without** the `ColorReprOption` hex conversion — exact-f32
    RON, matching what is on this machine's disk today
    (`accent -> Some((red: 0.0, green: 0.32…))`).
  - `ThemeBuilder`/`Theme` are `#[version = 2]`: writes land in `…/v2/`; stale
    v1 `accent` keys on disk are harmless (reads prefer v2) — don't debug them.
- i18n: new message ids must land in **all 73 catalogues** (guard tests
  enforce exact-id parity); the 72 non-English ones are machine-translated
  (pattern: commit `dc38450`).

## Development Approach

- **testing approach**: Regular (code first, then tests **within the same task**)
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
  - tests are not optional - they are a required part of the checklist
  - unit tests for new and modified functions, success and error scenarios
- **CRITICAL: all tests must pass before starting next task** (`just check`)
- **CRITICAL: update this plan file when scope changes during implementation**
- tests never touch real user config/state — inject `Config::with_custom_path`
  handles and tempdir paths exactly like `src/config.rs`'s tests (notes §7).
  Caveat: `with_custom_path` sets `system_path: None`, so TempDir tests never
  see `/usr/share/cosmic` defaults — they exercise a *different* read path than
  production; a green TempDir run must not be read as "the system-default
  fallback works".
- maintain backward compatibility (existing `AppletConfig` entries must load)

## Testing Strategy

- **unit tests**: required for every task (see Development Approach above)
- **e2e tests**: none in this project (iced view code is exempt by convention);
  the untested remainder must shrink to the config write calls and the iced
  rows, mirroring how `wallpaper.rs` isolates its cosmic-bg context plumbing
- synthetic images built with `image::RgbImage::from_fn` directly — **not**
  `testutil::tiny_jpeg` (hardcoded gradient, and JPEG chroma subsampling shifts
  hue, a poor substrate for ±5° assertions)

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope

## Solution Overview

- New module `src/accent.rs` (a different config domain from `wallpaper.rs`,
  which stays cosmic-bg-only — notes §9.6): extraction, transplant, guard,
  theme writer, snapshot/restore, the pure apply/disarm decision, **and the
  snapshot/colour types that `config.rs` persists** (colour domain stays in one
  module; `config.rs` depends on `accent.rs`).
- **Recompute on every successful apply** (the three runtime paths through
  `on_apply_success`) **plus a startup reconciliation** in `init` when
  `accent_enabled` (the applet may have been down while the wallpaper or the
  accent changed; the next apply can be ~24 h out, or never, offline).
- **Both modes written from one hue** (notes §9.2 resolved): light builder gets
  hue + light-palette tone, dark builder gets hue + dark-palette tone; a
  light/dark mode flip then needs no work from us.
- **Off by default; reversible; snapshot lifecycle** (plan review #11 resolved):
  - **Enable** (toggle on): snapshot the *live* accents now — always, even if a
    stale snapshot exists (re-enable re-snapshots) — then compute and write.
  - **Disable** (toggle off): restore the snapshot verbatim (including `None` =
    palette default), then clear snapshot + last-written.
  - **Disarm** (external accent change detected): flip the toggle off via the
    config setter and clear snapshot + last-written **without restoring** — the
    user's manual choice stands.
- **Don't-clobber**: before each write, compare each builder's current accent
  against `accent_last_written` **in 8-bit space** (`[u8; 3]`); a mismatch
  means the user (or Settings) changed it → Disarm. Comparison is exact by
  construction: we quantise the computed colour to `u8`, convert back
  (`u8/255` f32), write that via `set_accent` (exact-f32 RON round-trip), and
  store the same `[u8; 3]` in config.
- **Failure behaviour** (notes §9.5): missing/failed thumbnail, decode error,
  or unwritable theme config — every failure path leaves the user's accent
  exactly as it was; log via `tracing`, no UI error state.

## Technical Details

### Extraction (pure, `src/accent.rs`)

- Input: `&image::RgbImage` (any size; production feeds the 480×270 thumbnail,
  tests feed tiny synthetic images).
- Per pixel (stride-sampled if larger than the thumbnail): convert Srgb →
  `Oklcha` via the re-exported `palette` crate; mask near-black/near-white
  pixels by L (like ColorThief's near-white filter, `color-thief.js:44-47`);
  accumulate unmasked pixels into a 36-bucket hue histogram **weighted by
  chroma**.
- Dominant hue = circular weighted mean over the peak bucket ±1 neighbour.
- Grey cutoff is **normalised**: `None` when the **mean chroma over unmasked
  pixels** is under threshold, or when *every* pixel was masked — an absolute
  total would make tiny test images and the 130k-pixel thumbnail disagree by
  construction.

### Transplant + gamut mapping + guard (pure)

- `tone_band(palette: &CosmicPaletteInner) -> (l, c)`: mean Oklch L and C of
  the 8 chromatic `accent_*` fields (`Srgba` → opaque `Srgb`) of that builder's
  own palette. Excludes `accent_warm_grey`; respects a user-customised palette.
  Why the mean and not just `accent_blue`: the L mean is near-trivial (light
  accents all sit at L≈0.40, dark at ≈0.80) but the *chroma* spread is real
  (≈0.07–0.16) — `accent_blue` alone would give a washed-out accent.
- `accent_for(palette, hue: Option<f32>) -> Srgb`: build `Oklcha { l, c, hue }`
  then **gamut-map by chroma reduction at fixed (L, h)** — binary search on C
  until inside sRGB. Naive per-channel clamping is *not* acceptable: at the
  real tone bands, roughly 140/360 (dark) and 168/360 (light) hues start
  outside sRGB and clipping shifts the hue. `None` hue → the palette's own
  `accent_warm_grey`.
- Guard: single check `max(contrast(accent, white), contrast(accent, black))
  >= 6.0` via `Wcag21RelativeContrast`; on failure fall back to
  `accent_warm_grey`. No iterative nudge loop — measured worst case over all
  360 hues on the stock palettes is ≥ 8.3, so a nudge would be unreachable
  dead code; the fallback branch is exercised by a test with a synthetic
  pathological palette instead.
  ⚠️ Threshold corrected 4.5 → 6.0 during Task 2: for *any* colour the better
  of white/black contrast has a hard floor of ≈ 4.58 (the two ratios are equal
  at relative luminance Y ≈ 0.179, both ≈ 0.229/0.05), so a 4.5 guard is
  mathematically unreachable — dead code, contradicting both the "branch must
  not be dead code" requirement below and notes §4's purpose (reject
  mid-luminance accents, which live exactly at that floor). 6.0 rejects the
  mid-luminance band Y ∈ (0.125, 0.25) and keeps ≥ 2.3 margin under the stock
  palettes' measured ≥ 8.3 (re-asserted in the sweep test).

### Theme write (`src/accent.rs`; supersedes notes §2)

- Read, per mode: `ThemeBuilder::get_entry(&builder_cfg)` accepting the
  partial on `Err`, **then probe the `palette` key directly**
  (`ConfigGet::get::<CosmicPalette>(cfg, "palette")`); if the probe fails,
  substitute the mode's own default (`ThemeBuilder::light()`/`dark()`
  palette). Never trust `get_entry`'s palette on the Ok path (dark default
  leaks in when the key is absent — verified in the derive) and never fall
  back to `ThemeBuilder::default()`.
- Write, per mode:
  1. `builder.set_accent(&builder_cfg, Some(srgb))` — the generated
     **single-key setter**: only the `accent` key changes, nothing else gets
     pinned user-locally (write_entry on the builder would materialise every
     key and cut the user off from future COSMIC default changes).
  2. `builder.build().write_entry(&theme_cfg)` — the derived `Theme` **is** a
     full-entry write; that matches upstream (cosmic-settings does the same)
     and both writes are required: nothing on the system rebuilds the theme
     from the builder.
- Restore: `set_accent(&builder_cfg, snapshot_value)` (including `None`),
  then rebuild + write the derived theme.
- All four `Config` handles (light/dark × builder/theme) injectable
  (`Config::with_custom_path`) so tests run in a `TempDir`.

### Config additions (`AppletConfig`, stays version 1 — new fields default cleanly)

Types live in `src/accent.rs`; all colours are `[u8; 3]` (keeps `Eq` on
`AppletConfig`, exact comparisons, no float epsilon anywhere):

- `accent_enabled: bool = false`
- `accent_snapshot: Option<AccentSnapshot>` where
  `AccentSnapshot { light: Option<[u8; 3]>, dark: Option<[u8; 3]> }`
  — outer `None` = no snapshot taken; inner `None` = "user had palette default".
- `accent_last_written: Option<AccentPair>` (`AccentPair { light: [u8; 3],
  dark: [u8; 3] }`) — persisted so the don't-clobber comparison survives applet
  restarts.

### App wiring (`src/app.rs`)

- `on_apply_success` changes signature to return `cosmic::app::Task<Message>`
  (today `()`, `src/app.rs:563`); its three call sites already sit in
  `Task`-returning paths and batch it in: `finish_refresh` (`:516`),
  `ApplyImage` (`:1077`, currently `return self.sync_shuffle(true)`),
  `ShuffleDue` (`:1119`).
- The spawned async task decodes the thumbnail and extracts the hue off the UI
  thread, finishing with `Message::AccentComputed { source: PathBuf,
  hue: Option<f32> }` — the **source path is the staleness guard** (repo
  convention, same hazard class as the generation-counter timers): the handler
  ignores a result whose `source` no longer equals `self.current`.
- Thumbnail access in the async task: `thumbs::thumbnail_path` + only decode
  when `thumbs::is_cached`; if `decode_failed` or not cached, log and change
  nothing (never `ensure_thumbnail` here — a `Failed` slot would re-decode the
  full UHD file on every apply).
- `AccentComputed` handler (UI thread, against live state): fresh
  `read_current_accents`, pure `accent_plan`, then execute the action.
- Pure `accent_plan(enabled, snapshot, last_written, current_builders, hue)
  -> AccentAction { Write { light, dark, snapshot_now }, Disarm, Skip }` — the
  tested analogue of `refresh_success_plan` / `wallpaper::classify`.
- Startup: `init` arms a reconciliation when `accent_enabled` — same async
  extraction for the restored `current`, so an accent/wallpaper change while
  the applet was down is caught (Disarm) or re-applied (Write) without waiting
  for the next apply.
- `Window` holds `accent_handles: Option<accent::ThemeHandles>` built in `init`
  mirroring how `config_context` is kept (`src/app.rs:943-953`); tests inject
  TempDir-rooted handles.

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code, tests, i18n, docs
- **Post-Completion** (no checkboxes): live-desktop verification, upstream watch

## Implementation Steps

### Task 1: Dominant-hue extraction in new `src/accent.rs`

**Files:**
- Create: `src/accent.rs`
- Modify: `src/main.rs` (register module)

- [x] create `src/accent.rs` with `dominant_hue(img: &RgbImage) -> Option<f32>`:
      Oklch conversion via re-exported `palette` crate, chroma-weighted
      36-bucket hue histogram, L-based dark/light pixel mask, circular mean
      over peak bucket ±1
- [x] grey cutoff on **mean chroma of unmasked pixels** (all-masked → `None`)
- [x] write tests on `RgbImage::from_fn` synthetic images: solid vibrant colour
      → its hue (±5°), vibrant object on grey ground → object hue
      (dominant-*vibrant*, not dominant-pixel), grey/near-grey image → `None`
- [x] write tests for edge cases: 1×1 image, pure black/white image (mask must
      not panic or divide by zero), hue wrap-around at 0°/360° — same grey
      threshold must behave identically for tiny and thumbnail-sized images
      (normalised cutoff)
- [x] run `just check` - must pass before task 2

### Task 2: Tone band, transplant, gamut mapping, WCAG guard

**Files:**
- Modify: `src/accent.rs`

- [x] `tone_band(palette: &CosmicPaletteInner) -> (f32, f32)`: mean Oklch
      (L, C) of the 8 chromatic `accent_*` fields (`Srgba` → `Srgb`)
- [x] `accent_for(palette, hue: Option<f32>) -> Srgb`: transplant, then
      gamut-map by **chroma reduction at fixed (L, h)** (binary search on C);
      `None` → `accent_warm_grey`
- [x] WCAG guard via `Wcag21RelativeContrast`: single check ≥ 6.0 against the
      better of white/black; failure → `accent_warm_grey` (no nudge loop)
      ⚠️ threshold was specified as 4.5 but that is below the ≈ 4.58
      mathematical floor of max-contrast-vs-white-or-black, i.e. a guard that
      can never fire — see the correction note under "Transplant + gamut
      mapping + guard" above
- [x] write tests: transplant preserves hue **after gamut mapping** (±small,
      full 0..360 sweep in 1° steps for both `DARK_PALETTE` and
      `LIGHT_PALETTE`), and the same sweep passes the WCAG guard without
      hitting the fallback
- [x] write tests: light vs dark palette produce different tones from the same
      hue; grey fallback returns the palette's own warm grey; a synthetic
      pathological palette (mid-luminance accents) does trigger the
      warm-grey fallback (the branch must not be dead code)
- [x] run `just check` - must pass before task 3

### Task 3: Theme writer with snapshot/restore + persisted colour types

**Files:**
- Modify: `src/accent.rs`

- [x] define the persisted types `AccentPair` / `AccentSnapshot` (`[u8; 3]`
      colours, serde + `Eq`), plus `[u8; 3]` ↔ `Srgb` conversion helpers
      (quantise-then-convert so the written f32 is exactly `u8/255`)
- [x] `ThemeHandles` struct bundling the four `Config`s (light/dark ×
      builder/theme), with a `::system()` constructor and a test constructor
      rooting all four in a `TempDir` via `Config::with_custom_path`
- [x] `read_builder(mode)` per the Theme-write recipe: `get_entry` accepting
      the partial, then **probe the `palette` key directly and substitute the
      mode default on failure** (never `ThemeBuilder::default()`);
      `read_current_accents(&ThemeHandles) -> (Option<[u8; 3]>, Option<[u8; 3]>)`
- [x] `write_accents(&ThemeHandles, light, dark)` — **`set_accent` single-key
      write** on each builder + `.build().write_entry` on each derived theme;
      `restore_accents(&ThemeHandles, snapshot)` writing `Option<Srgb>` back
      verbatim (including `None`) the same way
- [x] write tests in a `TempDir`: write → read-back of builder accent and
      derived theme accent (exact `[u8; 3]` round-trip through the RON file);
      restore of `Some` and of `None`; light handle with absent palette key
      **builds a light-palette theme** (assert the palette variant and inner —
      this is the dark-default-leak regression test); builder file contains
      **only** the `accent` key after our write (no pinning of `active_hint`,
      `window_hint`, `palette`, …)
- [x] run `just check` - must pass before task 4

### Task 4: `AppletConfig` fields for enable, snapshot, last-written

**Files:**
- Modify: `src/config.rs`

- [x] add `accent_enabled: bool` (default **false**), `accent_snapshot:
      Option<AccentSnapshot>`, `accent_last_written: Option<AccentPair>` using
      the Task 3 types (`Eq` on `AppletConfig` must keep compiling — that is
      why the colours are `[u8; 3]`)
- [x] write-on-change setters following the existing pattern; `normalize()`
      leaves the new fields alone (setters come from the `CosmicConfigEntry`
      derive, same as the existing fields — no hand-written ones needed)
- [x] write tests: defaults (feature off, no snapshot), round-trip through a
      `with_custom_path` config, pre-existing v1 entry without the new fields
      still loads (backward compat)
- [x] write tests for setter write-on-change behaviour (no write when equal)
- [x] run `just check` - must pass before task 5

### Task 5: Pure apply/disarm decision (`accent_plan`)

**Files:**
- Modify: `src/accent.rs`

- [x] define `AccentAction { Write { light, dark, snapshot_now }, Disarm, Skip }`
      and `accent_plan(enabled, snapshot, last_written, current_builders, hue)`
      — don't-clobber: any current builder accent differing (exact `[u8; 3]`
      compare) from `last_written` means the user intervened → `Disarm`
      ➕ the sketched signature also gained the two palette refs
      (`light_palette`/`dark_palette`, from `read_builder`): `Write` carries
      concrete `[u8; 3]` colours the executor just writes, and computing them
      from `hue` needs each builder's own tone band — still pure; a
      `BuilderAccents` type alias names the `(light, dark)` current pair
- [x] snapshot lifecycle per Solution Overview: enable-time snapshot is taken
      by the caller (always re-snapshot on enable); `Disarm` means clear
      snapshot + last-written, **no restore**; plan never writes when disabled
      (the plan itself never captures the snapshot: `Write.snapshot_now` flags
      "no snapshot persisted — capture the user's accents before writing")
- [x] write tests: disabled → `Skip`; first write after enable carries
      `snapshot_now`; steady state rewrites; user-changed-accent-in-Settings →
      `Disarm`; user change while applet was stopped (persisted `last_written`
      vs fresh read at startup reconciliation) → `Disarm`
- [x] write tests: grey hue (`None`) still writes (warm grey); the
      enable→manual-change→disarm→re-enable→disable sequence ends with the
      accents from *re-enable time* restored (not the original pre-feature
      ones) — the re-snapshot rule
- [x] run `just check` - must pass before task 6

### Task 6: Wire into the app message loop

**Files:**
- Modify: `src/app.rs`

- [ ] change `on_apply_success` to return `cosmic::app::Task<Message>` and
      batch it at its three call sites (`src/app.rs:516`, `:1077`, `:1119`)
- [ ] async extraction task producing `Message::AccentComputed { source, hue }`:
      `thumbs::thumbnail_path`, decode **only if `thumbs::is_cached`**, skip
      (log, no change) on `decode_failed`/missing — never `ensure_thumbnail`
      on this path; spawned from `on_apply_success` and gated on
      `accent_enabled`
- [ ] `AccentComputed` handler: drop stale results (`source != self.current`),
      fresh `read_current_accents`, `accent_plan`, execute the action (write +
      persist snapshot/last-written via config setters; `Disarm` flips
      `accent_enabled` off through the setter so the UI row follows and clears
      snapshot + last-written without restoring)
- [ ] `Message::SetAccentEnabled(bool)`: on → snapshot live accents (always),
      then immediate compute for the current wallpaper; off →
      `restore_accents(snapshot)`, clear snapshot + last-written
- [ ] startup reconciliation in `init`: when `accent_enabled` and a current
      wallpaper was restored, arm the same extraction task; add
      `accent_handles: Option<accent::ThemeHandles>` to `Window`, built in
      `init` (mirroring `config_context`, `src/app.rs:943-953`), injectable in
      tests
- [ ] every failure path (no thumbnail, decode error, config write error) logs
      via `tracing` and changes nothing
- [ ] write tests for the handler-level decisions kept pure (plan execution
      mapping, stale-source drop, toggle-off clears persisted state) using
      injected `ThemeHandles` + `TempDir` config
- [ ] run `just check` - must pass before task 7

### Task 7: Popup UI row + i18n across all 73 catalogues

**Files:**
- Modify: `src/view.rs`
- Modify: `i18n/en/cosmic_bing_wallpaper.ftl` and the 72 other
  `i18n/*/cosmic_bing_wallpaper.ftl`

- [ ] add `accent_toggler` row after the shuffle row in `src/view.rs`
      (copy the `shuffle_toggler` idiom, `src/view.rs:372`), bound to
      `Message::SetAccentEnabled`
- [ ] add the new message id(s) (e.g. `match-accent-to-wallpaper`) to
      `i18n/en/cosmic_bing_wallpaper.ftl`
- [ ] machine-translate the id(s) into the 72 remaining catalogues (pattern of
      commit `dc38450`) — the guard tests fail on any miss
- [ ] confirm `every_message_id_is_referenced_by_the_ui` passes (no orphaned
      ids) and `loader_is_pinned_to_english` still holds
- [ ] run `just check` - must pass before task 8

### Task 8: Verify acceptance criteria

- [ ] verify all requirements from Overview: off by default, hue transplant per
      the decided algorithm, gamut-mapped + WCAG-guarded, snapshot/restore
      exact (incl. the `None` = palette-default state), don't-clobber disarm,
      builder writes touch only the `accent` key
- [ ] verify failure paths leave the accent untouched (grep the handler for an
      early-return on every `Err`)
- [ ] run full test suite: `just check`
- [ ] smoke-test on the live desktop: `just install`, toggle on → accent
      follows wallpaper across browse/shuffle/refresh; change accent in
      Settings → applet disarms (toggle drops, chosen accent stays); toggle
      off → snapshot accent returns; light/dark flip shows per-mode tones;
      restart applet with feature on → startup reconciliation runs
- [ ] confirm no new dependencies were added to `Cargo.toml`

### Task 9: [Final] Update documentation

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `docs/plans/20260808-accent-from-wallpaper-notes.md`
- Move: this plan and the notes file → `docs/plans/completed/`

- [ ] update README.md (feature blurb, off-by-default note, and: turn the
      feature off before uninstalling — the derived accent stays and the
      snapshot dies with the applet config)
- [ ] update CLAUDE.md architecture section (`src/accent.rs` entry: what it
      owns, the hue-transplant rule, the single-key builder write, the
      don't-clobber/disarm + snapshot lifecycle invariants)
- [ ] fold corrections into the notes file (§2's write recipe is superseded —
      mark it in place the way the lock-screen corrections were)
- [ ] move this plan and the notes file to `docs/plans/completed/`

## Post-Completion

*Items requiring manual intervention or external systems - no checkboxes, informational only*

**Manual verification:**
- Extended live usage across several daily Bing images: do the derived accents
  *feel* right (vibrancy, not washed out) for photographic wallpapers with
  dusk/dawn palettes? Tune the chroma threshold / tone band only with evidence.
- Verify a user-customised palette (non-stock theme) still yields a legible
  accent (the tone band follows the user's palette by design).

**External systems to watch:**
- [cosmic-settings#343](https://github.com/pop-os/cosmic-settings/issues/343) —
  when COSMIC ships native accent-from-wallpaper, this feature should be
  retired: the restore path (Task 3) is the exit strategy.
- If `Cargo.toml`'s libcosmic rev moves, re-verify the `cosmic-theme` citations
  (builder read/probe recipe, `set_accent` serialisation shape, palette field
  names) — that crate is not API-stable.
