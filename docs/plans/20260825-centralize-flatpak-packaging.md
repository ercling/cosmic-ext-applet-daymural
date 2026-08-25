# Centralize Flatpak Packaging

## Overview

- Move the developer Flatpak manifest, vendoring tools, helper tests, lockfile, and generated Cargo source list into `packaging/flatpak/`.
- Preserve all `just flatpak-*` commands, Flatpak identity and permissions, dependency pins, CI behavior, bundle name, and root build-cache locations.
- Keep AppStream metadata in `data/` and the workflow in `.github/workflows/`, where they serve application metadata and GitHub integration rather than the packaging bundle itself.

## Context

- **Pre-change layout (before 2026-08-25):** the manifest was at the repository root, tooling was under `flatpak/`, and generated `cargo-sources.json` was at the root. `justfile`, `.github/workflows/flatpak.yml`, and compile-time tests in `src/app.rs` embedded those paths.
- **Target layout:** the manifest and tracked tooling move together under `packaging/flatpak/`; future source generation writes `packaging/flatpak/cargo-sources.json` beside the manifest.
- **Path model:** the canonical manifest resolves its repository source through `../..`; the vendoring wrapper resolves both its own directory and the repository root, so it remains callable from any working directory. Local recipes generate an ignored root-level manifest view whose two source paths are root-relative, allowing Flatpak Builder's `--sandbox` hardening to remain enforced without duplicating the canonical manifest.
- **Constraints:** this is a path-only refactor. Add no dependencies, do not alter runtime behavior or sandbox permissions, and do not run the applet.

## Development Approach

- **Testing approach:** regular code-then-tests.
- Apply the physical move and every executable path consumer atomically so Rust's compile-time `include_str!` references never form a task boundary in a broken state.
- Preserve existing success and deliberate-drift coverage, adding only the hermetic wrapper-path case needed by the new layout.
- Complete each task and its test gate before continuing.

## Testing Strategy

- **Python helper tests:** run the relocated Git traversal suite directly.
- **Rust contract tests:** verify the new coherent layout, arbitrary-working-directory wrapper behavior, missing-`uv` handling, old/mixed path rejection, manifest behavior, and CI security pins.
- **Packaging acceptance:** regenerate sources, prefetch inputs, then perform a force-clean build with downloads disabled.
- **Full-suite command:** `just check`.

## External Review Authorization

- Decision: authorized
- Reviewer: automatic Codex at /home/linuxbrew/.linuxbrew/Caskroom/codex/0.148.0/bin/codex; arguments: exec --sandbox read-only -c model=gpt-5.5 -c model_reasoning_effort=xhigh -c stream_idle_timeout_ms=3600000; credential profile: codex
- Command fingerprint: ffecdfa99eff4a845267a98f22901a747ccf2b6e8996008c9ba59eef8d6f86d1
- Data scope: repository diff, complete repository checkout including tracked, untracked, and ignored files, plan, and progress log
- Purpose: read-only implementation review
- Applies to: this plan execution only

## Progress Tracking

- Mark completed work with `[x]` immediately.
- Add discovered scope with a `➕` prefix.
- Record blockers or deviations with a `⚠️` prefix.
- Keep this file synchronized with implementation.

## Implementation Steps

### Task 1: Relocate the bundle and update all executable path contracts

**Files:**

- Move: `io.github.ercling.cosmic-applet-daymural.json` to `packaging/flatpak/io.github.ercling.cosmic-applet-daymural.json`
- Move: tracked files under `flatpak/` to `packaging/flatpak/`
- Modify: relocated manifest and `generate-cargo-sources.sh`, `.gitignore`, `justfile`, `.github/workflows/flatpak.yml`, `src/app.rs`

- [x] Move only tracked packaging sources; leave existing ignored root `cargo-sources.json`, Python caches, `.flatpak-builder/`, and `build-dir/` untouched.
- [x] Change the manifest's directory source from `./` to `../..`, retaining the adjacent `"cargo-sources.json"` entry and all build, identity, and sandbox fields.
- [x] Make the wrapper derive an absolute script directory and repository root, read root `Cargo.lock`, invoke the adjacent pinned generator, and write adjacent `cargo-sources.json`.
- [x] Ignore `packaging/flatpak/cargo-sources.json` and its Python cache. Retain legacy ignores so existing generated files do not surface as unrelated changes.
- [x] Point every `just flatpak-*` builder recipe at `packaging/flatpak/<app-id>.json` and `flatpak-sources` at the relocated wrapper; preserve recipe names, the shared builder command, and root build directories.
- [x] Update CI's scanner command, generator command, generated-file validation path, and builder action manifest path without changing triggers, permissions, action/image pins, AppStream validation, or bundle identity.
- [x] Update all packaging `include_str!` paths, filesystem assertions, helper invocations, workflow expectations, and deliberate path-drift mutations in `src/app.rs`. Represent the manifest-local filename and repository-relative generated-file path separately.
- [x] Add a hermetic success test that invokes the wrapper from a `TempDir` with a fake `uv`, then asserts its working directory and arguments identify root `Cargo.lock`, the adjacent generator, and the adjacent output without network or real-user access.
- [x] Preserve the hermetic missing-`uv` failure test and the existing scanner tests for `.git` exclusion and directory-symlink exclusion.
- [x] Add or update negative contract cases rejecting the old root manifest, old `flatpak/` paths, mixed old/new layouts, and a wrong generated-output path.
- [x] Run `python3 packaging/flatpak/test_git_manifest_scan.py`.
- [x] Run focused Rust gates: `cargo test flatpak`, `cargo test cargo_sources_script`, and `cargo test ci_workflow` with the repository's required `PKG_CONFIG_PATH`; all must pass before Task 2.

### Task 2: Synchronize documentation and run the repository gate

**Files:**

- Modify: `README.md`, `AGENTS.md`, `CLAUDE.md`, `docs/plans/20260820-flatpak-distribution.md`

- [x] Update user-facing manifest and generated-source paths while preserving all documented `just` commands.
- [x] Replace the repository-map descriptions of the root manifest and `flatpak/` directory with the consolidated packaging directory.
- [x] Update durable architecture references in `CLAUDE.md`.
- [x] Add a current-layout note to the original Flatpak distribution plan and update its normative layout descriptions; retain completed task history where old paths are historical evidence.
- [x] Search active source, workflow, recipes, and current documentation for stale root-manifest or `flatpak/` tooling references; allow only action identifiers, historical evidence, and legacy-ignore comments.
- [x] Run `just check`; it must pass before Task 3.

### Task 3: Verify Flatpak acceptance and finalize the plan

- [x] Run `just flatpak-sources`; confirm it creates or updates `packaging/flatpak/cargo-sources.json` and does not read or modify a legacy root copy.
- [x] Run `just flatpak-prefetch` to populate the retained Flatpak source cache through the relocated manifest.
- [x] Run `just flatpak-build-offline` to prove a force-clean build succeeds without downloads.
- [x] Run final `just check`.
- [x] Inspect `git status` and confirm generated artifacts remain ignored and no unrelated files changed.
- [x] Mark this plan complete (archive deferred to the orchestrator; plan intentionally left in place).

### Review corrections

- [x] Preserve a disabled persisted accent snapshot during initial config confirmation so a later enable can retry the deferred restore without replacing the user's original accents.
- [x] Let a complete initial accent lifecycle repair any independently defaulted accent key before startup confirmation, preserving disable-time restoration.
- [x] Restore `flatpak-builder --sandbox` for every local build recipe by atomically staging a root-relative manifest view from the canonical `packaging/flatpak/` manifest.
- [x] Add hermetic regression coverage for both recovery paths and rerun the repository and Flatpak validation gates.
- [x] Reject a symlink at the ignored staged-manifest destination before atomic publication so local Flatpak recipes cannot overwrite its target.
- [x] Defer every startup accent compute entry point until fresh config confirmation has repaired any independently defaulted lifecycle key.
- [x] Resolve the startup confirmation gate from fresh takeover hydration, and route a confirmed matching raw enabled flag through the normal reversible enable lifecycle.
- [x] Centralize startup accent recovery in a table-tested pure classifier shared by config confirmation and takeover hydration.
- [x] Make local manifest staging consume the shared canonical and staged manifest path variables directly.

## Public Interfaces

- No Rust API, configuration schema, application behavior, or user-facing command changes.
- Direct repository consumers must use `packaging/flatpak/io.github.ercling.cosmic-applet-daymural.json` and `packaging/flatpak/cargo-sources.json`.

## Assumptions

- Existing ignored root packaging artifacts may remain locally but are inert and are not deleted by this refactor.
- Root `.flatpak-builder/` and `build-dir/` locations remain unchanged.
- The ignored root `.daymural-flatpak-manifest.json` is generated for local builds only; the canonical source and direct-consumer interface remain under `packaging/flatpak/`.
- The selected code-then-tests approach is intentional; the atomic first task prevents an unbuildable handoff between tasks.
- No live COSMIC panel test is required because application behavior and the sandbox contract do not change.

## Post-Completion

### External updates

- Use the relocated manifest path in any future downstream COSMIC Store submission or packaging documentation outside this repository.
