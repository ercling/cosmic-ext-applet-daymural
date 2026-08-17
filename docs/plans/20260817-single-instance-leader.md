# Single-Instance Leader Election

## Overview

cosmic-panel spawns one applet process **per output** (`output: All` in the
panel config), so a multi-monitor session runs two (or more) full instances of
this applet. The whole runtime — timers, refresh pipeline, wallpaper applies,
lock-screen pokes, and above all the accent state machine — was built on a
documented single-instance assumption, and two instances demonstrably fight:

- **Diagnosed 2026-08-17 (live trace, reproduced + control-tested):** user
  toggles accent on in instance A's popup → A persists `accent_enabled=true`
  and spawns the theme write → instance B's `ConfigUpdated` routes the flip as
  an *external enable*, snapshots the pre-write accents, computes, and reads
  A's freshly written builder accents ≠ its own stand-in snapshot → gap
  `Disarm { keep_snapshot: true }` flips the toggle off (~140 ms after enable)
  → A's write completion reconciles disk `false` vs spawn-time `true` as an
  external disable → `DisableRestore` puts the snapshot back. Net effect:
  "accent turns on for a few seconds, then reverts". With one instance
  SIGSTOPped the same enable sticks permanently — the second instance is
  conclusively the trigger.
- The same duplication silently doubles every automatic behavior: two refresh
  timers (double Bing fetches), two shuffle timers (images skipped at double
  rate), two auto-applies, two lock-poke ladders, two thumbnail passes and
  prune sweeps racing over one state dir.

**Fix (approach A, chosen):** advisory file-lock leader election. Exactly one
instance — the leader — runs all automatic/background work and the accent
lifecycle. Non-leaders stay fully interactive: the popup renders, navigation
and manual applies still work, and the accent toggler works by writing the
config flag to disk, which the leader already consumes through its existing
external-flip routing (`ConfigUpdated` → `set_accent_enabled`). The lock is
released automatically on process exit, so unplugging the monitor that hosted
the leader lets the survivor take over.

**The accent lifecycle must be unreachable on a non-leader through *every*
entry point, not just the toggler/watcher.** `start_accent_compute`
(`src/app.rs:1240`) is reachable from `Message::ApplyImage` →
`on_apply_success` (~2551), `RefreshFinished` → `finish_refresh` →
`on_apply_success` (~1174), and `Message::ThumbnailsReady` (~2685) — all of
which stay user-reachable on a non-leader by design. Gating only the toggler
would reproduce the traced self-disarm through a "next"-click. The gate
therefore lives at the choke point (see Technical Details).

## Context (from discovery)

- Files/components involved:
  - `src/app.rs` — `init` (~line 2313: arms `schedule_refresh`, `sync_shuffle`,
    `start_thumbnail_pass_over`, startup accent reconciliation),
    `Message::RefreshDue`/`ShuffleDue`/`LockEvent`/`LockPokeDue` handlers,
    `finish_refresh` (~1135: unconditional `schedule_refresh` + `sync_shuffle`
    re-arm), `ConfigUpdated` (~2469: accent flip routing, `prune_immediately`,
    `sync_shuffle`), `set_accent_enabled` (~1664), `set_config` (~852:
    **full-entry** `write_entry` persist — the accent-trio clobber hazard),
    `persist_accent_flag` (~1870), `start_accent_compute` (~1240),
    `restore_catalogue` (~1957: persists the startup sweep and runs
    `thumbs::reconcile` — destructive), `state_dir()` (~50),
    `Message::SetAccentEnabled` (~2642), `SetRetention` (~2635, direct
    `prune_immediately`), `TogglePopup` (~2411).
  - `src/view.rs` — `accent_toggler` (~386) routes to
    `Message::SetAccentEnabled`; no view changes expected (same UI on both
    instances; `status_line` degrades correctly on a restored catalogue — the
    zero-new-strings claim is intentional, not an oversight).
  - New: `src/leader.rs` — the lock primitive.
- Related patterns found: pure decision functions + injected dirs/paths for
  tests (`Config::with_custom_path`, tempdir-rooted state), one-shot
  generation-counter timers (`schedule_refresh` ~872 — the codebase's timer
  idiom, reused for the takeover tick), the external-flip routing in
  `ConfigUpdated` that the non-leader toggler proxy reuses wholesale,
  `wallpaper::synced_current` (`src/wallpaper.rs:277`) for re-syncing
  `self.current` against the live cosmic-bg config.
- Dependencies identified: **none new** — rustc is 1.97.1 and
  `std::fs::File::try_lock` (stable since 1.89) provides advisory locking.
  Verified empirically on this toolchain: it is `flock`-backed (per open file
  description), so two separate opens of the same path *in the same process*
  conflict (`Err(TryLockError::WouldBlock)`), and a third handle acquires
  after the first is dropped — the primitive is unit-testable in-process.
  Caveat for tests: cargo runs tests as threads of one process, so **every
  lock test needs its own tempdir**.
- Environment facts worth keeping straight: this system's COSMIC 1.5.0 reads
  theme **v2** dirs (`~/.config/cosmic/com.system76.CosmicTheme.*/v2/`); the
  `v1` dirs are stale leftovers. The theme writes themselves work — the bug is
  purely the instance fight.

## Development Approach

- **testing approach**: Regular (code first, then tests within the same task)
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
  - tests are not optional - they are a required part of the checklist
  - write unit tests for new functions/methods
  - write unit tests for modified functions/methods
  - add new test cases for new code paths
  - update existing test cases if behavior changes
  - tests cover both success and error scenarios
- **CRITICAL: all tests must pass before starting next task** - no exceptions
- **CRITICAL: update this plan file when scope changes during implementation**
- run tests after each change (`just check` — exports the required
  `PKG_CONFIG_PATH`; raw `cargo` needs it exported manually)
- maintain backward compatibility: a single-monitor session must behave
  byte-for-byte as today (one instance always wins the lock at startup)

## Testing Strategy

- **unit tests**: required for every task (see Development Approach above).
  Tests never touch real user config/state — inject tempdir-rooted paths, as
  the whole suite already does; `arm_leader_duties` takes the live-wallpaper
  value as a parameter precisely so tests can pass a hermetic one.
  `Leadership` implements `Default` **as leader**, so the existing
  `Window::default()`-based suite (~43 fixture sites) keeps its current
  semantics with zero edits; only the new non-leader tests use
  `Leadership::forced(false)`.
- **e2e tests**: none in this project (iced view code is exempt by
  convention); the two-instance regression test in Task 4 is the closest
  equivalent — two `Window`s over one shared tempdir config replaying the
  exact traced failure sequence, including the compute-path trigger.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope
- keep plan in sync with actual work done

## Solution Overview

- **One primitive, one question.** `src/leader.rs` owns a `Leadership` value:
  an exclusive advisory lock on `state_dir()/leader.lock`, acquired
  non-blocking at `init`, re-attemptable later. Everything else asks
  `window.is_leader()`.
- **Leader**: identical to today's behavior. No code path changes for it.
- **Non-leader**: restores catalogue + config for display **without side
  effects** (load only — no sweep persist, no `thumbs::reconcile`), renders
  the same popup, but arms **no** automatic work: no refresh timer, no shuffle
  timer, no startup thumbnail pass, no startup accent reconciliation, no lock
  pokes, no watcher-driven prune/shuffle re-arming, and **no accent lifecycle
  through any entry point** — the choke-point gate in `start_accent_compute`
  covers apply/refresh/thumbnail triggers, and the toggler/watcher paths are
  gated besides. Its accent toggler becomes a proxy: persist the flag raw and
  let the leader's existing external-flip path do the real work. Its settings
  persists go through raw per-key writes so the leader's on-disk accent trio
  is never clobbered by a stale full-entry `set_config`.
- **Takeover**: while not leader, a slow periodic retry (one-shot
  generation-counter timer, the codebase's timer idiom) re-attempts the lock.
  On acquisition, the instance adopts the on-disk config wholesale (its
  in-memory accent trio is cold and no flights exist, so at this boundary —
  and only here — disk is authoritative) and then runs the same arming
  sequence `init` runs for a leader, factored into one shared
  `arm_leader_duties`.
- **User-initiated actions stay local**: prev/next/newest/apply and the manual
  refresh button keep working on whichever instance the user clicked — but
  their *re-arming side effects* (`finish_refresh`'s `schedule_refresh` +
  `sync_shuffle`, `ApplyImage`'s `sync_shuffle`) are leader-only, so a
  non-leader click never leaves a timer armed behind it. Catalogue writes are
  atomic (temp-then-rename) and last-writer-wins; a non-leader reloads the
  catalogue *and re-syncs `self.current`* from disk when its popup opens so it
  never navigates a stale list.

## Technical Details

- **Lock mechanics**: `File::options().create(true).write(true).open(path)`
  then `try_lock()`. Holding the open `File` in the struct holds the lock;
  process death (including SIGKILL and the panel reaping an output's applets)
  releases it. `Err(TryLockError::WouldBlock)` = someone else leads; any other
  error (unwritable state dir, filesystem without working flock) logs the
  **reason at warn level** — so live verification can tell "won the lock" from
  "gave up on locking" — and resolves to **leader**: a lone instance that
  cannot lock must still do its job, and two instances both failing the same
  way is no worse than today.
- **`Leadership` shape** (`src/leader.rs`): explicit
  `{ leader: bool, file: Option<File> }` — the flag is not derived from
  `file.is_some()` because the error-path leader holds no file, and because
  `#[derive(Default)] struct Window` (app.rs:67) requires
  `impl Default for Leadership`, which must be **leader** so all ~43 existing
  `Window::default()` fixtures keep today's semantics untouched. Production
  cannot pick the default up silently: `init` builds `Window` exhaustively
  (~2363), so the new field is a compile error there until set from
  `acquire()`. API:
  - `acquire(dir: &Path) -> Leadership` — create dir best-effort, attempt lock.
  - `is_leader(&self) -> bool`
  - `try_acquire(&mut self) -> bool` — returns `true` only on the
    not-leader → leader edge (the takeover trigger).
  - `#[cfg(test)] forced(leader: bool) -> Leadership`.
- **Gating points in `app.rs`** (each checks `self.is_leader()`):
  - `init`: the arming block (refresh timer, `sync_shuffle`,
    `start_thumbnail_pass_over`, startup accent reconciliation / auto-apply)
    moves into
    `fn arm_leader_duties(&mut self, live: wallpaper::CurrentWallpaper) -> app::Task<Message>`
    — the `live` value is a **parameter** (a no-arg version would have to call
    `wallpaper::current_wallpaper()` itself, which reads the real cosmic-bg
    config and breaks test hermeticity); the refresh `delay` is computed
    inside from `self.catalogue`. `init` calls it only when leader.
  - `restore_catalogue` on a non-leader: **load only** — no sweep persist, no
    `thumbs::reconcile`. The current restore persists the startup sweep and
    deletes thumbnails; racing the leader's in-flight startup pass from a
    second process is exactly the race `may_sweep_thumbnails` exists to
    prevent within one process, and a non-leader has no visibility into the
    leader's pass.
  - **`start_accent_compute` (~1241): the accent choke point** — return
    `Task::none()` when not leader. Defense in depth: `AccentComputed` and
    `AccentWriteFinished` are also dropped on a non-leader (none should ever
    exist there).
  - `Message::RefreshDue` / `Message::ShuffleDue` / `Message::LockEvent` /
    `Message::LockPokeDue`: drop when not leader, even with a matching
    generation (a takeover must not activate generations armed before it).
  - `finish_refresh` (~1210-1213): the `schedule_refresh(plan.delay)` and
    `sync_shuffle(false)` re-arms are leader-only; the merge/prune/save of a
    user-initiated refresh still completes locally. Same for the
    `sync_shuffle(true)` in the `ApplyImage` arm (~2552).
  - `ConfigUpdated` when not leader: keep adopting shuffle/retention values
    into memory (display mirrors) but skip `prune_immediately` and the
    `sync_shuffle` re-arm; for the accent flag, adopt from a **fresh disk
    read** for toggler display only (same freshness rule the leader uses;
    payloads can be late) and never route `set_accent_enabled`. The
    snapshot/last-written fields keep their never-adopt-from-payload rule on
    both sides.
  - `Message::SetRetention` on a non-leader: persist the value but skip the
    direct `prune_immediately` (~2639) — the leader prunes when its watcher
    sees the change. (Pruning deletes image files; a stale non-leader
    catalogue must not drive that.)
  - `Message::SetAccentEnabled` when not leader: set
    `self.config.accent_enabled = desired` for immediate toggler feedback and
    `persist_accent_flag(desired)`; the leader's watcher picks the flip up as
    an external toggle and runs the real lifecycle. No snapshot, no compute,
    no task on the non-leader, ever. `persist_accent_flag` with no
    `config_context` currently no-ops silently — add a `tracing::warn!`.
  - **Non-leader persists never use full-entry `set_config`**: `set_config`
    persists via `write_entry` — *all* fields, from in-memory state — and a
    non-leader's accent trio is stale by design, so a shuffle/retention change
    from the second monitor's popup would wipe the leader's on-disk
    `accent_snapshot` (the only record of the user's pre-feature accents:
    unrecoverable) and `accent_last_written` (creating the gap-disarm shape).
    On a non-leader, settings persists go through raw per-key
    `ConfigSet::set` (the `persist_accent_flag` shape). CLAUDE.md already
    names this exact hazard ("a concurrent `set_config` full-entry write can
    rewrite the pinned disk flag from stale memory").
- **Takeover sequence**: a one-shot generation-counter timer (the
  `schedule_refresh` idiom, ~60 s period, armed at `init` when not leader and
  re-armed after each failed attempt) fires `Message::LeadershipTick(u64)`;
  stale generations are dropped. Handler: `try_acquire()` → on the edge:
  reload the config from disk via `AppletConfig::load(context).normalize()`
  (all fields, accent trio included — cold state, no flights, the one
  boundary where disk is authoritative; with `config_context: None` keep
  memory), then `arm_leader_duties(wallpaper::current_wallpaper())`. The
  accent arming is the same startup reconciliation the leader runs from
  `init` — a steady-state recompute Skips for free, and a disk-enabled state
  left behind by a dead leader arms correctly because snapshot/last-written
  are read fresh. No new subscription: `subscription()` (~2693) is currently
  a fixed unconditional batch, and this plan keeps it that way. The
  `lockwatch::subscription()` zbus stream stays live on non-leaders
  deliberately — only the `LockEvent` handler gates — so a takeover needs no
  D-Bus re-establishment; the idle cost is one parked stream.
- **Popup-open reload** (non-leader only, and only when no local refresh is
  pending): in the `TogglePopup` create branch, reload the catalogue from
  disk and re-sync `self.current` via
  `wallpaper::synced_current(&wallpaper::current_wallpaper(), self.current.take())`
  (the existing helper) — the leader applies wallpapers all day, and
  `self.current` is otherwise set once at `init`, so navigation targets would
  drift stale without this. Leader keeps its in-memory-authoritative
  catalogue untouched.
- **What deliberately does NOT change**: the accent state machine itself (all
  its invariants held — the premise it rests on is being restored, exactly as
  the popup-crash fix restored the no-crash premise), the popup ledger, i18n
  (no new strings — both instances render identical UI), `wallpaper.rs`
  internals, `lockwatch.rs` internals (only the `app.rs` handlers gate).

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code, tests, and doc updates in
  this repository.
- **Post-Completion** (no checkboxes): live two-monitor verification and
  behaviors only observable on real hardware.

## Implementation Steps

### Task 1: Leadership lock primitive

**Files:**
- Create: `src/leader.rs`
- Modify: `src/main.rs` (module declaration)

- [ ] create `src/leader.rs` with `Leadership { leader: bool, file: Option<File> }`,
      `acquire(dir)`, `is_leader()`, `try_acquire()`,
      `#[cfg(test)] forced(bool)`, and `impl Default` = **leader** (required
      by `#[derive(Default)] struct Window`; production sets the field
      explicitly in `init`, so the default cannot leak there)
- [ ] lock file is `<dir>/leader.lock` via `std::fs::File::try_lock`;
      `Err(TryLockError::WouldBlock)` → non-leader; any other error logs the
      reason at **warn** and resolves to leader; log the won role at info
- [ ] declare `mod leader;` in `src/main.rs`
- [ ] write tests (each in its own tempdir — tests share one process): second
      `acquire` on the same dir is not leader; `try_acquire` flips to leader
      after the first `Leadership` is dropped and reports the edge exactly
      once (and returns false when still locked out / already leader);
      `forced(true)`/`forced(false)` and `Default` report correctly;
      unwritable dir resolves to leader (error path)
- [ ] run `just check` - must pass before task 2

### Task 2: Thread leadership through `Window`; gate `init` arming and make the non-leader restore non-destructive

**Files:**
- Modify: `src/app.rs`

- [ ] add `leadership: Leadership` to `Window` (the `Default` derive picks up
      leader-by-default, so no fixture edits); `init` sets it from
      `Leadership::acquire(state_dir())`; add `fn is_leader(&self) -> bool`
- [ ] extract `init`'s automatic arming block into
      `fn arm_leader_duties(&mut self, live: wallpaper::CurrentWallpaper) -> app::Task<Message>`
      (refresh timer with `delay` computed inside from `self.catalogue`,
      `sync_shuffle`, `start_thumbnail_pass_over`, startup accent
      reconciliation / restore auto-apply); `init` calls it only when leader
- [ ] non-leader `restore_catalogue` path: load only — skip the sweep persist
      and the `thumbs::reconcile` call (a second process must never delete
      thumbnails under the leader's in-flight startup pass)
- [ ] write tests: a `Leadership::forced(false)` window through the
      `init`-shaped path arms nothing (no refresh scheduled, no thumbnail
      pass pending, no accent compute) and leaves the state dir's thumbnails
      and the saved catalogue untouched by its restore; a leader window arms
      exactly what `init` armed before this change (hermetic: pass
      `CurrentWallpaper::NoFile`-style values, per the existing
      startup-thumbnail-pass test's pattern)
- [ ] confirm the full existing suite passes unmodified (the
      leader-by-default guarantee) — no fixture churn permitted
- [ ] run `just check` - must pass before task 3

### Task 3: Gate runtime automatic work (timers, re-arms, watcher work, lock pokes, raw non-leader persists)

**Files:**
- Modify: `src/app.rs`

- [ ] `Message::RefreshDue` and `Message::ShuffleDue`: drop (log at debug)
      when not leader, even with a matching generation
- [ ] `finish_refresh`: the `schedule_refresh(plan.delay)` +
      `sync_shuffle(false)` re-arm block is leader-only (a non-leader's
      manual refresh still merges/prunes/saves); same for `sync_shuffle(true)`
      in the `ApplyImage` arm
- [ ] `Message::LockEvent` and `Message::LockPokeDue`: drop when not leader —
      one poke ladder per session, not per output (the zbus subscription
      itself deliberately stays live; see Technical Details)
- [ ] `ConfigUpdated` when not leader: keep adopting shuffle/retention values
      into memory for display, but skip `prune_immediately` and the
      `sync_shuffle` re-arm; `Message::SetRetention` when not leader: persist
      the value, skip the direct `prune_immediately`
- [ ] non-leader settings persists (`SetShuffleEnabled`, `SetShuffleInterval`,
      `SetRetention`) go through raw per-key `ConfigSet::set` instead of the
      full-entry `set_config`, so the leader's on-disk
      `accent_snapshot`/`accent_last_written` are never rewritten from the
      non-leader's stale memory
- [ ] write tests: non-leader drops `RefreshDue`/`ShuffleDue`/`LockPokeDue`
      with a *current* generation (leader still acts); a non-leader
      user-initiated refresh completing does not schedule the next refresh or
      re-arm shuffle (and a subsequent takeover does not inherit a stale
      armed generation); non-leader `ConfigUpdated` with reduced retention
      does not prune and with changed shuffle does not re-arm, yet in-memory
      values update; a non-leader shuffle-interval change leaves
      `accent_snapshot`/`accent_last_written` on disk **byte-identical**
- [ ] run `just check` - must pass before task 4

### Task 4: Non-leader accent gating (choke point + proxy) and the two-instance regression test

**Files:**
- Modify: `src/app.rs`

- [ ] **`start_accent_compute`: return `Task::none()` when not leader** — this
      covers every lifecycle trigger (`on_apply_success` from
      `ApplyImage`/`RefreshFinished`, and `ThumbnailsReady`); defensively
      drop `AccentComputed` and `AccentWriteFinished` on a non-leader too
- [ ] `Message::SetAccentEnabled` when not leader: set
      `self.config.accent_enabled` for immediate toggler rendering, call
      `persist_accent_flag(desired)`, spawn nothing — no snapshot, no
      compute, no theme task; add the missing `tracing::warn!` to
      `persist_accent_flag`'s silent no-`config_context` path
- [ ] `ConfigUpdated` when not leader: adopt `accent_enabled` for display from
      a fresh disk read (never the payload), never route
      `set_accent_enabled`; snapshot/last-written stay never-adopted
- [ ] write tests: non-leader toggle writes only the raw flag (snapshot and
      last-written untouched on disk and in memory; error path: no
      `config_context` → warns, changes nothing); non-leader `ConfigUpdated`
      enable-flip performs no snapshot capture and spawns no task but the
      toggler state follows the disk flag; **non-leader `on_apply_success`
      and `ThumbnailsReady` with `accent_enabled=true` spawn nothing and
      leave the accent trio untouched in memory and on disk**
- [ ] write the regression test replaying the traced 2026-08-17 fight: leader
      window and non-leader window over one shared tempdir config; leader
      enables (snapshot + write task settle via the existing
      `settle_accent_tasks` machinery); deliver the resulting `ConfigUpdated`
      to the non-leader **and then drive the non-leader's compute path too**
      (give it a `current` + cached thumbnail, run `on_apply_success` /
      `ThumbnailsReady`); assert the non-leader neither disarms nor writes
      `accent_enabled=false` through either route, and the leader's enabled
      state survives — the exact sequence that previously self-destructed in
      ~2 s
- [ ] run `just check` - must pass before task 5

### Task 5: Takeover — periodic retry and become-leader adoption

**Files:**
- Modify: `src/app.rs`

- [ ] add `Message::LeadershipTick(u64)` on the one-shot generation-counter
      idiom (`schedule_refresh` shape, ~60 s): armed at `init` when not
      leader, re-armed after each failed attempt, stale generations dropped;
      already-leader ticks are no-ops and arm nothing
- [ ] handler: `try_acquire()`; on the not-leader → leader edge, reload the
      config via `AppletConfig::load(context).normalize()` (whole struct,
      accent trio included — cold state, no flights, the one boundary where
      disk is authoritative; `config_context: None` keeps memory), then
      return `arm_leader_duties(wallpaper::current_wallpaper())`
- [ ] write tests: a forced-non-leader window whose `Leadership` can now
      acquire (other lock dropped, own tempdir) processes `LeadershipTick` →
      becomes leader, adopts a config edited on disk meanwhile (e.g.
      `accent_enabled=true` + snapshot left by the dead leader), and arms
      duties (refresh scheduled; accent reconciliation runs and Skips in
      steady state); a tick while still locked out re-arms and changes
      nothing; a stale-generation tick is dropped
- [ ] run `just check` - must pass before task 6

### Task 6: Non-leader popup reload (catalogue + current)

**Files:**
- Modify: `src/app.rs`

- [ ] in the `TogglePopup` create branch: when not leader and no local
      refresh is pending, reload the catalogue from disk
      (`Catalogue::load_or_rebuild` with the same injected dirs) **and
      re-sync `self.current` via
      `wallpaper::synced_current(&wallpaper::current_wallpaper(), self.current.take())`**
      so navigation targets follow the leader's applies
- [ ] write tests: non-leader popup open picks up entries the "leader" (a
      direct on-disk catalogue save in the test) added and dropped, and
      `prev_target`/`next_target` follow the applied file recorded on disk;
      leader popup open does not reload (in-memory stays authoritative); a
      non-leader with `refresh_pending` does not reload
- [ ] run `just check` - must pass before task 7

### Task 7: Verify acceptance criteria

- [ ] verify all requirements from Overview are implemented: exactly one
      instance runs timers/fetch/apply-automation/pokes/accent; the toggler
      works from either popup; takeover arms a survivor within one tick
- [ ] verify edge cases: leader dies mid-accent-flight (survivor's takeover
      reconciliation Skips or disarms per the existing gap rules — no new
      states introduced); both instances racing `acquire` at startup (flock
      atomicity — one wins); single-monitor session identical to today
- [ ] run full test suite: `just check` (fmt + clippy `-D warnings` + tests)
- [ ] grep for ungated automatic entry points — every caller of
      `schedule_refresh`, `sync_shuffle`, `start_thumbnail_pass_over`,
      `start_accent_compute`, `accent_compute_for_current`,
      `on_apply_success`, `prune_immediately`, `finish_refresh`, poke arming,
      and every non-leader-reachable `set_config` is leader-gated,
      user-initiated-and-side-effect-free, or raw-per-key
- [ ] verify no i18n changes leaked in (no new `fl!` ids — otherwise 73
      catalogues would be due)

### Task 8: [Final] Update documentation

- [ ] update `CLAUDE.md`: new `src/leader.rs` bullet; amend the accent
      section's "single-instance assumption" to state how leadership restores
      it; amend the `ConfigUpdated` rule "in-memory accent state is
      authoritative (single-instance assumption)" with the explicit
      non-leader exception (flag adopted from disk for display only); note
      the per-output spawning fact, the takeover tick, and the
      non-leader raw-per-key persist rule
- [ ] update the session memory note if implementation details diverge from
      the diagnosis write-up
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

*Items requiring manual intervention or external systems - no checkboxes, informational only*

**Manual verification on the real two-monitor session:**
- `just install`, restart the panel (or re-log), confirm two applet processes
  and exactly one holds the lock — path:
  `~/.local/state/io.github.ercling.CosmicBingWallpaper/leader.lock`
  (`lslocks` / `fuser`)
- toggle accent from **each** monitor's popup: it must arm and stay armed
  (watch `~/.config/cosmic/io.github.ercling.CosmicBingWallpaper/v1/accent_enabled`
  and the **v2** theme dirs — not v1)
- click prev/next on the *non-leader* popup with accent enabled: wallpaper
  changes, accent recomputes on the leader (watch v2), and the toggle stays on
- unplug the leader's monitor: within ~60 s the survivor takes over
  (`lslocks` moves; a later refresh/shuffle still fires); replug and confirm
  the new instance comes up as non-leader
- lock/unlock the screen once: exactly one poke ladder in `RUST_LOG` output

**Known accepted limitations (documented, not fixed here):**
- Catalogue writes from user-initiated actions on a non-leader are
  last-writer-wins against the leader's background saves (atomic writes, no
  corruption; worst case one merge is redone on the next refresh). A
  non-leader `SetRetention` no longer prunes locally — the prune happens on
  the leader when the watcher delivers the change
- A non-leader's toggler reflects a leader-side disarm only when the watcher
  event arrives (sub-second in practice)
- A filesystem where flock itself errors (not WouldBlock) yields two leaders —
  logged at warn, no worse than today's behavior
