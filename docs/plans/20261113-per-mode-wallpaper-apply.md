# Per-Mode Wallpaper Apply Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make "apply wallpaper" work again on a cosmic-bg that has per-mode
backgrounds, without breaking a cosmic-bg that does not, from one binary
(native and Flatpak).

**Terms.** The feature is not tied to a release number we can name yet, so
this plan names the two layouts instead:

- **legacy** — cosmic-bg up to and including the `epoch-1.11.0` tag
  (`9520e16`, 2026-10-20). Its config crate is byte-identical to our pin
  `1685f7f` (`git diff 1685f7f epoch-1.11.0 -- config/src/` is empty).
- **per-mode** — cosmic-bg `d6dcfa9` and later. As of 2026-11-13 that is
  upstream `master` only: no tag contains `d6dcfa9`, so it first ships in
  whatever release follows 1.11.0.

Never branch on a COSMIC version number. The on-disk layout is the only
signal, and it is what the code below detects.

**Architecture:** Stop going through the pinned `cosmic-bg-config` Context API
for the *config* side. Mirror what `poke_state` already does for the state side:
read and write raw keys through **our** pinned `cosmic::cosmic_config`
instance. Use a local, version-tolerant entry type. Pick the write targets at
runtime from the on-disk layout. That also makes `apply` and `current_wallpaper`
hermetically testable via `Config::with_custom_path`, which they are not today.

**Tech Stack:** Rust 2024, libcosmic `cosmic_config` (locked rev), serde, ron 0.12.

**Spec:** user report "Since the new themes, the wallpaper doesn't apply
anymore" + upstream pop-os/cosmic-bg `d6dcfa9` "feat: add per-mode backgrounds"
(authored 2026-10-28, landed on `master` 2026-11-06, **after** the
`epoch-1.11.0` tag; the reporter is therefore on a master/nightly build).
Evidence below is from a scratch clone of upstream `master` (`2439903`) diffed
against our pin `1685f7f`.

## Root Cause (evidence)

| | legacy cosmic-bg (our pin `1685f7f` … `epoch-1.11.0`) | per-mode cosmic-bg (`d6dcfa9`+) |
|---|---|---|
| Config it reads/watches | `com.system76.CosmicBackground` v1 | `com.system76.CosmicBackground.Dark` v1 and `.Light` v1; displays the one matching `com.system76.CosmicTheme.Mode` v1 key `is_dark` (unreadable → Dark) |
| Shared config | — | read **once at startup** by `Context::seed_from`, and only when the mode config has no *local* `all` key; never watched |
| `Entry` fields | `output, source, filter_by_theme (required), rotation_frequency, filter_method, scaling_mode, sampling_method`, `deny_unknown_fields` | adds `transition_duration: u64` (`serde(default = 500)`), `filter_by_theme` now `serde(default)`, still `deny_unknown_fields` |
| `same-on-all` | in shared config | per mode config |
| State (`wallpapers`, lock poke) | `com.system76.CosmicBackground` state | **unchanged** (`config/src/state.rs` not touched) |

`wallpaper::apply` writes only the shared config. Per-mode cosmic-bg never
looks at it after startup, so nothing changes on screen. There is a second
fault: any per-mode-written entry carries `transition_duration`. Our pinned `Entry` rejects
unknown fields, so `context.entry("all")` fails. `updated_entry` then starts
from defaults and drops the user's scaling/filter settings. `current_wallpaper`
also misreads per-mode state, because it reads the stale shared config.

**Rejected: bumping `cosmic-bg-config`** (the pasted `Mode::ALL` snippet). The
new `Entry` always serializes `transition_duration`. Writing it to the shared
config makes legacy cosmic-bg (`deny_unknown_fields`, which still includes the
released 1.11.0) fail to parse `all` and
fall back to its default wallpaper. A bump also drags lockfile and libcosmic
churn that AGENTS.md forbids as an incidental fix. We keep the snippet's idea
(write every mode) and implement it without the new crate API.

## Global Constraints

- No dependency changes: `Cargo.toml` and `Cargo.lock` stay byte-identical.
- Config names, versions and keys pinned verbatim: `com.system76.CosmicBackground`,
  `com.system76.CosmicBackground.Dark`, `com.system76.CosmicBackground.Light`
  (all version `1`); keys `all`, `same-on-all`. Theme mode:
  `com.system76.CosmicTheme.Mode` version `1`, key `is_dark`.
- Anything written to the **shared** config must still deserialize as the
  pinned `cosmic_bg_config::Entry` (the legacy oracle).
- Never create the `.Dark`/`.Light` config directories on a legacy system.
  Creating them makes the later per-mode upgrade skip `seed_from`, which would
  discard any wallpaper the user chose on legacy after our last apply.
- The cosmic-bg handles must be rooted in the **same directory** that
  `Config::new` uses for every other handle in this applet (applet config,
  accent, theme mode). In Flatpak that is not `dirs::config_dir()`; see
  Task 2's `config_root`. A handle rooted elsewhere fails silently: the
  probe says "legacy" and the write lands where cosmic-bg never looks.
- No Flatpak manifest change. `--filesystem=xdg-config/cosmic:rw` already
  covers the two new config directories.
- Tests use `Config::with_custom_path` under a `tempfile::TempDir`. Never touch
  real config.
- `apply`/`current_wallpaper` keep their public signatures; the call sites in
  `src/app.rs` (2681, 4433, 4472, 1064…2341) do not change.
- `just check` passes. No new Fluent ids.

## Review Focus

1. **Upgrade legacy → per-mode after using daymural.** Expected: the user's last
   wallpaper survives the upgrade, through upstream seeding from the shared
   config. Pinned by the Task 2 test `old_layout_apply_creates_no_mode_dirs`.
2. **Per-mode user with different dark/light wallpapers set in Settings.**
   Expected: an explicit apply sets both modes. Each mode keeps its own
   scaling, filter and transition settings. Pinned by the Task 3 test
   `new_layout_preserves_each_modes_own_fields`.
3. **Entry with fields we don't know about** (a future cosmic-bg). Expected:
   apply still changes the source and keeps known fields. Unknown fields are
   dropped, not fatal. Pinned by the Task 1 test `unknown_fields_are_tolerated`.
4. **Re-apply of the image already shown.** Expected: no file rewrite, so no
   spurious cosmic-bg reload or fade. Pinned by the Task 3 test
   `reapply_is_write_free` (inode check, like `bg_wallpapers_inode`).
5. **Theme mode switch after apply (per-mode).** Expected: the other mode
   shows the same daymural image, and `current_wallpaper` follows the active
   mode. Pinned by the Task 4 test `current_follows_active_mode`.
6. **Flatpak build.** Expected: same detection and same writes as the native
   build, against the host's `~/.config/cosmic`. Pinned by the Task 2
   `config_root_*` tests and the Task 5 Flatpak smoke step. Hermetic tests
   cannot exercise the sandbox itself.

---

### Task 1: Version-tolerant local background entry

**Files:**
- Modify: `src/wallpaper.rs` (replace the `cosmic_bg_config::Entry` usage in
  `updated_entry`/`classify`; add the type next to them)
- Test: `src/wallpaper.rs` `mod tests`

**Interfaces:**
- Produces:
  - `pub struct BgEntry { pub output: String, pub source: Source, #[serde(default)] pub filter_by_theme: bool, pub rotation_frequency: u64, #[serde(default, skip_serializing_if = "Option::is_none")] pub transition_duration: Option<u64>, #[serde(default)] pub filter_method: FilterMethod, #[serde(default)] pub scaling_mode: ScalingMode, #[serde(default)] pub sampling_method: SamplingMethod }`.
    Derives `Serialize, Deserialize, Clone, Debug, PartialEq`. It has **no**
    `deny_unknown_fields`. `Source`, `FilterMethod`, `ScalingMode` and
    `SamplingMethod` are reused from `cosmic_bg_config` (plain serde types, so
    the two-instance problem does not apply).
  - `impl BgEntry { pub fn new_all(source: Source) -> Self }`, with values copied
    from pinned `Entry::new`: `output "all"`, `filter_by_theme false`,
    `rotation_frequency 900`, `transition_duration None`, default enums.
  - `pub fn updated_entry(existing: Option<BgEntry>, path: &Path) -> BgEntry`.
    Same semantics as today.
  - `fn classify(same_on_all: bool, entry: Option<BgEntry>) -> CurrentWallpaper`.

- [x] **Step 1: Write failing tests.** Adapt the existing `updated_entry_*` and
  `classify_*` tests to `BgEntry`, and add:
  - `legacy_shape_round_trips_without_transition_duration`: parse the RON of a
    legacy entry (no `transition_duration`), apply `updated_entry`, serialize
    with `ron::to_string`. Assert the output has no `transition_duration` and
    that it parses as `cosmic_bg_config::Entry`, the pinned legacy oracle.
  - `new_shape_preserves_transition_duration`: parse per-mode-shape RON with
    `transition_duration: 1200, scaling_mode: Fit`. Assert both are kept and
    the source is replaced.
  - `unknown_fields_are_tolerated`: RON with an extra `future_field: 1` parses,
    and `updated_entry` keeps `scaling_mode`.
  - `missing_filter_by_theme_defaults_false`.
- [x] **Step 2:** `just check` shows these tests failing to compile (no `BgEntry`).
- [x] **Step 3:** Implement the items above. Leave `apply`/`current_wallpaper`
  on the old path for now; in this task they can convert through
  `cosmic_bg_config::Entry`, or you can fold this into Task 3 if that is simpler.
- [x] **Step 4:** `just check` passes.
- [x] **Step 5:** Commit `fix(wallpaper): add version-tolerant cosmic-bg entry type`.

### Task 2: Layout detection and target handles

Detection is a directory probe: per-mode iff
`<config root>/cosmic/com.system76.CosmicBackground.{Dark,Light}/v1` exists.
It relies on daymural never opening those configs on a legacy system.

**Verified 2026-11-13** (no further verification step is needed, and there is
no schema-dir fallback):

- *The directories exist on every per-mode system.* cosmic-bg `d6dcfa9`
  `src/main.rs:171-173` opens `Mode::ALL` contexts at every startup, before
  seeding. `Mode::context()` is `Config::new`, which runs
  `fs::create_dir_all` on the user path. Checked in our locked `cosmic-config`
  (`8a017a1`, `cosmic-config/src/lib.rs:249`, and `:281` for
  `with_custom_path`) and in upstream master's pin (`896b252`).
- *The Flatpak build sees them.* In a shell inside the installed sandbox
  (Flatpak 1.18.4, applet not started), `$XDG_CONFIG_HOME/cosmic` and
  `$HOME/.config/cosmic` are both read-write bind mounts of the host's
  `~/.config/cosmic` (same device and inode), so a directory cosmic-bg creates
  on the host appears live.
- *The system schema dir is not an option in Flatpak.* `/usr/share/cosmic`
  does not exist in the sandbox, which is why the probe uses the user dir only.

**Config root.** The locked `Config::new` does not use `dirs::config_dir()`.
Its private `get_config_dir()` (`cosmic-config/src/lib.rs:15`) resolves, when
`FLATPAK_ID` is set, `HOST_XDG_CONFIG_HOME`, then `$HOME/.config`, and only
then `dirs::config_dir()`. In the sandbox `dirs::config_dir()` is
`~/.var/app/<id>/config`, which reaches the host only through Flatpak's alias
mount for `xdg-config/<subdir>` permissions. That alias exists on a default
install but disappears if the permission is overridden (for example replaced
by a whole `xdg-config` or `home` grant), while `$HOME/.config/cosmic` keeps
working. `config_root` below therefore mirrors `get_config_dir()` exactly, so
these handles can never point somewhere different from the applet's other
`Config::new` handles. Upstream made `get_config_dir` `pub` by `896b252`;
after a future deliberate libcosmic bump, replace the helper with it.

**Detection notes** (accepted, no code):

- *Third-party directory creation.* Anything built on the new
  `cosmic-bg-config` (a newer cosmic-settings, another applet) can create the
  mode directories on a legacy cosmic-bg. We then write both modes and shared.
  Legacy cosmic-bg still reads shared, so apply keeps working.
- *First login after the upgrade.* If an apply probes before cosmic-bg has
  created the directories and writes shared after cosmic-bg has seeded from
  it, that one apply is not displayed. The probe runs on every call, so the
  next apply or refresh corrects it.
- *Downgrade.* Leftover mode directories are harmless for the same reason as
  the first note.

**Files:**
- Modify: `src/wallpaper.rs`
- Modify: `src/testutil.rs` (tempdir helpers)

**Interfaces:**
- Produces:
  - `const BG_SHARED: &str = cosmic_bg_config::NAME; const BG_DARK: &str = "com.system76.CosmicBackground.Dark"; const BG_LIGHT: &str = "com.system76.CosmicBackground.Light"; const BG_VERSION: u64 = 1;`
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum BgMode { Dark, Light }`,
    with `fn config_name(self) -> &'static str`. Do not reuse `accent::Mode`;
    it is a different concept.
  - `pub fn per_mode_layout(config_root: &Path) -> bool`, true iff
    `<root>/cosmic/<BG_DARK>/v1` or `<root>/cosmic/<BG_LIGHT>/v1` is a
    directory. It is a pure path probe and **opens no config**.
  - `pub struct BgTargets { pub shared: cosmic::cosmic_config::Config, pub modes: Vec<(BgMode, cosmic::cosmic_config::Config)>, pub theme_mode: Option<cosmic::cosmic_config::Config> }`.
  - `pub fn open_targets(config_root: &Path) -> Result<BgTargets, WallpaperError>`.
    It always opens `shared`. It opens `modes` (Dark then Light) only when
    `per_mode_layout` is true. It opens `theme_mode` best-effort. Everything is
    via `with_custom_path(name, 1, config_root)`.
  - `fn config_root_from(flatpak: bool, host_xdg_config_home: Option<OsString>, home: Option<OsString>, default: Option<PathBuf>) -> Option<PathBuf>`.
    Pure. When `flatpak`: `host_xdg_config_home` verbatim if `Some`, else
    `home.join(".config")` if `Some`, else `default`. When not `flatpak`:
    `default`, ignoring the other two. No extra filtering (an empty value is
    used as-is), to stay byte-for-byte with `get_config_dir()`.
  - `fn config_root() -> Option<PathBuf>` =
    `config_root_from(var_os("FLATPAK_ID").is_some(), var_os("HOST_XDG_CONFIG_HOME"), var_os("HOME"), dirs::config_dir())`.
  - `fn production_targets() -> Result<BgTargets, WallpaperError>`, rooted at
    `config_root()`. A missing config root is an error.
  - testutil: `pub fn bg_layout(root: &Path, per_mode: bool)` creates the
    directories for the requested layout.
    `pub fn bg_key_file(root: &Path, name: &str, key: &str) -> PathBuf`.

- [x] **Step 1: Failing tests:**
  - `old_layout_detects_shared_only`
  - `new_layout_detects_per_mode`: only the Dark dir exists → true.
  - `open_targets_old_layout_opens_no_mode_configs`: `modes.is_empty()`, and
    after the call neither the `.Dark` nor the `.Light` dir exists. This is the
    seeding-safety guard.
  - `mode_names_are_pinned`: literal strings, version 1.
  - `config_root_native_uses_default`: not Flatpak → `default`, even when
    `host_xdg_config_home` and `home` are `Some`.
  - `config_root_flatpak_prefers_host_xdg_config_home`.
  - `config_root_flatpak_falls_back_to_home_dot_config`: no host value →
    `<home>/.config`, **not** `default` (the sandbox-private dir).
  - `config_root_flatpak_without_host_or_home_uses_default`.
- [x] **Step 2:** `just check` → FAIL.
- [x] **Step 3:** Implement. Before writing `config_root_from`, re-read
  `get_config_dir()` at the locked rev and confirm the precedence above still
  matches it.
- [x] **Step 4:** `just check` passes.
- [x] **Step 5:** Commit `fix(wallpaper): detect cosmic-bg per-mode config layout`.

### Task 3: Apply to every target

**Files:**
- Modify: `src/wallpaper.rs` (`apply` and its doc/test note; module comment)

**Interfaces:**
- Consumes: `BgEntry`, `updated_entry`, `BgTargets`, `production_targets`.
- Produces: `pub fn apply_to(targets: &BgTargets, path: &Path) -> Result<(), WallpaperError>`.
  `pub fn apply(path) = check_apply_source → production_targets → apply_to`.

Per target config, in this order: each mode (Dark, Light), then shared. Modes
go first because they are what per-mode cosmic-bg displays. Shared still
matters for legacy cosmic-bg and for a future re-seed.
1. Read `all` as `BgEntry`. Missing key → `None`. **Present but unparseable**
   → also `None` (current behaviour, now rare), logged at `warn`.
   `with_custom_path` handles have no system path, so a missing *user* key
   no longer falls back to the distro default in `/usr/share/cosmic` as the
   pinned `context()` did natively. That is intended: the Flatpak build could
   never see that default, so both builds now behave alike. The only effect
   is that the first apply starts from `new_all` rather than the distro
   entry. `classify` decisions do not change, because a distro default is
   never one of our files.
2. Build `updated_entry(existing, path)`. Write `all` **only if** it differs
   from the existing value (change-only, as pinned `Config::set_entry` did).
3. Read `same-on-all` as `bool`, missing = `true`. Do **not** write on a plain
   read: the pinned getter's write-on-read side effect is intentionally
   dropped. Write `true` only if the value read was `false`.
4. Entry before `same-on-all`, as today (no flash of the previous default).

Fail fast on the first write error, mapped through `config_err`. The
`backgrounds` key is not touched, matching the pinned `set_entry` for the
`all` output.

- [x] **Step 1: Failing tests** (all under a TempDir via `open_targets`):
  - `old_layout_apply_writes_shared_only` (also asserts `old_layout_apply_creates_no_mode_dirs`).
  - `new_layout_apply_writes_both_modes_and_shared`: all three `all` keys have
    `source: Path(<img>)` and both modes have `same-on-all = true`.
  - `new_layout_preserves_each_modes_own_fields`: Dark has `scaling_mode: Fit,
    transition_duration: 1200`, Light has `Zoom` and no `transition_duration`.
    After apply, each keeps its own values.
  - `per_mode_same_on_all_false_flips_true`.
  - `reapply_is_write_free`: the inode of every key file is unchanged on the
    second apply.
  - `shared_entry_stays_legacy_parseable`: after a new-layout apply, the shared
    `all` file parses as pinned `cosmic_bg_config::Entry`. This holds even when
    the starting shared entry was missing.
  - The existing `apply_source_must_be_an_existing_regular_file` keeps passing
    and writes nothing.
- [x] **Step 2:** `just check` → FAIL.
- [x] **Step 3:** Implement `apply_to` and rewire `apply`. Remove the
  "untestable context plumbing" note and update the module header comment.
- [x] **Step 4:** `just check` passes.
- [x] **Step 5:** Commit `fix(wallpaper): apply to per-mode cosmic-bg configs`.

### Task 4: Read the displayed wallpaper from the active mode

**Files:**
- Modify: `src/wallpaper.rs` (`current_wallpaper`)

**Interfaces:**
- Produces: `pub fn current_from(targets: &BgTargets) -> CurrentWallpaper`, and
  `fn active_mode(theme_mode: Option<&Config>) -> BgMode`: `is_dark` true or
  unreadable → `Dark`, false → `Light`, matching upstream `Mode::current`.
  `pub fn current_wallpaper() = production_targets().map_or(Unknown, |t| current_from(&t))`.

Use the per-mode layout when `modes` is non-empty: classify the active mode's
`same-on-all` + `all`. Otherwise use the shared config. If the active mode's
config is missing from `modes`, return `Unknown`.

- [x] **Step 1: Failing tests:**
  - `old_layout_reads_shared`
  - `current_follows_active_mode`: Dark = daymural image A, Light = foreign
    image B. `is_dark=false` → `File(B)`, `is_dark=true` → `File(A)`, missing
    theme-mode config → `File(A)`.
  - `new_layout_ignores_stale_shared`: shared shows an old image, the mode
    shows the new one, and the mode wins.
  - `new_shape_entry_is_classified`: the entry has `transition_duration`
    → `File`, not `NoFile`. This is the regression the current code has.
- [x] **Step 2:** `just check` → FAIL.
- [x] **Step 3:** Implement.
- [x] **Step 4:** `just check` passes.
- [x] **Step 5:** Commit `fix(wallpaper): read current wallpaper from active cosmic-bg mode`.

Known, accepted behavior to document (not code): an apply, including an
auto-apply, sets **both** modes to the same image. The do-not-clobber check
only looks at the active mode. A light-only foreign wallpaper is therefore
replaced by the next auto-apply that runs while dark mode is active, and the
reverse holds too.

### Task 5: Docs and manual verification

**Files:**
- Modify: `README.md` (compatibility: works with cosmic-bg both before and
  after per-mode dark/light backgrounds; applying sets dark and light). Do not
  name a COSMIC release for the per-mode side: 1.11.0 does **not** have it,
  and the release that will is not tagged yet.
- Modify: `docs/limitations.md` (the both-modes behavior from Task 4). No
  Flatpak detection caveat is needed; see Task 2's verified facts.
- Modify: `AGENTS.md` (Lock-screen/wallpaper section, one line: "The
  `cosmic-bg` config side uses raw keys via our `cosmic_config` and a local
  `BgEntry`; per-mode configs are written only when their directory already
  exists. Never write `transition_duration` into the shared config unless it
  was already there. Root these handles with `wallpaper::config_root`, which
  mirrors cosmic-config's Flatpak-aware `get_config_dir`; never with
  `dirs::config_dir()`. Detect the layout, never a COSMIC version number.")

- [x] **Step 1:** Update the docs.
- [x] **Step 2:** `just check` passes.
- [ ] **Step 3: Manual smoke, legacy, native.** This machine runs
  `cosmic-bg-1.10.0`. Install with `just install`, restart the panel, and
  apply from the popup. Expected: the wallpaper changes, and neither
  `~/.config/cosmic/com.system76.CosmicBackground.Dark` nor `.Light` exists.
- [ ] **Step 4: Manual smoke, legacy, Flatpak.** Same machine. `just uninstall`
  the native build first (two installs means two leaders), then
  `just flatpak-sources` and `just flatpak-install`, restart the panel, and
  apply from the popup. Expected: the wallpaper changes;
  `~/.config/cosmic/com.system76.CosmicBackground/v1/all` on the **host**
  names the new image; neither mode directory exists under
  `~/.config/cosmic`.
- [ ] **Step 5: Manual smoke, per-mode.** Needs a tester on cosmic-bg
  `d6dcfa9`+ (master/nightly, or the release after 1.11.0), ideally the
  reporter. A stock 1.11.0 system is **legacy** and proves nothing here:
  first confirm `~/.config/cosmic/com.system76.CosmicBackground.Dark/v1`
  exists. Then apply, toggle dark/light in Settings, and confirm both modes
  show the image and the custom scaling survived. Ask for one run with the
  native build and one with the Flatpak build.
- [x] **Step 6:** Commit `docs: document per-mode wallpaper support`.

## Implementation notes (2026-11-14)

Tasks 1–4 and the Task 5 docs are implemented on
`fix/per-mode-wallpaper-apply`. The manual smoke steps (Task 5, Steps 3–5) are
still open. Where the code differs from the text above:

- **`transition_duration` needs a bare-integer (de)serializer.** A derived
  `Option<u64>` with only `serde(default, skip_serializing_if)` does not work:
  RON without `implicit_some` rejects the bare `transition_duration: 1200` that
  per-mode cosmic-bg writes (`ExpectedOption`), and would write `Some(1200)`
  back, which cosmic-bg's plain `u64` rejects. `BgEntry` uses a private
  `bare_u64` serde `with` module; `new_shape_preserves_transition_duration`
  pins the on-disk shape against a plain-`u64` oracle.
- **`theme_mode` is opened only on a per-mode layout.** Nothing reads it on
  legacy, and opening it would create the theme-mode directory on a machine
  that has none. `open_targets_old_layout_opens_no_mode_configs` asserts that a
  legacy probe leaves only the shared config directory.
- **`old_layout_apply_creates_no_mode_dirs` is a Task 3 test** (it needs
  `apply_to`), next to `old_layout_apply_writes_shared_only`.
- **Extra tests:** `open_targets_new_layout_opens_dark_then_light` (handle
  order and rooting), `unreadable_entry_is_rebuilt_from_defaults`,
  `apply_fails_fast_on_a_failed_write`, `active_mode_per_output_is_unknown`,
  `missing_active_mode_config_is_unknown`.
- **The active mode falls back to shared when it has no `all` key** (added
  after the final review; `active_mode_without_entry_reads_shared`). Without
  it, mode directories with no entry — a per-mode cosmic-bg before seeding, or
  directories left on a legacy cosmic-bg — read as "no file": the daily apply
  stops, and the image legacy cosmic-bg displays loses its prune protection.
  A present but unreadable mode entry does not fall back.
- **`classify` decisions do change in one native case.** On a never-customised
  native desktop the read is now `NoFile` where it used to be
  `File(<distro default>)`, because the handles have no system path. This
  matches the Flatpak build and is benign.
- **The "two `cosmic-config` instances" rationale is stale.** `Cargo.lock`
  holds one `cosmic-config` package. The raw-key design stands for other
  reasons (the pinned `context()` cannot open mode configs or an injected
  path, and the pinned `Entry` is `deny_unknown_fields`); the comments in
  `src/wallpaper.rs` now say so.

### Open after the final review

- **Leftover mode directories on a legacy cosmic-bg are not harmless for
  reads.** This corrects the "Downgrade" and "Third-party directory creation"
  detection notes, which hold for apply only. When a mode config holds an
  entry that legacy cosmic-bg never updates (after a downgrade, or once
  Daymural has applied through third-party-created directories),
  `current_wallpaper` trusts it. A wallpaper later chosen in legacy Settings
  is then not seen by the do-not-clobber rule. The layout alone cannot tell
  this from a real per-mode system; cosmic-bg's `wallpapers` state key could
  arbitrate. Documented in `docs/limitations.md` with the workaround (delete
  the directories).
- **Prune protects only the active mode's file.** With a foreign wallpaper in
  the active mode and a Daymural image in the other, that image can be deleted
  once it ages out. Protecting every mode's file source needs changes in
  `src/app.rs`, which this plan froze. Documented in `docs/limitations.md`.
- **Background applies still set both modes** (accepted in Task 4). Together
  with the prune gap this argues for a follow-up that makes protection and
  background applies mode-aware and then reconciles the AGENTS.md
  do-not-clobber and prune invariants.
- **Manual smoke Steps 3–5 have not been run.** Per-mode behaviour is so far
  backed by hermetic tests and by reading upstream `2439903`, not by a real
  per-mode system.
