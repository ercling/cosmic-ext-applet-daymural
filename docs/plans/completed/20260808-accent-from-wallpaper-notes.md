# Notes: setting the COSMIC accent colour from the applet

**Status: research only, not a plan.** Gathered 2026-08-08 to feed a later plan.
Every claim below was read out of the pinned libcosmic rev (`8a017a1`,
`cosmic-theme/`) or observed on this machine (`cosmic-*` 1.5.0-1.fc44, Fedora
44) — nothing here is recalled API. Re-verify the `cosmic-theme` citations if
`Cargo.toml`'s libcosmic rev moves; that crate is not API-stable.

Short answer: **yes, an applet can set the accent**, with a same-user
`cosmic-config` write and no new dependency (`cosmic::cosmic_theme` is
re-exported by libcosmic). The work is not the write; it is choosing a colour
that stays legible, and being reversible.

## 1. Where the accent lives

Four configs, all under `~/.config/cosmic/`, all owned by the user:

| Config id | Role | Type of `accent` |
| --- | --- | --- |
| `com.system76.CosmicTheme.Light.Builder` (`LIGHT_THEME_BUILDER_ID`) | **input** — user's overrides | `Option<Srgb>` |
| `com.system76.CosmicTheme.Dark.Builder` (`DARK_THEME_BUILDER_ID`) | same, dark mode | `Option<Srgb>` |
| `com.system76.CosmicTheme.Light` / `.Dark` (`LIGHT_THEME_ID` / `DARK_THEME_ID`) | **derived** theme every app reads | full `Component`s |
| `com.system76.CosmicTheme.Mode` (`THEME_MODE_ID`) | which mode is live | `is_dark`, `auto_switch` (`model/mode.rs:11-16`) |

`ThemeBuilder` and `Theme` are both `#[version = 2]` and both derive
`CosmicConfigEntry` (`model/theme.rs:48`, `:844`), so they carry
`get_entry`/`write_entry` plus generated `set_<field>` setters — the same
plumbing `src/config.rs` already uses for `AppletConfig`.

Observed here (light mode, `Mode/v1/is_dark = false`):

```
~/.config/cosmic/com.system76.CosmicTheme.Light.Builder/v1/
  accent -> Some((red: 0.0, green: 0.32156864, blue: 0.3529412))   # the user's teal
  active_hint, corner_radii, gaps, spacing                          # untouched by us
```

## 2. The write recipe

`accent: Option<Srgb>` — `None` means "use the palette default", which
`build()` resolves to `palette.accent_blue` (`model/theme.rs` ~1108). So the
*absence* of a value is itself a meaningful state to preserve (see §5).

```rust
use cosmic::cosmic_theme::{Theme, ThemeBuilder, palette::Srgb};

// per mode: ThemeBuilder::dark_config() / light_config() (theme.rs:1641-1649)
let builder_cfg = ThemeBuilder::dark_config()?;
// A partial/corrupt entry returns Err((errors, fallback)). Do NOT fall back to
// ThemeBuilder::default() — use the mode-specific ThemeBuilder::dark()/light()
// (theme.rs:955-990), or the palette flips with it.
let builder = ThemeBuilder::get_entry(&builder_cfg)
    .unwrap_or_else(|(_errs, partial)| partial)
    .accent(Srgb::new(r, g, b));            // theme.rs:1044
builder.write_entry(&builder_cfg)?;          // 1. the input

let theme_cfg = Theme::dark_config()?;       // theme.rs:170-178
builder.build().write_entry(&theme_cfg)?;    // 2. the derived theme (theme.rs:1072)
```

**Both writes are required.** Nothing on the system rebuilds the theme from the
builder: `grep ThemeBuilder` over the cosmic-settings-daemon checkout is empty,
and cosmic-settings does the `.build()` itself. Writing only the builder changes
nothing visible; writing only the theme is undone the next time anything else
rebuilds. Propagation is then automatic — every libcosmic app watches
`com.system76.CosmicTheme.{Light,Dark}`, so the repaint is live and includes our
own popup and the panel.

⚠️ **Superseded (2026-08-08, plan review — the recipe above is unsafe as
written; the shipped implementation is `src/accent.rs`, per the plan's "Theme
write" section in `20260808-accent-from-wallpaper.md`).** Two faults, both
found before any code was written; the "both writes are required" and
propagation facts above still hold:

- `builder.write_entry(&builder_cfg)` writes **every** builder field in one
  transaction, materialising keys the user never set user-locally — pinning
  them there and cutting the user off from future COSMIC default changes. The
  `CosmicConfigEntry` derive also generates single-key setters, so the shipped
  write is `builder.set_accent(&builder_cfg, Some(srgb))` — only the `accent`
  key ever changes on the builder. (`set_accent` serialises the bare
  `Option<Srgb>` as exact-f32 RON, matching what is on disk in §1.) The
  derived `Theme`'s full-entry `write_entry` stays exactly as above — that one
  is correct and matches cosmic-settings.
- The `unwrap_or_else(|(_errs, partial)| partial)` read is not enough, and the
  code comment's `dark()`/`light()` advice covers only the `Err` arm:
  `get_entry` starts from `Self::default()` — whose palette is
  **`DARK_PALETTE`** — and silently skips `NoConfigDirectory` errors, so a
  light builder with no `palette` key on disk comes back with the dark palette
  on the **Ok** path. The palette must be probed directly
  (`ConfigGet::get::<CosmicPalette>(cfg, "palette")`) and the mode's own
  default substituted on failure; never fall back to
  `ThemeBuilder::default()`.

Also correcting §1's observation while here: `ThemeBuilder` and `Theme` are
`#[version = 2]`, so reads prefer and writes land under `…/v2/` — the `v1/`
paths and values quoted above are stale leftovers; don't debug accent values
seen there.

## 3. What an accent repaints

Not just "highlights". In `build()` the accent feeds `accent` itself,
`accent_button`, `link_button`, `icon_button`'s `on`, the `button` component's
`on`, `destructive`'s accent slot, and `accent_text`
(`model/theme.rs:1438-1500`, `:1303-1345`). It is a system-wide restyle from a
wallpaper applet — that framing should drive the UX (opt-in, off by default).

Deliberately **out of scope**: `active_hint` (a width, `u32`) and `window_hint`
(`Option<Srgb>`, cosmic-comp's window outline colour) are separate builder
fields (`theme.rs:897-900`, default `None`) — leave them alone.

## 4. Legibility: what cosmic-theme guarantees, and what it doesn't

- **Accent *text* is protected.** `build()` computes `accent_text` only when the
  surface contrast is under 4:1, walks a 100-step ramp toward legibility, and
  falls back to pure white (dark mode) or pure black (light mode) if the derived
  step still fails (`model/theme.rs:1303-1345`). `Theme::accent_text_color()`
  falls back to `accent.base` when the guard decided nothing was needed
  (`:497-499`).
- **Accent *fills* are not measured.** The label on an accent-filled button
  comes from `get_text` (`steps.rs:79-115`), which picks a step 70 (then 50)
  positions away from the accent's own lightness index and otherwise clamps to
  the ramp end. That is a lightness-distance heuristic, not a contrast ratio —
  so a **mid-luminance** accent is the failure case: white and black are both
  mediocre on it.

Implication for the plan: the colour rule must reject mid-luminance candidates
and cap chroma, then *verify* with a real WCAG ratio. We already have that
maths — `luminance`/`contrast` in `src/tooltip.rs`'s tests — and it should move
into a shared, tested helper rather than be re-derived.

## 5. Reversibility and don't-clobber

- Snapshot the user's accent **before the first override**, distinguishing
  `Some(colour)` from `None` (= palette default), and restore exactly that when
  the feature is switched off. A new `AppletConfig` field, versioned as usual.
- Mirror the wallpaper rule: if the user picks an accent by hand in Settings
  afterwards, stop overriding until they re-arm it. `AppletConfig`'s existing
  watch subscription pattern extends to the builder config — the same shape as
  `wallpaper::should_auto_apply`.
- Both modes need a value, or toggling light/dark shows a stale accent. Options
  for the plan: write both from one colour, derive a per-mode variant, or write
  the active mode and re-derive on `ThemeMode` change.

## 6. Where the colour comes from

- `image` is pinned to 0.25 with **only** the `jpeg` feature, so no quantiser is
  available (`color_quant` rides along with gif/png) — a palette extractor has
  to be hand-rolled. Fine: a histogram in HSL over a downscaled image is a
  few dozen lines and unit-testable on synthetic pixels, which suits this
  repo's testing rules.
- Feed it the **cached 480×270 thumbnail** (`src/thumbs.rs`), not the ~5 MB UHD
  file: ~130k pixels, already decoded for the popup, and the cache's identity
  rules (mtime+size sidecar, `failed` verdicts) already handle staleness. The
  UI never decodes the full image and this must not either.
- Bing ships no palette metadata in `HPImageArchive` (see `src/bing.rs` types),
  so there is nothing to read instead of computing it.

## 7. Testability

`Config::with_custom_path(name, version, PathBuf)` exists
(`cosmic-config/src/lib.rs:264`), so both theme handles can be rooted in a
`tempfile::TempDir` exactly like `Config::with_custom_path` in
`src/config.rs`'s tests. The rule "tests never touch real user config/state"
therefore survives: inject the two `Config`s into the writer, keep the
colour-choosing rule a pure function, and the untested remainder is two
`write_entry` calls.

## 8. Prior art — upstream intends to ship this

- **pop-os/cosmic-settings#343** (open): "Appearance Settings UI to
  automatically change the accent to match the wallpaper".
- **pop-os/cosmic-settings#204** (closed as *duplicate* of #343): same request.
  Maintainer comments there: git-f0x — "planned post-release (i.e. extract theme
  from wallpaper like Plasma/Material You)"; wash2 — "this could possibly be a
  toolkit variable and xdp-cosmic could monitor the cosmic-bg state for the
  current wallpaper to calculate the accent colour". They also floated
  *per-output* accents.

Consequences worth designing around: the feature is a stopgap for something
COSMIC plans natively, so it should be off by default, cheap to retire, and
must not leave a mess behind when it is (§5 restore). Per-output accents are out
of scope for us regardless — applying a wallpaper forces `same-on-all`
(`src/wallpaper.rs`), so this applet has exactly one wallpaper to reason about.

## 9. Decisions the plan still has to make

1. Colour rule: dominant cluster vs. most-vibrant vs. hue-average — and the
   luminance/chroma band that keeps §4 satisfied.
2. One accent for both modes, or a per-mode adjustment of the same hue.
3. When to recompute: every apply (browse/shuffle/refresh) or only on refresh.
4. UI surface: a toggle row in the popup (new FTL ids → **all 73 catalogues**,
   per CLAUDE.md) and whether to preview the colour.
5. Failure behaviour: image undecodable, colour rejected by the contrast guard,
   theme config unwritable — each should leave the user's accent untouched.
6. Whether a new `src/accent.rs` module is the right home (it is a different
   config domain from `wallpaper.rs`, which should stay cosmic-bg-only).

## 10. Incident & fix — 2026-08-09: btrfs fsync freeze + echo oscillation

**Observed live** (Fedora 44, btrfs home) when the user enabled the toggle:
the applet froze for minutes (D-state, wchans `barrier_all_devices` /
`folio_wait_bit_common` / `wait_log_commit` — btrfs fsync/commit paths) while
theme keys trickled out at ~1–2/s; cosmic-panel logged 18×
`Failed to get theme entry value: GetKey("list_button", NotFound)` (it read
the derived theme dir mid-write — cosmic-config transactions are not
reader-atomic); after the first write cycle completed the applet immediately
started another (`accent_last_written` flipped Some→None on disk), and the
panel/dock flapped as every surface reloaded its theme per write. Recovery
was manual (SIGSTOP, flip `accent_enabled=false` on disk, SIGKILL).

Two root causes, two fixes (branch `fix-accent-async-writes`):

- **BUG A — the freeze**: the whole write batch (2× builder `set_accent` +
  2× `build()` + 2× full derived-theme `write_entry`, ~40 fsync'd atomic key
  writes per mode) ran synchronously inside `update()`. Under I/O pressure
  each fsync forces a btrfs transaction/log commit (~1 s), so one cycle
  blocked the event loop for minutes. Fixed twice over:
  - the theme write/restore now runs on the blocking pool
    (`AccentInflight` task, completion message `AccentWriteFinished`,
    mirroring `extract_accent_hue`), so `update()` never blocks;
  - the derived-theme write is now a **changed-keys-only transaction**
    (`accent::write_theme`, upstream cosmic-settings' `build_theme` pattern
    — read `Theme::get_entry`, diff field-by-field, `tx.set` only
    differences). ⚠️ The original full-entry `write_entry` was **our
    deviation from upstream** — the plan's Theme-write section claimed it
    "matches upstream (cosmic-settings does the same)", which was wrong and
    is corrected there in place. An accent change legitimately touches ~20
    of 39 keys (every component's `focus` ring is the accent), still half
    the fsyncs and a much smaller torn-read window. Virgin theme dirs
    (probed via `is_dark`) still get one full materialising `write_entry`:
    upstream's diff base is `get_entry`'s default = `Theme::preferred_theme()`,
    which is environment-dependent (`XDG_CURRENT_DESKTOP`) and could
    nondeterministically skip `is_dark` itself. Note the first diff after a
    materialisation rewrites `shade`/`accent_text` once — repr migration
    from the derive's lossy hex `ColorRepr` to the bare-exact form upstream
    writes (the hex form would re-flag those keys on every diff).
- **BUG B — the oscillation**: with minute-long writes, our own multi-key
  config persists echoed back through the watcher stale/torn relative to
  in-memory state, and the `ConfigUpdated` arm routed the apparent
  `accent_enabled` flip through the real toggle lifecycle (deliberate for
  genuine external flips; every prior echo-interleaving analysis assumed
  writes are fast). Stale echo → spurious disable/disarm → re-enable →
  another full write cycle → more echoes. Fixed with a write guard:
  while an accent task is in flight (`accent_inflight`, generation counter,
  stale completions ignored) `ConfigUpdated` routes no flip,
  `AccentComputed` results are dropped-and-rearmed, and user toggles are
  recorded + pinned to disk; the task's completion reconciles exactly once —
  a recorded user toggle wins (a concurrent `set_config` full-entry write
  can rewrite the pinned disk flag from stale memory), else a genuine
  external flip read from the **disk** (never the suppressed echo payloads)
  — and routes it through the normal lifecycle then.

  A follow-up review round (2026-08-09) hardened three edges of that guard:

  - `ConfigUpdated` now **never adopts the three accent fields from a
    watcher payload — in-flight or not**. Payloads are read at event time
    and can be delivered a message late, so a stale echo can arrive *after*
    `AccentWriteFinished` retired the guard; adopting its mid-flight
    `accent_last_written: None` while the builders hold the new pair makes
    the next recompute Disarm spuriously (feature off, accent stranded —
    the oscillation class under I/O pressure). In-memory accent state is
    authoritative (single-instance assumption); a not-in-flight payload
    flip is treated as evidence only and routed with the value a fresh
    disk read confirms.
  - The completion's reconcile takes its fresh disk read **before the
    completion handlers' own persists** and compares it against a
    **spawn-time baseline** (`accent_disk_enabled_at_spawn`, kept across a
    chained rollback). Reading after them read our own write back: an
    `EnableRestore` completion's `arm_accent_enable` pins the flag `true`
    over an external disable written mid-flight, and a `DisableRestore`
    completion's full `set_config` rewrites `false` over an external
    enable — both silently stomped instead of routed. The baseline is what
    distinguishes a genuine mid-flight flip (disk *changed*) from the
    enable path's deliberate flag-lands-last ordering (disk still `false`
    during a popup-origin `EnableRestore` — no external edit happened).
  - A rollback that itself fails (themes and config failing together) now
    **repairs the on-disk `accent_last_written`**: the themes keep the new
    pair, memory adopts it so this session's guard holds — but then every
    later recompute Skips and the stale on-disk record would survive until
    a restart hit the destructive `Disarm { keep_snapshot: false }` (the
    old comment claimed the gap disarm covered this; it does not — the
    record on disk is `Some`, not the gap's `None`). Best-effort ladder:
    persist `Some(pair)` (a restart then Skips), else clear to `None` (the
    gap shape, whose disarm keeps the snapshot); only both failing leaves
    the destructive shape, with nothing writable left to repair it.

  The same round also made the changed-keys-only claim *physically* true
  and test-enforced: the theme diff now compares **serialized bytes**
  (`would_rewrite`, same ron serializer as cosmic-config's `set`), because
  colour-bearing `Component`/`Container` fields round-trip lossily through
  the hex `ColorRepr` quantisation — upstream's value-space diff re-flags
  every one of them forever, and byte-identically rewrote all ~19 colour
  keys of an *unchanged* mode on each write (half the freeze's fsyncs,
  invisible to content assertions). The
  `theme_rewrites_transact_only_the_changed_keys` test pins every key
  file's mtime to a sentinel and asserts the physically rewritten set
  equals the content-changed set — and that the unchanged mode sees no
  file writes at all.

Invariants across the async split (all still test-enforced, now through
`settle_accent_tasks` driving the completion messages): snapshot persisted
before the theme write starts; `last_written` persisted only after the write
succeeds (in the completion handler; its failure chains an async rollback
task restoring the accents the plan compared); `write_accents`' internal
half-write rollback stays inside the blocking task; the plan's builders
travel into the task (no re-read TOCTOU); disable flips the toggle
immediately but clears the snapshot only after its restore verifiably lands
(failure keeps it — the kept-snapshot shape); an enable's deferred restore
arms the feature only from a *successful* completion. New adaptation worth
naming: a crash while a disable-restore is in flight now leaves
`enabled=false` + snapshot kept on disk, which the next enable's deferred
restore already reconciles — strictly better than the old synchronous
crash-mid-restore exposure.
