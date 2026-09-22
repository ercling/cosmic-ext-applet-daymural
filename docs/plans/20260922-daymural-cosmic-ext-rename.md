# Rename Daymural’s COSMIC integration to cosmic-ext

## Overview

Resolve jackpot51's September 19, 2026 changes-requested review on
[cosmic-flatpak PR #298](https://github.com/pop-os/cosmic-flatpak/pull/298#pullrequestreview-5255775195)
by using `cosmic-ext-applet-daymural` for the repository and public integration
identity, including App ID `io.github.ercling.cosmic-ext-applet-daymural`.
Retain the display name **Daymural**, executable and Cargo package `daymural`,
and the existing internal storage namespace.

The reviewer explicitly requests the cosmic-ext prefix. The referenced
[policy](https://github.com/pop-os/cosmic-epoch/blob/master/TRADEMARK.md)
reserves the cosmic- package namespace for official software and encourages
cosmic-ext- for third-party integrations. Reviewer acceptance remains an
external gate; this plan does not claim legal clearance.

## Context

- **Files and components:** `src/app.rs`, `src/config.rs`, `src/leader.rs`,
  Cargo metadata, `data/`, `packaging/flatpak/`, `justfile`, workflows, and
  installation documentation. The separate store PR contains a manifest,
  generated sources, and `store-metadata.patch` under its current App ID.
- **Existing patterns:** `APP_ID` currently controls application identity,
  config contexts, watcher subscriptions, and the state-directory suffix.
  Packaging contracts and deliberate-drift tests live in `src/app.rs`.
- **Dependencies:** retain all dependency pins, Rust edition, sandbox grants,
  Fluent IDs/domain, Cargo package, module name, and build-directory conventions.
- **Constraints:** preserve settings, catalogue/history, wallpapers, accent
  snapshot/last-written state, and leader/mailbox semantics. Do not launch the
  applet or touch live user data in tests. Preserve unrelated work.
- **Design references:** read `docs/plans/completed/20260817-single-instance-leader.md`
  before coordination changes, and the accent design and notes dated 20260808
  before touching accent-related persistence. The 20260822 Daymural rename's
  deliberate data loss is historical and does not apply to this change.

## Development Approach

- **Testing approach:** regular code-then-tests, as selected by the user.
- Complete each task and its test gate before starting the next.
- Keep changes focused; add success and failure coverage for changed behavior.
- Preserve backward compatibility through the existing storage namespace.
- Implementation and external updates are not authorized by writing this plan.

## Testing Strategy

- Rust tests use injected paths and `Config::with_custom_path` under TempDir.
  Cover old data access, config/watch identity agreement, shared leadership,
  packaging consistency, and deliberate identity drift.
- Use the existing packaging helper tests and format-specific validators.
- Test upgrade documentation with the existing `include_str!` contract pattern:
  check ordered steps, exact paths, conflict handling, and backup retention.
  Do not extract and execute shell examples containing real user paths. No
  migration helper or coreutils failure tests are needed for this documentation task.
- Full-suite command: `just check`. Raw Cargo commands require
  `PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig`.
- Live panel and Flatpak upgrade acceptance is attended post-completion work.

## Progress Tracking

- Mark completed work with `[x]` immediately.
- Add discovered scope with a `➕` prefix.
- Record blockers or deviations with a `⚠️` prefix.
- Keep this file synchronized with implementation.

## Solution Overview

Separate the public application identifier from the persistent storage
identifier. Change `APP_ID` for Wayland/application identity and installed
assets. Introduce a documented `STORAGE_ID` equal to
`io.github.ercling.cosmic-applet-daymural`, used consistently by applet config,
coordination config, their watcher subscriptions, and the state-path suffix.
This avoids copying native settings and preserves accent recovery records.

Native upgrades require no data transfer. Under Flatpak, the locked
cosmic-config `get_config_dir()` uses `HOST_XDG_CONFIG_HOME`, falling back to
`$HOME/.config`: settings, accent snapshots/last-written values, and the
coordination mailbox already use shared host configuration. Retaining
`STORAGE_ID` preserves these without copying any configuration.

Only the applet's direct `dirs::state_dir()` resolver follows Flatpak's private
`XDG_STATE_HOME`. Document an offline host-side transfer of `catalogue.json`
and `thumbs/` (including sidecars and failed-decode records) between the exact
private state roots below. Stop old/new instances and back up first. Do not
copy any `config/` subtree or lock files, overwrite an existing destination
silently, or widen sandbox permissions. Stop on a conflict or failed transfer;
do not launch against a partially transferred state. Keep old data and backups.
Old/new sandboxes do not share leadership merely because their suffix matches.

## Technical Details

- `AppletConfig::context`, `CoordinationConfig::context`, and both
  `watch_config` subscriptions must use the same storage identifier.
- `Window`'s application identity and installed desktop/AppStream/icon IDs use
  the new public identifier. In locked libcosmic `8a017a1`,
  `src/app/mod.rs` sets `iced.id` and the platform application ID from APP_ID.
  The applet uses `cosmic::applet::run`, not `run_single_instance`; keep this
  distinction when auditing consumers. Its two explicit config subscriptions
  must instead use STORAGE_ID.
- Default paths (respect effective XDG overrides when documenting upgrades):
  shared native/Flatpak config is
  `~/.config/cosmic/io.github.ercling.cosmic-applet-daymural/`;
  native state is `~/.local/state/io.github.ercling.cosmic-applet-daymural/`.
  Both remain unchanged. Old Flatpak private state is
  `~/.var/app/io.github.ercling.cosmic-applet-daymural/.local/state/io.github.ercling.cosmic-applet-daymural/`;
  new Flatpak private state is
  `~/.var/app/io.github.ercling.cosmic-ext-applet-daymural/.local/state/io.github.ercling.cosmic-applet-daymural/`.
  Only the outer App ID changes. See the pinned cosmic-config resolver and
  `docs/installation.md` data locations, also documented by the completed
  Flatpak distribution plan.
- Preserve the existing state directory's catalogue, thumbnail identities,
  decode-failure records, and leader-lock location for unchanged host roots.
- Keep `~/Pictures/BingWallpaper` unchanged and preserve the applied wallpaper.
- Update active repository/homepage links to
  `https://github.com/ercling/cosmic-ext-applet-daymural`; the actual GitHub
  repository rename is a manual external step. Preserve historical plans.
- Existing panel entries refer to the old desktop ID; document removing and
  re-adding Daymural instead of automatically rewriting panel configuration.
- Old identifiers remain legitimate only for internal storage compatibility,
  upgrade instructions/tests, and historical evidence.
- Preserve the existing leader-lock fail-open policy on open/acquisition errors
  (`src/leader.rs` and the completed leader plan). Changing this policy is
  unrelated behavior and is not part of this rename.
- Claude must perform the external implementation review with the exact
  identity below. Configure the project reviewer before execution and verify
  the resolver fingerprint; a mismatch requires renewed authorization.

## External Review Authorization

- Decision: authorized
- Reviewer: configured claude at /home/ercling/.local/share/claude/versions/2.1.275; arguments: --print --permission-mode plan --tools Read,Glob,Grep --allowedTools Read,Glob,Grep --strict-mcp-config --setting-sources ''; credential profile: claude
- Command fingerprint: 5c5845161d8cc8b371fcda77b3c6cdb10ba11b5259720df9e445f21cee8792c9
- Data scope: repository diff, complete repository checkout including tracked, untracked, and ignored files, plan, and progress log
- Purpose: read-only implementation review
- Applies to: this plan execution only

## What Goes Where

- Implementation Steps contain repository changes as checkboxes.
- Post-Completion contains manual or external work without checkboxes.

## Implementation Steps

### Task 1: Separate persistent storage identity from public identity

**Files:** modify `src/app.rs`, `src/config.rs`; retain and run existing
`src/leader.rs` tests. Keep the public ID unchanged in this task so the
existing packaging contract remains green.

- [x] Introduce `STORAGE_ID` with the current ID; route config contexts, both
      watchers, and state-path construction through it. Keep path decisions
      pure and injectable rather than changing global environment in tests.
- [x] Use the same storage-ID accessor/constant in production and injected
      test contexts. Add source-contract guards over the two config factories
      and both subscription sites to reject use of APP_ID for persistence or
      watching; test deliberate APP_ID substitutions so guards can fail.
      Assert STORAGE_ID's exact old value. Defer `APP_ID != STORAGE_ID` until
      Task 2, since the two intentionally remain equal during Task 1.
- [x] Audit all runtime APP_ID consumers against the locked libcosmic source;
      explicitly distinguish runtime/application identity from durable storage.
- [x] Success tests: populate old settings and state under TempDir; verify
      settings, catalogue, thumbnail identity, accent snapshot/last-written
      fields and mailbox remain accessible without reinitialization. Run the
      existing `one_leader_wins_and_loser_takes_over_once` test rather than
      duplicate it.
- [x] Failure tests: missing/corrupt config retains established per-key behavior;
      retain the deterministic file-as-directory tests for existing fail-open
      lock behavior. Identity guards reject public-ID storage consumers and
      tests never fall back to real user paths.
- [x] Test gate: `just check` must pass before Task 2.

Task 1 validation: `just check` passed (449 tests; formatting and clippy clean).
The installed Flatpak SDK Rust 1.98.1 bin directory was prepended to PATH
because the local rustfmt/clippy binaries still target the removed 1.97.1
driver. `CC=/usr/bin/gcc CXX=/usr/bin/g++` bypassed read-only ccache; the
check ran outside the filesystem/network sandbox to permit loopback mock
servers. No dependencies or machine configuration were changed.

Task 1 audit evidence: locked libcosmic `8a017a1` routes
`cosmic::applet::run` through `iced_settings`, which uses the application's
`APP_ID` for `iced.id` and the Linux platform application ID
(`src/app/mod.rs:67-70`). Its separate `run_single_instance` D-Bus identity
path is not used by this applet. Remaining runtime public-ID consumers are
`Window::APP_ID` and the startup log in `src/main.rs`. Both explicit config
watchers, both config factories, injected config contexts, and the state-root
suffix now use `STORAGE_ID`; no leader or accent state-machine behavior changes.

### Task 2: Rename the public integration and its packaging atomically

**Files:** modify `src/app.rs`, `Cargo.toml`, `justfile`,
`.github/workflows/flatpak.yml`, `docs/packaging.md`, and other workflow identity
consumers found by search. Leave `src/bing.rs`'s `daymural/` user agent unchanged.
Keep `src/catalogue.rs`'s old state-path comment accurate by documenting that it
uses the retained storage namespace. Rename and update:

- `data/io.github.ercling.cosmic-applet-daymural.desktop` to
  `data/io.github.ercling.cosmic-ext-applet-daymural.desktop`
- `data/io.github.ercling.cosmic-applet-daymural.metainfo.xml` to
  `data/io.github.ercling.cosmic-ext-applet-daymural.metainfo.xml`
- `data/icons/io.github.ercling.cosmic-applet-daymural-symbolic.svg` to
  `data/icons/io.github.ercling.cosmic-ext-applet-daymural-symbolic.svg`
- `packaging/flatpak/io.github.ercling.cosmic-applet-daymural.json` to
  `packaging/flatpak/io.github.ercling.cosmic-ext-applet-daymural.json`

- [x] Set public APP_ID to the new identifier; retain STORAGE_ID unchanged.
      Update every installed identity, filename, embedded include, install
      command, workflow artifact, homepage expectation and active URL consumer
      in the same task. Do not rename `daymural` or its Fluent resources.
- [x] Preserve all manifest permissions, runtime/SDK pins, generated-source
      layout, module name, CARGO_HOME, and dependency lockfile contents.
- [x] Success tests: coherent new public identity passes native and Flatpak
      contracts while old storage remains readable after the public ID changes.
      Assert `APP_ID != STORAGE_ID`; run Task 1's production-site guards under
      the now-distinct IDs. Add `docs/packaging.md` to documentation identity
      coverage, including a deliberate stale-ID mutation.
- [x] Failure tests: reject mixed old/new desktop, icon, manifest, workflow,
      metainfo and install identities; ensure retained storage references are
      explicitly distinguished from accidental public-identity leftovers.
      Use narrowly scoped compatibility sections/constant assertions rather
      than exempting whole files from identity checks.
- [x] Test gate: `just check`, `python3 packaging/flatpak/test_git_manifest_scan.py`,
      `desktop-file-validate data/io.github.ercling.cosmic-ext-applet-daymural.desktop`,
      and `appstreamcli validate --pedantic --explain --strict --no-net --override cid-contains-uppercase-letter=error data/io.github.ercling.cosmic-ext-applet-daymural.metainfo.xml`
      must pass before Task 3.

Task 2 validation: `just check` passed (450 tests; formatting and clippy clean),
using the same SDK toolchain and compiler environment recorded for Task 1.
The packaging Python helper test, desktop-file validator, and strict no-network
AppStream validator passed. Desktop validation emitted only the existing
COSMIC category hint. Public assets and active repository URLs now use the
cosmic-ext identity; Cargo.lock, runtime/SDK pins, sandbox permissions, binary,
Fluent domain, module/build paths, and the storage namespace are unchanged.

### Task 3: Document and verify a data-preserving upgrade

**Files:** modify `README.md`, `docs/installation.md`, `AGENTS.md`, and
documentation-contract tests in `src/app.rs`.

- [ ] Document public/storage identity separation, retained native data, old
      desktop/icon removal, new install commands, panel re-addition, and rollback.
- [ ] Add a new sibling upgrade section; preserve the historical
      `Upgrading from the previous applet identity` section and its guarded
      warning about the older CosmicBingWallpaper rename. Its no-migration
      behavior does not apply to this rename.
- [ ] Before native installation, explicitly remove the old
      `$HOME/.local/share/applications/io.github.ercling.cosmic-applet-daymural.desktop`
      and `$HOME/.local/share/icons/hicolor/scalable/apps/io.github.ercling.cosmic-applet-daymural-symbolic.svg`.
      Do not rely on the new `just uninstall` to remove old-ID assets, and do
      not remove the unchanged `daymural` binary after installing its replacement.
- [ ] Document the concrete shared/private paths in Technical Details. Native
      data and shared COSMIC configuration need no transfer. Explicitly forbid
      copying sandbox-local configuration over host settings or accent records.
- [ ] Add an AGENTS.md invariant: APP_ID names public integration; STORAGE_ID
      names applet/coordination config, watchers and applet state. Future public
      renames must not silently change durable storage identity.
- [ ] Provide ordered Flatpak upgrade steps with old/new instances stopped,
      backup before changes, transfer of private durable state before first
      launch, explicit conflict handling, and retention of the backup/old data.
      Transfer only catalogue.json and thumbs/; exclude locks and config.
- [ ] Success tests: documentation contracts verify exact old/new paths,
      stop/back-up/transfer/launch ordering, the transfer allowlist, unchanged
      shared settings, explicit old native asset removal and retained backups.
- [ ] Failure tests: deliberate documentation mutations omitting the conflict
      stop/non-clobber safeguards, admitting config or lock copying, deleting
      backups, or reversing launch/transfer ordering fail the contracts. Test
      the documentation strings, not shell commands or coreutils failures.
- [ ] Test gate: documentation-contract tests and `just check` must pass before Task 4.

### Task 4: Verify acceptance and prepare the store handoff

**Files:** update this plan with evidence and a concrete external handoff;
create `docs/plans/20260922-daymural-cosmic-ext-store-handoff.md`.

- [ ] Verify settings/history/accent recovery preservation, unchanged wallpaper
      paths, correct public identities and unchanged dependencies/permissions.
- [ ] Run `just check`; run `just flatpak-sources`, `just flatpak-prefetch`, then
      `just flatpak-build-offline` when prerequisites are available. These build
      gates must not install or start the applet. Record any unavailable gate
      as blocked, never as passed.
- [ ] Produce the exact store file/path changes listed below, preserving PR
      metadata improvements and describing how to rebase its patch and pin the
      final published upstream commit. Do not invent a future commit hash.
- [ ] Confirm Claude review configuration matches the authorized fingerprint
      before external review; record review findings and their disposition.
- [ ] Test gate: all automated checks above pass and handoff matches the final
      upstream patch. Attended checks remain explicitly pending if not performed.

### Task 5: Finalize documentation and execution records

**Files:** `README.md`, `docs/installation.md`, this plan and its store handoff.

- [ ] Cross-check upgrade guidance against the tested implementation; record
      deviations and any unavailable build or manual acceptance gates.
- [ ] Verify active links/names are consistent, allowing explicit compatibility
      references and historical plans; rerun `just check` after any code or
      embedded-documentation-contract change.
- [ ] Archive the plan under `docs/plans/completed/` through the execution
      workflow only after required implementation/review gates are satisfied;
      archive its store-handoff companion alongside it and update links.
      Retain external follow-ups without marking them performed.

## Plan Review Disposition

Claude's first review returned NEEDS REVISION. This revision incorporates the
verified shared-config/private-state distinction, concrete paths, production
storage-identity guards, missing packaging documentation, separate historical
upgrade guidance, old native asset cleanup, durable-identity guidance and the
handoff lifecycle. A local cross-check also corrected the proposed fail-closed
lock test to preserve the repository's deliberate fail-open behavior.

The earlier plan did not explicitly require copying private configuration or
executing examples against real paths; those review severity claims were
overstated. This revision nevertheless removes the ambiguities and avoids
adding migration machinery or testing coreutils. Unverified suggestions about
mandatory AppStream replacement metadata or renaming the `daymural` package
are not adopted. This revised plan has not yet received a second Claude review;
use the user-requested 1,200-second timeout for that review.

## Post-Completion

### Manual verification

- In an attended session, stop old instances, back up data and follow the
  documented native or Flatpak upgrade. Re-add the panel entry; verify saved
  settings/history, wallpaper selection, accent disable/restore and multi-output
  ownership. Do not perform this against the user's environment during tests.
- Verify old/new Flatpak instances are not running concurrently; different
  private state roots cannot be assumed to share a leader lock.

### External updates

- Rename GitHub repository to `ercling/cosmic-ext-applet-daymural`, update the
  local remote, publish the tested upstream commit, and verify links/redirects.
- In `pop-os/cosmic-flatpak` PR #298, rename
  `app/io.github.ercling.cosmic-applet-daymural/` to
  `app/io.github.ercling.cosmic-ext-applet-daymural/`, including its manifest
  basename. Update manifest ID, asset install paths, source URL and pinned commit.
- Rebase `store-metadata.patch` onto that exact commit: retain its improved
  summary/description, developer name, screenshot and associated test changes;
  update renamed file paths, launchable and repository links. Keep screenshot
  and help references pinned to real published commits.
- Retain `cargo-sources.json` if dependency inputs are unchanged; verify it
  remains consistent with the upstream lockfile. Run the store repository's
  required checks/build before publishing the PR update.
- Update the PR description's App ID, source link and commit. Send a concise
  response to jackpot51 only when explicitly instructed; obtain reviewer
  acceptance of the new namespace. Publishing and messaging are separate work.
