# Store-Ready Applet Hardening

## Overview

Prepare the applet's behavior for Store submission with two related changes:

- honor Bing's per-image `wp` eligibility: never download an image whose
  eligibility is not affirmatively `true`, and remove images Bing explicitly
  marks `false` — durably, so they cannot return through a catalogue rebuild.
  The one deliberate exception is the wallpaper currently on screen, which is
  kept (entry and file) until another image replaces it, so the popup never
  attributes a different image than the one displayed;
- repair metadata immediately after rebuilding `catalogue.json`, eliminating
  the period where a recovered image pack displays filenames until the user
  presses **Check for new images now**.

The list request always asks Bing for its complete supported eight-image
window (`idx=0&n=8`, a few KB of JSON); the configured retention horizon
governs which of those images are *downloaded*. If the horizon holds no
eligible image, download only the newest eligible image later in that same
response. Never paginate beyond the supported window.

The product rename to **Daymural** is deliberately **not** part of this plan:
it has no technical dependency on this work, is the most invasive and least
reversible change, and is gated on name clearance that is still preliminary.
It lives in `docs/plans/20260822-daymural-rename.md` and is executed after
this plan lands. Nothing here may introduce the new name.

## Context

- **Eligibility:** `src/bing.rs` currently omits `wp` (its doc comment at
  `src/bing.rs:38-40` says so deliberately; CLAUDE.md does not mention `wp`
  at all yet); every structurally accepted archive entry reaches the download
  pipeline. The checked-in fixture carries `wp: true`, but that is one
  capture — Task 6 live-verifies the field is present for the market-less URL
  shape the applet uses (`src/bing.rs:157`). Because a remote JSON change is
  outside our control, **absent `wp` must never delete anything**: it blocks
  downloads only. The only irreversible action in this plan — unlinking a
  JPEG — is authorized solely by an explicit `wp: false`.
- **Metadata root cause (verified):** `Catalogue::load_or_rebuild`
  (`src/catalogue.rs:139-159`) reconstructs a missing, corrupt, or
  valid-but-empty catalogue from wallpaper filenames. Rebuilt entries
  deliberately have blank title, copyright, and information-link fields.
  Startup (`src/app.rs:3503-3507`) sees the resulting nonempty catalogue as
  warm, starts a thumbnail pass, and schedules the next network refresh from
  the synthetic `…0000` date: ~24 h out when the newest file is dated today,
  otherwise `next_refresh`'s out-of-range reset (`src/schedule.rs:49-54`)
  fires in ~6 min. Either way the pack shows filenames until that refresh
  merges the metadata; a manual refresh does it immediately, explaining the
  reported recovery.
- **Resurrection path:** `rebuild_from_folder` (`src/catalogue.rs:139,188`)
  turns every wallpaper-named JPEG in `~/Pictures/BingWallpaper` into a
  navigable entry, and `Catalogue::prune` (`src/catalogue.rs:317-360`) only
  deletes files it still has entries for. An ineligible image whose entry is
  dropped but whose JPEG is kept therefore returns after any rebuild. The
  fail-closed guarantee must be held at the file level, **and** an entry may
  only be dropped once its file is confirmed gone — prune already does this
  (on unlink failure it keeps the entry and retries next time,
  `src/catalogue.rs:352-357`).
- **Prune's live-wallpaper evidence:** `Window::prune_over`
  (`src/app.rs:1683-1693`) passes `wallpaper::prune_retention(&live, …)`,
  which returns `0` — deletion off entirely — whenever the live state is
  `CurrentWallpaper::Unknown` (`src/wallpaper.rs:293-298`: `same-on-all ==
  false`, i.e. per-output setups, or an unreadable cosmic-bg config). In that
  state `self.current` is `None`, so a "protect `self.current`" exemption
  protects nothing; eligibility deletion must reuse the same guard.
- **Displayed entry:** `view::displayed` (`src/view.rs:99-103`) falls back to
  `catalogue.newest()` when the live wallpaper has no entry, and
  `prev_target`/`next_target` (`src/view.rs:108-122`) switch to the
  foreign-wallpaper branch via `catalogue.contains`. Removing the live
  wallpaper's entry would make the popup attribute a different image than the
  one on screen. (Auto-apply itself is path-based and unaffected —
  `wallpaper::is_ours`, `src/wallpaper.rs:302-315`.)
- **Scheduling:** `refresh_success_plan` (`src/app.rs:3411-3427`) derives both
  `auto_apply` and `delay` from one `newest_fullstartdate`, and
  `finish_refresh` (`src/app.rs:2247-2252`) feeds it `self.catalogue.newest()`,
  not the response. An all-ineligible response on a warm catalogue would
  schedule off a stale entry and hit the same ~6-minute reset.
- **Thumbnail invariant (CLAUDE.md, `run_thumbnail_pass`):** previews come
  from the cache, a non-empty catalogue is not a cold start, and the first
  refresh can be ~24 h out — or never, offline — so thumbnail generation must
  never depend on a fetch. `fetch_and_download` (`src/app.rs:3165-3215`)
  aborts at `bing::fetch_image_list` on any network error, before
  `backfill_thumbnails` at its tail runs.
- **Producer arming is an either/or today:** `Window::arm_leader_duties`
  runs `if let Some(request) = outstanding { start_refresh_over } else {
  start_thumbnail_pass_over }`. The outstanding-peer-request branch therefore
  already violates the invariant above (offline leader start with a queued
  peer request ⇒ dead fetch, no thumbnail pass). Task 3 fixes that branch
  too.
- **Two producers collide on writes, not just sweeps:** `fsutil::temp_sibling`
  builds a deterministic `dest + suffix` path, so `thumbs::ensure_thumbnail`
  writes `<thumb>.part`/`<thumb>.meta` at fixed names; and the refresh's
  fetch loop calls `ensure_thumbnail_logged` on every `existing_file` hit
  (`src/app.rs:3194-3208`) — on a rebuilt start, exactly the files the
  startup pass is decoding. The `may_sweep_thumbnails` deferral covers the
  *sweep*; the write overlap needs its own interlock.
- **Auto-apply suppression:** `wallpaper::should_auto_apply`
  (`src/wallpaper.rs:313-315`) is false when the user has set a foreign
  wallpaper. A fallback image is out of retention by construction, so without
  a rule it would be pruned on the next refresh and re-downloaded daily.
- **Concurrency:** exactly one active leader may fetch, download, prune,
  reconcile thumbnails, or persist shared catalogue changes. Followers must
  use the existing coordination mailbox.
- **Existing work:** preserve and integrate the current uncommitted README
  attribution edits (`git diff README.md`). Its paragraph beginning "The
  current implementation does not inspect Bing's per-image `wp` field" is the
  text Task 5 replaces; its credit/`catalogue.json` paragraph stays.

## Development Approach

- **Testing approach:** regular code-then-tests within each task.
- Keep network, filesystem, thumbnail, and theme work off the UI thread.
- Keep merge, eligibility reconciliation, prune, save, and auto-apply in
  `RefreshFinished` against live UI-thread state.
- The startup thumbnail pass is the guaranteed thumbnail producer and is
  armed unconditionally for an active leader; a refresh is an *additional*
  task. While `thumbnail_pass_pending`, the refresh does not write
  thumbnails (its per-download `ensure_thumbnail_logged` and tail
  `backfill_thumbnails` are skipped) — the pass ends in its own sweep and
  `ThumbnailsReady` recompute, and the next refresh backfills anything new.
- Use injected tempdirs and loopback HTTP fixtures; tests must never contact
  Bing or real COSMIC configuration.
- Add no production dependencies and do not update pinned dependency commits.
- CLAUDE.md is the architecture record: every task that invalidates or
  extends one of its bullets updates that bullet in the same task.

## Public and Internal Interfaces

There is no external API change. Internal interfaces change as follows:

- `BingImage` gains `wp: Option<bool>` with `#[serde(default)]`:
  `Some(true)` = downloadable, `Some(false)` = explicitly ineligible (entry
  and file removed), `None` = not downloadable, nothing removed.
- Parsed archive output retains source position and carries eligible images,
  **explicitly** ineligible URL bases (`Some(false)` only), a count of
  absent-`wp` entries for logging, and the newest structurally valid
  `fullstartdate` scheduling anchor.
- `refresh_success_plan` takes the scheduling anchor (from the response)
  separately from the "has images / auto-apply" input (from the live
  catalogue); the two are no longer one `newest_fullstartdate`.
- `schedule::fetch_count` is renamed `download_horizon(retention_days)`: the
  list request is a constant 8; this is the number of newest archive
  positions considered for download.
- Catalogue restoration returns a `CatalogueRestore` containing the catalogue
  and `Loaded` or `Rebuilt` provenance.
- Refresh completion returns a batch containing hydrated/downloaded entries,
  explicitly ineligible URL bases, the scheduling anchor, and an optional
  protected cold-start fallback.

## Implementation Steps

### Task 1a: Parse and enforce eligibility at the fetch boundary

- [x] Validate `startdate`, `fullstartdate`, and `urlbase` before an entry can
      influence downloads or scheduling. Note in `src/schedule.rs:36-39` that
      `next_refresh`'s malformed-`fullstartdate` branch is now reachable only
      from corrupt catalogue entries, not from the response path — it is not
      dead code.
- [x] Preserve response order while partitioning structurally valid entries
      into eligible (`Some(true)`), explicitly ineligible (`Some(false)`),
      and absent (`None`).
- [x] Never issue an image GET for an entry that is not `Some(true)`.
- [x] Keep an empty response or a response with no structurally valid entries
      as `FetchError::EmptyList`.
- [x] Update the `src/bing.rs` doc comment and **add** the eligibility rule to
      the CLAUDE.md `src/bing.rs` bullet.
- [x] Test true, false, missing, mixed, malformed, and empty responses with
      hermetic parser tests; update the one `BingImage` literal at
      `src/bing.rs:648`. ⚠️ `src/bing.rs:648` was `fixture_image()`, a
      clone out of the parsed fixture, not a struct literal — there is no
      `BingImage { .. }` literal in the crate (`grep -rn "BingImage {"`);
      it and every other `.images[0]` test access moved to
      `.eligible[0].image`.
- [x] Assert via the loopback server that non-`Some(true)` entries produce
      zero image GET requests.
- [x] Run focused Bing tests before Task 1b.

### Task 1b: Reconcile the catalogue and schedule on completion

- [x] Carry explicitly ineligible URL bases to refresh completion. Deletion
      is gated on the same evidence prune uses: if the live state is
      `CurrentWallpaper::Unknown`, delete nothing this refresh and retry next
      time; otherwise, for each matching entry **other than the live
      wallpaper's**, `fs::remove_file` its JPEG and, **only if the unlink
      succeeded or the file was already absent**, remove the entry. On unlink
      failure keep the entry (mirroring `Catalogue::prune`) and retry next
      refresh. The next `thumbs::reconcile` sweep collects the orphaned
      thumbnail (it is a directory sweep, deferred while
      `may_sweep_thumbnails` is false as today).
- [x] Exempt the live wallpaper's entry from eligibility removal, mirroring
      prune's `currently_applied` protection; it and its file go on the first
      refresh after another image is applied. Document the exemption and the
      `Unknown` guard in the CLAUDE.md `catalogue.rs` bullet.
- [x] Treat a valid response with zero eligible images as a successful no-op:
      retain other eligible history and the current wallpaper, clear prior
      refresh errors, keep cold-start auto-apply armed, acknowledge peer
      refreshes successfully, and schedule from the response's anchor.
      Emit one `tracing::warn!` that names the explicit-`false` count and the
      absent-`wp` count separately, so an all-restricted day and a Bing
      payload change that dropped the field are distinguishable in logs
      without a new i18n string. (The warn was already placed in
      `fetch_and_download` by Task 1a; it is kept there, where both counts
      are at hand.)
- [x] Split `refresh_success_plan`: `auto_apply` from the live catalogue's
      `has_images`, `delay` from the response anchor. Update
      `refresh_success_plan_without_images_backs_off` (`src/app.rs:4490`)
      and any sibling tests to the new signature.
- [x] Update the CLAUDE.md `src/app.rs` refresh-pipeline bullet
      (`RefreshFinished` now also reconciles eligibility).
- [x] Test: an already-downloaded image marked `Some(false)` is removed from
      the catalogue **and** its JPEG is gone; then delete `catalogue.json`,
      restart via `load_or_rebuild`, and assert it does not return.
- [x] Test: an already-downloaded image whose `wp` is absent keeps its entry
      and file.
- [x] Test: unlink failure (read-only dir) ⇒ entry retained, no resurrection
      window; next refresh retries.
- [x] Test: `CurrentWallpaper::Unknown` ⇒ nothing deleted (mirror
      `classify_maps_the_three_cosmic_bg_states`).
- [x] Test: the live wallpaper's entry marked `Some(false)` stays in the
      catalogue and on disk; `view::displayed` still resolves to it.
- [x] Test: all-ineligible response on a warm catalogue schedules the normal
      daily delay, not the out-of-range ~6-minute reset.
- [x] Run focused catalogue and refresh tests before Task 2.

### Task 2: Request the full window; bounded fallback download

- [ ] Always request `idx=0&n=8`; drop the retention-sized list request.
      Retention now governs download selection only: retention `0` or at
      least eight considers all eight positions; retention `1..=7` considers
      that many newest archive positions.
- [ ] If the normal horizon contains no eligible entry **and auto-apply is not
      suppressed** (`wallpaper::should_auto_apply`), download only the newest
      eligible image from the remaining positions as the fallback. This
      applies on every refresh, not only cold start — it is what keeps
      `retention=1` with an ineligible newest image from becoming a permanent
      daily no-op. When auto-apply is suppressed (foreign wallpaper) the
      fallback is skipped: it exists only to give the applet something to
      apply, and downloading it would be pruned and re-fetched daily.
- [ ] Download no other out-of-retention images solely for fallback.
- [ ] Protect the selected fallback through the current merge/prune cycle so
      it cannot be removed before auto-apply. If apply fails, retain it until
      the next ordinary refresh. This protection is in-memory only: a restart
      between download and a failed apply lets `prune_and_persist` delete an
      out-of-retention, non-applied fallback, and the next refresh simply
      re-downloads it — accepted, and documented in the `Backfill` paragraph
      of CLAUDE.md's `src/app.rs` bullet.
- [ ] Once applied, rely on the existing current-wallpaper protection.
- [ ] If all eight entries are ineligible, complete as a successful no-op
      (Task 1b rule) and wait for the normal next daily refresh.
- [ ] Do not increment `idx` or attempt unsupported historical pagination.
- [ ] Rename `schedule::fetch_count` to `download_horizon`, update its tests,
      and the CLAUDE.md `src/schedule.rs` bullet.
- [ ] Test newest-eligible, fallback-eligible, multiple older eligible,
      all-eight-ineligible, prune protection, failed apply, the warm
      `retention=1` + ineligible-newest case (fallback downloads the next
      eligible), and foreign-wallpaper + ineligible-newest (no fallback
      download, no churn across two refreshes).
- [ ] Run focused pipeline and scheduling tests before Task 3.

### Task 3: Catalogue provenance and leader-side metadata repair

- [ ] Mark missing, corrupt, and valid-but-empty catalogue fallbacks as
      `Rebuilt`; ordinary nonempty JSON loads remain `Loaded`. Update the
      CLAUDE.md `load_or_rebuild` bullet.
- [ ] Restructure `Window::arm_leader_duties`: the thumbnail pass is armed
      unconditionally for an active leader; a refresh is started *in
      addition* when there is an outstanding peer request **or** the restore
      was `Rebuilt` and nonempty. Never instead of the pass — offline, the
      refresh fails at the list fetch and the pass is the only producer.
- [ ] Producer write interlock: while `thumbnail_pass_pending`, the refresh
      skips `ensure_thumbnail_logged` per download and the tail
      `backfill_thumbnails`; the next refresh backfills. No change to
      `fsutil` (the "never hand-roll another temp-then-rename" rule stands;
      a unique-suffix variant is not needed once only one producer writes).
- [ ] Hydrate every matching local eligible JPEG in the eight-entry window,
      but do not download additional out-of-retention images solely for
      metadata repair.
- [ ] Coalesce repair with any outstanding peer refresh so only one refresh
      is in flight.
- [ ] Merge recovered title, credit, and information link into live state and
      save atomically. Historical images outside Bing's response keep the
      honest filename fallback.
- [ ] On failure, retain reconstructed entries and use the existing retry path;
      never delete local JPEGs or persist an empty history.
- [ ] Update the CLAUDE.md startup-thumbnail-pass paragraph: pass always
      armed, refresh additive, write interlock.
- [ ] Test loaded/rebuilt provenance, missing/corrupt/empty JSON, leader
      startup, network failure, and persistence.
- [ ] Test: rebuilt catalogue + unreachable loopback server ⇒ thumbnails are
      still cached after the startup pass.
- [ ] Test: offline leader start with an outstanding peer request ⇒
      thumbnails still produced (the pre-existing hole).
- [ ] Test: pass and refresh running over the same rebuilt catalogue yield
      exactly one intact thumbnail + `cached` sidecar per entry, no stray
      `.part`.
- [ ] Add the reported regression: a rebuilt pack initially has filename
      fallbacks, then automatic hydration restores every available title and
      author without a manual refresh.
- [ ] Run focused catalogue and refresh tests before Task 4.

### Task 4: Repair across takeover and followers

- [ ] Carry restoration provenance through leadership takeover hydration and
      apply the same repair decision after readiness.
- [ ] If a follower rebuilds a nonempty catalogue during initialization or
      popup reload, request one leader refresh through the existing mailbox.
- [ ] Suppress duplicate follower requests while persistence, refresh, or
      acknowledgement is pending; allow retry after the established timeout.
- [ ] Update the CLAUDE.md leadership bullet for the new follower request
      reason.
- [ ] Two-instance tests: takeover with a rebuilt catalogue, follower proxy,
      coalescing with an outstanding request, timeout retry.
- [ ] Run focused coordination tests before Task 5.

### Task 5: Fluent cleanup, documentation, and packaging validation

- [ ] Replace the unreachable `fl!("bing-wallpaper")` fallback in
      `view::display_title` (`src/view.rs:147`) with
      `file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()`
      (`file_stem()` is still an `Option`), confirm
      `display_title_falls_back_to_file_stem_for_rebuilt_entries`
      (`src/view.rs:589-595`) still asserts the right shape, and delete the
      `bing-wallpaper` id from all 73 Fluent catalogues
      (`every_message_id_is_referenced_by_the_ui` and
      `every_locale_defines_every_english_message` both require it).
- [ ] Integrate the existing README rights/attribution changes rather than
      overwriting them.
- [ ] Explain that the applet creates `catalogue.json` from response metadata
      and does not embed that metadata into downloaded JPEG bytes.
- [ ] Replace the documented `wp` limitation with the implemented fail-closed,
      explicit-false-deletes, and bounded-fallback behavior; add a one-line
      troubleshooting note that an applet that stays empty with no error
      should be run with `RUST_LOG=cosmic_bing_wallpaper=warn` to see the
      eligibility counts.
- [ ] Clarify that GPL covers the applet's code/assets, not downloaded
      imagery, and attribution does not grant redistribution permission.
- [ ] Make CI run
      `appstreamcli validate --pedantic --explain --strict --no-net data/io.github.ercling.CosmicBingWallpaper.metainfo.xml`
      in `.github/workflows/flatpak.yml:88`, **and in the same commit** update
      the literal the packaging test asserts at `src/app.rs:6447`
      (`active_workflow_step_containing(&flatpak, "appstreamcli validate --no-net …")`)
      — otherwise `just check` fails. `--pedantic` alone only prints hints
      and exits 0 (verified with appstreamcli 1.1.3); `--strict` is what
      fails the step. Do not add a `cid-contains-uppercase-letter` override
      here — that is the rename plan's concern and would fail CI until it
      lands.
- [ ] Run `just check`, AppStream validation, Flatpak source
      generation/prefetch, and a force-clean offline Flatpak build.

### Task 6: Live acceptance

- [ ] Install in an intentional COSMIC session; verify panel discovery,
      refresh, navigation, wallpaper application, attribution, shuffle,
      retention, accent behavior, and multi-output leader ownership.
- [ ] Live-verify that the production HPImageArchive response for the
      market-less URL shape (`format=js&idx=0&n=8&mbl=1&mkt=`) carries `wp`
      on every entry; record the capture date in this plan.
- [ ] Start with retained JPEGs but fresh state and confirm automatic
      title/author hydration with no manual refresh — and that it is the
      immediate repair refresh, not the ~6-minute out-of-range reset, that
      does it (check the log timestamps).
- [ ] Test a controlled all-ineligible cold start and confirm there is no
      download loop, error state, or restricted wallpaper use, and that the
      warn line appears under `RUST_LOG=cosmic_bing_wallpaper=warn`.
- [ ] Test a controlled fallback response and confirm that only the newest
      eligible fallback is downloaded and applied.
- [ ] Move this plan to `docs/plans/completed/` only after all automated and
      live gates pass; then begin `20260822-daymural-rename.md`.

## Testing Strategy

- **Unit tests:** parsing (three-way `wp`), eligibility partitioning,
  archive-position selection, scheduling anchor split, catalogue provenance,
  merge/removal/file deletion with the unlink-failure and `Unknown` guards,
  live-entry exemption, and fallback protection/suppression.
- **Integration tests:** loopback HPImageArchive/image server and tempdir-rooted
  catalogue/download/cache state, including the offline rebuilt-start and
  two-producer thumbnail-integrity cases.
- **Two-instance tests:** one leader and one follower sharing injected
  coordination/catalogue roots; cover repair request, coalescing, completion,
  timeout, and takeover.
- **Packaging tests:** embedded declarative-file checks (with the updated
  workflow literal) plus external strict AppStream and offline Flatpak
  validation.
- **Full suite:** `just check`.
- **Live acceptance:** intentional COSMIC/Flatpak session only; never start the
  applet casually during automated verification.

## Assumptions and External Gates

- The Bing list request is bounded to the supported latest-eight response; it
  is not historical scraping.
- No new user-visible string is added; one unreachable string is removed, so
  the 73 catalogues change only by deleting that id.
- `wp` enforcement and attribution do not authorize Bing imagery use. Store
  publication remains gated on the applicable Microsoft terms and any
  required Microsoft or rightsholder permission.
- The Daymural rename, Store submission under the new ID, and repository
  rename are out of scope here — see `20260822-daymural-rename.md`.

## Progress Tracking

- Mark completed work with `[x]` immediately.
- Add discovered scope with a `➕` prefix.
- Record blockers or deviations with a `⚠️` prefix.
- Keep this plan synchronized with implementation and preserve unrelated
  working-tree changes.
