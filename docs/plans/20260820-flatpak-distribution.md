# Flatpak Distribution

## Overview

- Package `daymural` for distribution through `pop-os/cosmic-flatpak` while
  preserving the native `just install` route and full applet behavior.
- Follow the established Awake/weather packaging pattern: AppStream metadata, a developer
  manifest, offline Rust dependency vendoring, local Flatpak recipes, CI, and a pinned COSMIC
  Store submission.
- Preserve COSMIC system theming and live accent updates inside the sandbox. The applet's
  opt-in wallpaper-derived accent writer must retain all existing snapshot, restore, rollback,
  and do-not-clobber guarantees.
- Require behavior parity for Bing refresh, wallpaper application, history, thumbnails,
  `xdg-open` actions, theme/accent handling, and the lock-screen workaround before release.

## Context

- **Base branch:** implement on a branch cut from `main` **after** `single-instance-leader`
  merges. Four tasks edit `src/app.rs`; landing them on top of the in-flight leader branch
  guarantees conflicts and makes the multi-output gate unverifiable. The leader's multi-output
  behaviour is verified by that plan, not this one (see Post-Completion).
- **Files and components:** `data/` desktop/icon assets, `src/app.rs` identity tests,
  `src/wallpaper.rs` host Pictures and cosmic-bg integration, `src/accent.rs` COSMIC theme
  access, `src/lockwatch.rs` logind monitoring, `justfile`, `README.md`, `CLAUDE.md`,
  `AGENTS.md`, and new Flatpak, AppStream, vendoring, and CI files.
- **Existing patterns:** `APP_ID` is already shared by the desktop entry and applet config;
  the icon is already installed under the app-ID-prefixed name Flatpak can export. Tests embed
  declarative files and pin hand-written identity values. The source desktop entry currently
  names `/usr/bin/daymural`; the manifest installs that file verbatim into
  `/app/share/applications`, where that path does not exist (the binary is `/app/bin/…`), so it
  must become the bare command. `flatpak build-export` preserves that bare command in its export
  tree; the manifest command and `/app/bin` destination provide the matching launch target.
  Native `just install` keeps rewriting its
  installed copy with `sed 's|^Exec=.*|Exec={{bin-dst}}|'`, which matches either form; no
  existing test asserts an absolute source `Exec`.
- **Reference:** `~/workspace/cosmic-applet-awake` provides the local manifest, vendoring,
  AppStream, consistency-test, CI, and COSMIC Store publication pattern. The current
  `pop-os/cosmic-flatpak` weather applet uses Freedesktop runtime 25.08.
- **Dependencies:** retain the committed `Cargo.lock`, the pinned cosmic-bg revision, Rust
  stable SDK extension, and the vendored `flatpak-cargo-generator.py`; add no production **or
  dev** dependency merely for packaging (`serde_json` is already a production dependency and
  covers the manifest; workflows are checked by comment-stripped text scan, never a YAML dep).
- **⚠️ Pin blocker (Task 0):** `Cargo.lock` currently carries **two source ids** for the
  libcosmic repo — `git+…/libcosmic?rev=8a017a15…` (18 crates, from our `rev=` pin) and bare
  `git+…/libcosmic#8a017a15…` (5 crates via `cosmic-bg-config` → `cosmic-config`; see the
  trailing note in `Cargo.toml`). `flatpak-cargo-generator.py` keys its `[source]`
  replacements by canonical URL (query stripped) and merges by that key, so only one id is
  replaced and `cargo --offline fetch` in the sandbox hits the network and fails. The awake
  repo documents exactly this and deliberately drops `rev=` (the sha stays pinned by the
  committed `Cargo.lock`), guarded by
  `every_git_dependency_resolves_through_one_source_id_per_repo`. CLAUDE.md's "Both git deps
  are rev-pinned in `Cargo.toml`" must change with it.
- **Constraints:** use scoped permissions, not `home`, `host`, unrestricted D-Bus sockets, X11,
  or `--persist`. Preserve the existing `~/Pictures/BingWallpaper` contract and do not run the
  applet outside an intentional live verification session.

## Development Approach

- **Testing approach:** regular code/declarative-file changes followed immediately by tests.
- Complete each task before starting the next and keep changes small and focused.
- Unit tests cross-link the hand-written identity web (APP_ID ↔ desktop entry ↔ metainfo ↔
  manifest ↔ justfile ↔ scripts ↔ workflows ↔ `/app/bin`) and enforce the finish-args
  denylist. They do **not** re-implement `appstreamcli validate`/`desktop-file-validate`
  well-formedness checks, and they **never read generated or gitignored artifacts**
  (`cargo-sources.json`) — `just check` must pass on a fresh clone with no `uv` installed.
- Run each task's gate before continuing; run `just check` before handoff.
- Keep this plan synchronized during implementation and preserve unrelated working-tree changes.

## Testing Strategy

- **Unit/integration tests:** extend `src/app.rs`'s identity tests to parse the manifest
  (`serde_json`), embed AppStream/tooling/workflow files, and cross-check APP_ID, crate
  metadata, install paths, finish-args, vendoring inputs (script and manifest *name*
  `Cargo.lock`/`cargo-sources.json`), and CI commands.
- **External validation (shell/CI gates, not `cargo test`):** `appstreamcli validate`, compare
  generated Cargo sources with every registry/git source in `Cargo.lock`, prefetch Flatpak
  sources, then prove a clean build with `flatpak-builder --disable-download`.
- **End-to-end:** perform an intentional live COSMIC panel test because network access,
  cosmic-bg propagation, portal launches, theme watchers, logind signals, and the greeter cache
  workaround cannot be proven hermetically.
- **Full-suite command:** `just check`.
- **Tool prerequisites (manual/CI only, not part of `just check`):** `appstreamcli`, `uv`,
  `flatpak-builder`, and a user flathub remote
  (`flatpak remote-add --user --if-not-exists flathub …`) for `--install-deps-from=flathub`.

## Progress Tracking

- Mark completed work with `[x]` immediately.
- Add discovered scope with a `➕` prefix.
- Record blockers or deviations with a `⚠️` prefix.
- Keep this file synchronized with implementation.

## Solution Overview

Add a developer Flatpak manifest at the repository root and install the existing binary,
desktop entry, and app-ID icon alongside new AppStream metadata. Generate every Cargo source,
prefetch the declared Flatpak inputs, then prove a clean repeat build with downloads disabled.
Grant only the host integrations the existing implementation requires: network, Wayland/DRI,
the exact wallpaper directory, COSMIC config and cosmic-bg state, COSMIC Settings Daemon
notifications, and logind.

The applet's catalogue and thumbnails remain in Flatpak-private XDG state. Wallpaper images stay
at the existing host path so host `cosmic-bg` can read them; because
`Catalogue::load_or_rebuild` rescans `~/Pictures/BingWallpaper`, a native→Flatpak switch
re-derives the history with no re-downloads (a migration, not data loss). COSMIC config remains
host-visible because libcosmic reads theme/accent values there and this applet intentionally
writes applet, wallpaper, and opt-in accent settings there.

**Native and Flatpak installs are mutually exclusive.** The leader and coordination locks
(`src/leader.rs`, taken in `app::state_dir()`) are per-install private state while the
coordination mailbox lives in shared `xdg-config/cosmic`, so two installs means two leaders
racing an unserialized read-modify-write. Document it; uninstall the native copy before any
live Flatpak verification.

## Technical Details

The root manifest `io.github.ercling.cosmic-applet-daymural.json` will use:

- `runtime: org.freedesktop.Platform`, `runtime-version: 25.08`,
  `sdk: org.freedesktop.Sdk`, and `org.freedesktop.Sdk.Extension.rust-stable`;
- `command` and module name `daymural`;
- `CARGO_HOME=/run/build/daymural/cargo`;
- offline, lockfile-enforced Cargo fetch/build commands and explicit `/app` install destinations;
- a local directory source excluding `.git`, `target`, `examples`, `.flatpak-builder`, and
  `build-dir`, plus generated `cargo-sources.json`.

The source desktop entry will use `Exec=daymural` (see Context for why). The
Flatpak build export preserves that bare command; it matches the manifest command and resolves
to `/app/bin/daymural` in the sandbox. Native `just install` retains its existing
`sed` rewrite to the selected native installation path. Tests pin all four names together.

The exact sandbox contract is:

- `--socket=wayland` and `--device=dri` for the libcosmic renderer;
- `--share=network` for Bing metadata and UHD downloads;
- `--filesystem=~/Pictures/BingWallpaper:create` for the hardcoded image directory only;
- `--filesystem=xdg-config/cosmic:rw` for system theme values, applet settings, cosmic-bg
  config, and accent writes;
- `--filesystem=~/.local/state/cosmic:create` for the cosmic-bg state key used by the
  lock-screen poke. **`:create`, not `:rw`** — `poke_state_handle()` uses
  `Config::new_state`, which `create_dir_all`s its path; with `:rw` and no host
  `~/.local/state/cosmic` yet, flatpak skips the mount, sandbox `$HOME` is unwritable,
  `poke_state_handle()` returns `None`, warns once, and the cosmic-greeter#511 workaround is
  silently dead for the process' life. It only works with `:rw` on machines where the
  directory already exists (this one);
- `--talk-name=com.system76.CosmicSettingsDaemon` and
  `--talk-name=com.system76.CosmicSettingsDaemon.*` for live config/theme notifications;
- `--system-talk-name=org.freedesktop.login1` for session resolution and lock/resume signals.

Sandbox facts measured live against a flatpak 1.18 sandbox, to be recorded in README/CLAUDE.md:

- **PID namespace:** the applet runs as PID 2, so `lockwatch::resolve_session`'s primary
  `Manager.GetSessionByPID(std::process::id())` resolves host PID 2 (a kernel thread) →
  `NoSessionForPID`. The watch survives only through the `$XDG_SESSION_ID` →
  `Manager.GetSession` fallback (flatpak forwards `XDG_SESSION_ID`). If that variable is ever
  absent, `is_session_absence` parks the watch permanently. Task 7 must assert *which* path
  resolved (`RUST_LOG=daymural=debug` prints `resolved_via`).
- **Icons:** the 25.08 runtime ships **no** icon SVGs; the six themed names
  (`preferences-desktop-wallpaper-symbolic` in `app.rs`, five in `view.rs`) resolve only via
  flatpak's host passthrough `/run/host/share/icons` on `XDG_DATA_DIRS`. Contingency if it
  proves fragile: bundle the SVGs under `/app/share/icons/hicolor/scalable/` (awake embeds its
  icons in the binary for this reason).
- **Permission audit:** `flatpak info --show-permissions` reports filesystems flatpak adds
  itself (`xdg-config/gtk-3.0:ro`, `gtk-4.0:ro`, `kdeglobals:ro`, `xdg-data/color-schemes:ro`)
  and elides `:rw`. Reject-tests are scoped to the manifest's `finish-args`; the runtime audit
  is informative only.
- **State layout:** `app::state_dir()` uses `dirs::state_dir()` (`XDG_STATE_HOME`), which
  flatpak ≥ 1.13 sets to `~/.var/app/<id>/.local/state`. Older flatpaks fall back to
  `$HOME/.local/state/<APP_ID>`, which no grant covers → silent write failures. Document the
  1.13+ requirement; optionally `tracing::warn!` at startup when `FLATPAK_ID` is set and
  `XDG_STATE_HOME` is not.

Portal APIs are available under Flatpak's default policy, so `xdg-open` does not receive a
redundant explicit portal talk-name. The permission tests must reject `home`, `host`, session or
system bus sockets, X11, and every `--persist` entry.

AppStream metadata will identify `io.github.ercling.cosmic-applet-daymural`, launch the matching
desktop entry, provide `com.system76.CosmicApplet` and binary `daymural`, use
GPL-3.0-only/CC0-1.0 licensing, name the COSMIC project group, carry an OARS rating and release
entry, and use `https://github.com/ercling/cosmic-applet-daymural` as its homepage. Its
`<summary>` is pinned to `Cargo.toml`'s `description` (which must stay free of XML-special
characters). `appstreamcli validate` will warn about the missing `<screenshots>` until
Post-Completion; that warning is accepted.

## What Goes Where

- Implementation Steps contain repository changes, automated validation, and release-blocking
  live COSMIC acceptance checks.
- Post-Completion contains screenshot hosting, the external `pop-os/cosmic-flatpak`
  submission, and the multi-output leader verification owned by the single-instance-leader
  plan; those do not control when the implemented plan is archived.

## Implementation Steps

### Task 0: Collapse the libcosmic git source id

**Files:**

- Modify: `Cargo.toml`
- Modify: `Cargo.lock` (regenerated, same sha)
- Modify: `CLAUDE.md`, `AGENTS.md`
- Modify: `src/app.rs`

- [x] drop `rev=` from the `libcosmic` dependency so all 23 crates resolve through the single
      bare `git+https://github.com/pop-os/libcosmic#8a017a15…` id; the sha stays pinned by the
      committed `Cargo.lock` (verify `cargo update` was **not** run and the sha is unchanged)
- [x] port awake's `every_git_dependency_resolves_through_one_source_id_per_repo` test over
      the embedded `Cargo.lock`; add a negative assertion that a `?rev=` and bare id for one
      repo fails it
- [x] update CLAUDE.md/AGENTS.md: the pin lives in `Cargo.lock`, and why (`?rev=` splits the
      source id and breaks offline vendoring)
- [x] run `just check`; it must pass before Task 1

### Task 1: Add AppStream metadata and identity coverage

**Files:**

- Create: `data/io.github.ercling.cosmic-applet-daymural.metainfo.xml`
- Modify: `data/io.github.ercling.cosmic-applet-daymural.desktop`
- Modify: `src/app.rs`

- [x] change the source desktop entry to `Exec=daymural`; preserve the native
      justfile rewrite to an absolute installed binary path
- [x] add desktop-application metadata with the exact app ID, desktop launchable, applet
      category provide, binary, licenses, developer, project group, homepage, OARS rating,
      version, release date, summary (from `Cargo.toml`), and description
- [x] add success tests cross-checking APP_ID, Cargo name/version/license/description, desktop
      entry, icon, bare desktop command, metainfo filename, launchable, binary, and Store
      category
- [x] add failure assertions for an absolute or mismatched desktop `Exec` and mismatched
      identity/category fields (no generic well-formedness checks — `appstreamcli` owns those)
- [x] run focused identity tests and `appstreamcli validate
      --override cid-contains-uppercase-letter=error
      data/io.github.ercling.cosmic-applet-daymural.metainfo.xml` (screenshot warning accepted);
      both must pass before Task 2

### Task 2: Add the developer manifest and sandbox contract

**Files:**

- Create: `io.github.ercling.cosmic-applet-daymural.json`
- Modify: `src/app.rs`

- [x] add the Freedesktop 25.08/Rust SDK manifest, offline build, explicit install commands,
      local directory source, skip list, and `cargo-sources.json` input
- [x] add the exact scoped permissions defined in Technical Details (state dir `:create`)
- [x] add success tests for valid JSON, runtime/module/Cargo-home alignment, source/install paths,
      exported names, and every required capability; tie desktop `Exec`, manifest `command`,
      module name, and `/app/bin` destination together
- [x] add negative tests (scoped to `finish-args`) rejecting missing functional permissions,
      `:rw` on the state dir, broad filesystem, D-Bus, X11, persistence grants, or an exported
      desktop command that cannot resolve in `/app/bin`
- [x] run focused manifest tests, a Flatpak metadata parse, and inspect the exported desktop
      command; all must pass before Task 3

### Task 3: Add reproducible vendoring and local Flatpak recipes

**Files:**

- Create: `flatpak/flatpak-cargo-generator.py`
- Create: `flatpak/flatpak-cargo-generator.py.lock`
- Create: `flatpak/generate-cargo-sources.sh`
- Modify: `.gitignore`
- Modify: `justfile`
- Modify: `src/app.rs`

- [x] vendor the generator at a documented upstream commit and add the locked `uv` wrapper that
      turns committed `Cargo.lock` into gitignored `cargo-sources.json`
- [x] add `flatpak-sources`, `flatpak-build`, `flatpak-install`, and `flatpak-uninstall` without
      changing native recipes, sharing one `flatpak-builder-cmd` variable so they cannot drift;
      also add a source-prefetch recipe and a `flatpak-build-offline` recipe that passes
      `--disable-download`
- [x] add success tests tying manifest source names, module/Cargo-home paths, the script's
      `Cargo.lock` input / `cargo-sources.json` output names, and just recipes together —
      **never reading `cargo-sources.json` itself**
- [x] test the missing-`uv` error, scripts or recipes whose paths drift from the manifest, and
      an offline recipe lacking `--disable-download`
- [x] shell gate: generate `cargo-sources.json`, prove every registry/git source in `Cargo.lock`
      has a generated entry (including the single libcosmic id from Task 0), run the
      source-prefetch recipe, then a force-clean `flatpak-builder --disable-download` build
      using the retained download cache; all must pass before Task 4
- [x] ➕ harden cached git metadata extraction by fetching and checking out every requested
      lockfile commit, then comparing the full checked-out hash before reading `Cargo.toml`
- [x] ➕ force-clean the parent checkout and every current submodule, force indexed submodule
      commits, and reject dirty or mismatched submodule state before recursively scanning manifests
- [x] ➕ exclude `.git` metadata and directory symlinks from recursive manifest discovery,
      with adversarial coverage for both paths

### Task 4: Add Rust and Flatpak CI

**Files:**

- Create: `.github/workflows/rust.yml`
- Create: `.github/workflows/flatpak.yml`
- Modify: `src/app.rs`

- [x] add Rust formatting, Clippy-with-warnings-denied, and hermetic test jobs with required
      Wayland/xkbcommon and D-Bus packages (no `uv`, no generated sources — `cargo test` must
      not need them)
- [x] add a Freedesktop 25.08 Flatpak job that installs `uv`, generates Cargo sources, runs the
      `Cargo.lock` coverage check, builds the manifest, and emits a
      `daymural.flatpak` artifact
- [x] add success tests (comment-stripped text scan) pinning workflow manifest, generator,
      runtime image, bundle name, and equivalent Rust check commands
- [x] add negative assertions against runtime-version drift and omitted source generation
- [x] scan both workflows and run `just check`; both must pass before Task 5
- [x] ➕ pin the privileged Flatpak builder container by digest and all actions by full commit,
      restrict the workflow token to `contents: read`, and prevent checkout credential persistence
- [x] ➕ apply the same full-action pinning, read-only token, and non-persisted checkout
      credentials to native Rust CI

### Task 5: Document the distribution and compatibility contract

**Files:**

- Modify: `README.md`
- Modify: `CLAUDE.md`, `AGENTS.md`

- [x] document local Flatpak prerequisites (`flatpak-builder`, `uv`, `appstreamcli`, flathub
      user remote), build/install/uninstall commands, and the COSMIC Store channel
- [x] document scoped permissions, shared wallpaper/config locations, Flatpak-private
      catalogue/thumbnail state (and the rescan migration), the Flatpak 1.13+/standard COSMIC
      state-layout requirement, native/Flatpak mutual exclusivity, and the measured sandbox
      facts (PID namespace → `XDG_SESSION_ID` fallback, host icon passthrough, auto-added
      read-only filesystems)
- [x] retain the warning to disable accent matching before uninstall when automatic restoration
      is desired
- [x] CLAUDE.md/AGENTS.md: add the manifest, `flatpak/`, `.github/workflows/`, metainfo, and
      the embedded-declarative-file test convention to the architecture/test sections
- [x] verify documented commands and paths against the manifest/justfile tests
- [x] run documentation/identity tests; they must pass before Task 6

### Task 6: Verify live sandbox acceptance

**Files:**

- Modify as needed: `README.md`
- Modify as needed: `docs/plans/20260820-flatpak-distribution.md`

- [ ] precondition: native copy uninstalled (`just uninstall`); Flatpak is the only install
      (not yet run; requires an intentional live COSMIC acceptance session)
- [ ] install the built Flatpak intentionally in a live COSMIC session; confirm Panel discovers
      it and the exported desktop command starts `/app/bin/daymural`
      (not yet run)
- [ ] confirm the panel button, prev/next/newest/refresh, and empty-catalogue placeholder icons
      all render (host passthrough); if not, apply the bundling contingency
      (not yet run)
- [ ] verify Bing refresh, exact-directory downloads, thumbnails, browsing, shuffle, retention,
      cosmic-bg application, and both `xdg-open` actions (not yet run)
- [ ] change system light/dark mode and accent while the popup is open; verify all popup controls,
      dropdowns, and tooltips follow COSMIC colors without restart (not yet run)
- [ ] enable accent matching and verify both theme modes update; verify disable restores the
      snapshot and an external accent change disarms without being overwritten
      (not yet run)
- [ ] with `RUST_LOG=daymural=debug`: confirm `resolved_via` is the
      `XDG_SESSION_ID` path, login1 lock and resume signals cross `xdg-dbus-proxy`, and the
      cosmic-bg state poke lands as a **changed value**; greeter healing is best-effort here
      (GDM is this machine's DM); treat a missing poke as release-blocking
      (not yet run)
- [ ] audit `flatpak info --show-permissions`, `flatpak run --log-session-bus`, filesystem access,
      and `flatpak run --log-system-bus`; reject denied required calls or unexpected services,
      reading flatpak's auto-added read-only filesystems as expected
      (not yet run)
- [ ] record results and any verified channel differences; every live gate must pass before
      Task 7

### Task 7: Final acceptance and plan state

**Files:**

- Modify: `README.md`
- Move after Tasks 0-6 and all repository acceptance checks are complete:
  `docs/plans/20260820-flatpak-distribution.md` to `docs/plans/completed/`

- [x] run `just check`, `appstreamcli validate`, regenerate `cargo-sources.json`, the
      `Cargo.lock` coverage check, source prefetch, and a force-clean `--disable-download` build
- [x] inspect exported desktop, metainfo, icon, command, and permissions; confirm the exported
      desktop entry's bare command matches the manifest command installed under `/app/bin`
- [ ] verify every Overview requirement; record verified compatibility requirements, intentional
      channel differences, and any deviation in this plan and README (blocked on Task 6's live
      COSMIC parity audit)
- [x] confirm the native installation instructions and behavior remain accurate
- [ ] move the plan to `docs/plans/completed/`; external publication remains explicitly tracked
      below and does not make repository implementation status ambiguous

Task 7 repository acceptance (2026-08-21): `just check` passed 374 tests; AppStream structural
validation passed with `--no-net` (the network-enabled homepage probe returns
`url-not-reachable` until the external repository is public); regenerated Flatpak sources covered
all 713 sourced lockfile packages; source prefetch and a force-clean `--disable-download` build
passed. The built `/app` tree and export contain the executable, desktop entry, AppStream metadata,
symbolic icon, and exact scoped permissions. Contrary to the earlier design note, measured
`flatpak build-export` output preserves `Exec=daymural` instead of rewriting it; the
name is nevertheless identical to the manifest command and `/app/bin` executable. A staged native
install under `/tmp` confirmed the documented binary, desktop, icon, and absolute installed `Exec`
rewrite. Native and Flatpak state-layout differences remain intentional and documented. This is
repository validation only: Task 6's live COSMIC checks remain unobserved release gates, so the
plan stays active until they pass.

## Post-Completion

### Follow-ups owned elsewhere

- Multi-output verification in the sandbox — only leader-owned fetch, prune, wallpaper,
  lock-poke, and accent effects — belongs to the single-instance-leader plan's acceptance once
  both land.

### External updates

- Publish/tag the exact tested commit at `https://github.com/ercling/cosmic-applet-daymural`.
- Capture and host a COSMIC screenshot, add it to AppStream, and re-run validation (clears the
  accepted screenshot warning).
- Replace the developer directory source with the pinned release commit and submit the manifest
  plus generated `cargo-sources.json` under
  `app/io.github.ercling.cosmic-applet-daymural/` in `pop-os/cosmic-flatpak`.
- Repeat the clean build and live sandbox audit against the exact submission commit.
