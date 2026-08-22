# Daymural Product Rename

## Overview

Rename the product to **Daymural**: repository `cosmic-applet-daymural`,
application ID `io.github.ercling.cosmic-applet-daymural`, executable
`daymural`. Then submit to the Store.

This is split out of `20260822-store-ready-applet-hardening.md` and starts
only after that plan is in `docs/plans/completed/`: the rename has no
technical dependency on the behavior work, rewrites nearly every declarative
file plus large stretches of the `src/app.rs` test module that the behavior
tasks also edit, and is the least reversible change in the set.

## Context

- **Identity web:** `src/app.rs`, Cargo metadata, `data/`, the root Flatpak
  manifest, the justfile, workflows, Fluent resource domains, and the
  embedded packaging tests cross-check one another. `expected_binary =
  env!("CARGO_PKG_NAME")` (`src/app.rs:5700,5816`) makes the binary name
  self-driving; the rest is literal and listed in Task 2.
- **Hidden rename targets** (each breaks silently if missed):
  - `io.github.ercling.CosmicBingWallpaper.json:24` — `"CARGO_HOME":
    "/run/build/cosmic-bing-wallpaper/cargo"`. flatpak-builder roots the build
    at `/run/build/<module-name>`; if `CARGO_HOME` drifts from the module
    name, the sandboxed `cargo --offline fetch` breaks and only
    `just flatpak-build-offline` catches it.
  - `src/app.rs:5791` — the homepage URL literal
    `https://github.com/ercling/cosmic-wallpaper-applet`.
  - `.github/workflows/flatpak.yml:88,93,94` and the literal workflow
    expectations at `src/app.rs:6086-6098`, `6114-6115`, `6440-6449`,
    `6650-6674`.
- **AppStream:** `appstreamcli validate --pedantic` emits
  `P: cid-contains-uppercase-letter` for the current ID and exits 0; the
  lowercase-hyphen ID validates with zero hints (verified 2026-08-22).
- **Leader lock is APP_ID-scoped:** `state_dir()` is `~/.local/state/<APP_ID>`
  (`src/app.rs:53-69`), so `leader.lock` does not cross identities. An old
  and a new install running together are *both* leaders over the same
  `~/Pictures/BingWallpaper`, pruning and applying concurrently.
- **Panel entry:** renaming the desktop-entry ID orphans the applet's entry
  in the existing cosmic-panel configuration; users must re-add it.
- **Compatibility:** this is an unpublished `0.1.0` pre-release. The identity
  change intentionally does not migrate old config/state, but preserves
  `~/Pictures/BingWallpaper`.

## Development Approach

- Mechanical, single-branch, one commit per task so the identity tests
  bisect cleanly.
- Message IDs, translations, and placeables in the 73 Fluent catalogues do
  not change; only the file name (the fluent domain is the crate name).
- Add no production dependencies and do not update pinned dependency commits.

## Implementation Steps

### Task 1: Name clearance gate

- [ ] Complete the Daymural name check (Flathub/Flatpak app IDs, crates.io,
      GitHub, common package registries, trademark search) and record the
      result and date here. **Do not start Task 2 until this is `[x]`.**

### Task 2: Rename the code identity

- [ ] Rename the Cargo package and executable to `daymural`; update only the
      root package identity in `Cargo.lock`.
- [ ] Change `APP_ID` to `io.github.ercling.cosmic-applet-daymural`.
- [ ] Update the user agent (`src/bing.rs:22`) and Cargo metadata
      (description, repository, homepage).
- [ ] Rename all 73 Fluent resource files to `daymural.ftl`; confirm
      `every_locale_defines_every_english_message` and
      `loader_is_pinned_to_english` still pass.
- [ ] Update the homepage literal at `src/app.rs:5791` and the workflow
      string expectations listed in Context.
- [ ] Run focused identity and localization tests before Task 3.

### Task 3: Rename the packaging identity

- [ ] Rename and update the desktop entry, AppStream metainfo, symbolic icon,
      root Flatpak manifest (module name **and** `CARGO_HOME`), install
      paths, and the embedded fixtures.
- [ ] Update the justfile, both workflows, the Flatpak bundle artifact name,
      `flatpak/` wrapper references, README, `AGENTS.md`, `CLAUDE.md`, and
      the active Flatpak plan.
- [ ] Use **Daymural** for visible names. Mention **Microsoft Bing** only in
      truthful descriptive text and add an unofficial/non-affiliation notice.
- [ ] Set the homepage/repository URL to
      `https://github.com/ercling/cosmic-applet-daymural`. Note in the
      metainfo commit that this URL 404s until Task 5 renames the repository;
      CI's `--no-net` validation does not check it.
- [ ] Extend identity tests to require a lowercase App ID and exact agreement
      among Cargo, desktop, AppStream, icon, manifest module, `CARGO_HOME`,
      executable, workflow, repository URL, and install paths.
- [ ] Add `--override cid-contains-uppercase-letter=error` to the CI
      `appstreamcli validate --pedantic --explain --strict --no-net` step so
      the lowercase ID is a hard gate.
- [ ] Permit the old identity only in completed historical plans and explicit
      uninstall instructions; reject accidental active packaging drift.
- [ ] Run `just check`, strict AppStream validation, Flatpak source
      generation/prefetch, and a force-clean offline Flatpak build.

### Task 4: Migration documentation

- [ ] README uninstall section: remove the old native *and* Flatpak
      installation before starting Daymural, stating the consequence — two
      differently identified leaders over one `~/Pictures/BingWallpaper`,
      pruning and applying concurrently.
- [ ] README: the panel entry must be re-added after the rename (Settings →
      Desktop → Panel); old config, state, thumbnails, locks, and the
      coordination mailbox are not migrated; the image folder is kept.

### Task 5: Release acceptance and submission

- [ ] Uninstall the previous native and Flatpak identities, then install
      Daymural in an intentional COSMIC session.
- [ ] Re-add the panel entry; verify discovery, refresh, navigation,
      wallpaper application, attribution, shuffle, retention, accent
      behavior, and multi-output leader ownership.
- [ ] Start with retained JPEGs but fresh Daymural state and confirm
      automatic title/author hydration (the hardening plan's behavior under
      the new identity).
- [ ] Publish or rename the repository to `ercling/cosmic-applet-daymural`,
      then rerun network-enabled AppStream validation.
- [ ] Submit under `app/io.github.ercling.cosmic-applet-daymural/` only after
      the exact commit passes offline and live validation.
- [ ] Use developer-owned or clearly licensed Store screenshots unless
      separate permission allows Bing imagery.
- [ ] Move this plan to `docs/plans/completed/` only after all automated and
      live gates pass.

## Testing Strategy

- **Packaging tests:** the embedded identity web in `src/app.rs` extended to
  the new name, `CARGO_HOME`, and a lowercase-ID assertion.
- **Localization tests:** existing locale guards, unchanged, over the renamed
  files.
- **External:** strict AppStream validation in CI, offline Flatpak build.
- **Full suite:** `just check`.

## Assumptions and External Gates

- The identity break is acceptable because the applet is unpublished.
- The Daymural name search is preliminary until Task 1 records clearance.
- Branding cleanup does not authorize Bing imagery use. Store publication
  remains gated on the applicable Microsoft terms and any required Microsoft
  or rightsholder permission.

## Progress Tracking

- Mark completed work with `[x]` immediately.
- Add discovered scope with a `➕` prefix.
- Record blockers or deviations with a `⚠️` prefix.
- Keep this plan synchronized with implementation and preserve unrelated
  working-tree changes.
