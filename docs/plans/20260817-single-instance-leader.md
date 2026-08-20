# Single-Instance Leader Election

## Overview

`cosmic-panel` starts one applet process per output. On a multi-monitor
session, every process currently arms refresh and shuffle timers, runs
thumbnail producers and pruning, reacts to lock events, and owns an
independent copy of the accent state machine over the same on-disk state.
The duplicated accent owner is known to self-disarm after one instance
enables the feature, and the other automatic jobs can race or run twice.

Restore the applet's single-owner premise with an advisory file-lock leader:

- exactly one **active leader** owns scheduled refresh/shuffle work,
  downloads, thumbnail production and reconciliation, pruning, automatic
  applies, lock-screen pokes, and every accent lifecycle entry point;
- non-leaders keep the same popup and settings controls, apply selected
  wallpapers locally, proxy manual refresh requests to the leader, and notify
  the leader after a successful local apply so it updates `current`, spends
  `ColdStart`, and recomputes the accent;
- non-leader settings use raw per-key persists, so stale in-memory accent
  snapshot fields can never be written over the leader's authoritative state;
- a surviving non-leader retries the lock periodically and, after the old
  leader process exits, hydrates fresh disk/live state before arming the same
  duties as an initial leader.

The proxy is required, not optional polish. A non-leader-local refresh would
reintroduce cross-process thumbnail-producer/sweep races, while a local apply
without a leader notification would leave the leader's accent and cold-start
state stale.

## Context

- **Files and components:** `src/app.rs` owns initialization, all timers,
  refresh completion, popup opening, config watchers, catalogue restore, and
  accent orchestration; `src/config.rs` defines the existing per-key applet
  config; `src/catalogue.rs`, `src/thumbs.rs`, and `src/wallpaper.rs` provide
  the persisted and live state used during leadership hydration. New
  `src/leader.rs` owns the advisory lock primitive.
- **Existing patterns:** one-shot generation-counter timers; fresh disk reads
  for watcher-sensitive accent decisions; pure decision helpers; blocking
  work returned through guarded completion messages; tempdir-rooted config and
  state in tests; `wallpaper::synced_current` for conservative live-state
  reconciliation.
- **Dependencies:** none. Rust 1.97.1 provides `std::fs::File::try_lock`; the
  pinned libcosmic `Core::watch_config<T>` supports a second config-entry type
  over the same app ID, keyed independently by `TypeId`.
- **Constraints:** preserve the pinned dependencies, Rust edition/toolchain,
  popup ledger, accent state-machine invariants, single-monitor behavior, and
  all filesystem/network/theme test isolation rules. Do not add UI strings.

## Development Approach

- **Testing approach:** regular code-then-tests within each task.
- Complete each task before starting the next and keep this file synchronized
  with implementation.
- Add success, failure, stale-event, and rollback tests for every changed
  decision path.
- Run each task's focused tests, then `just check`, before continuing.
- Preserve unrelated working-tree changes and make no incidental dependency
  updates.

## Testing Strategy

- **Unit/integration tests:** colocated Rust tests. Lock tests use a separate
  `tempfile::TempDir` per test. Config, catalogue, thumbnail, and coordination
  tests use injected tempdir roots. Tests invoke pure completion handlers with
  injected `CurrentWallpaper` values instead of reading real cosmic-bg state.
- **Two-instance regression tests:** two `Window` fixtures share tempdir-rooted
  applet/coordination config and catalogue state, with one real lock winner and
  one loser. They replay the diagnosed accent fight, peer refresh, peer apply,
  and takeover sequences without contacting Bing or the real desktop.
- **End-to-end tests:** iced view construction remains exempt. Real panel,
  compositor, lock screen, and monitor removal behavior is verified manually
  under Post-Completion.
- **Full-suite command:** `just check`.

## Progress Tracking

- Mark completed work with `[x]` immediately.
- Add discovered scope with a `➕` prefix.
- Record blockers or deviations with a `⚠️` prefix.
- Do not mark a task complete until its test gate passes.

## Solution Overview

### Leadership

`Leadership` holds an exclusive advisory lock on
`state_dir()/leader.lock`. `WouldBlock` means non-leader; another lock/open
error warns and fails open as leader so a lone instance still functions.
The open `File` holds the lock and process exit releases it, including when
COSMIC removes the panel applet for a disconnected output.

An initial winner is immediately active after ordinary startup state loading.
A takeover winner is temporarily **not ready** while a blocking task reloads
the complete applet config, coordination state, and live wallpaper. All
leader-only gates ask `is_active_leader()` (owns the lock and is ready), so a
half-hydrated takeover cannot run watcher or timer work. Config/coordination
events observed during hydration invalidate that snapshot and trigger a fresh
one before duties are armed.

### Coordination mailbox

`src/config.rs` gains a second `CosmicConfigEntry` over the existing app ID and
version. Its keys are disjoint from `AppletConfig`, so `AppletConfig::write_entry`
cannot clobber them:

- `refresh_request: u64` — monotonically increased by a non-leader refresh
  click;
- `refresh_completion: PeerRefreshCompletion { request: u64, outcome }` — the
  leader acknowledges every observed request up to `request` after the fetch
  it was attached to completes;
- `apply_notice: Option<PeerApplyNotice { generation: u64, path: PathBuf }>` —
  written after a non-leader successfully applies a wallpaper.

Request/notice read-modify-write operations run on the blocking pool under a
short-lived advisory `state_dir()/coordination.lock`, then write one key
atomically through `ConfigSet::set`. This serializes counter allocation across
processes without extending the leader lock to user interaction. Concurrent
refresh requests receive distinct counters but still coalesce onto one leader
fetch. The practical `u64` exhaustion case is rejected rather than wrapping.
Each process subscribes to `CoordinationConfig` separately from
`AppletConfig`.

A non-leader refresh click first persists the request, then marks its popup
pending and arms a generation-guarded acknowledgement timeout. The active
leader starts a refresh if none is running; otherwise the request joins the
current refresh. Completion reports success/network/disk class, allowing the
requesting popup to reuse existing status strings, clear pending state, and
reload catalogue/live state asynchronously. A failed request persist never
shows a false pending state; a missing completion eventually clears through
the timeout and performs the same safe reload.

After a non-leader apply succeeds locally, it updates its own display state
without running accent work, then writes `apply_notice`. The leader treats a
notice as evidence, reads cosmic-bg's live wallpaper on the blocking pool, and
checks a notice generation on completion. It never trusts a stale payload path
as the displayed wallpaper. A current `File` result flows through
`on_apply_success`, which updates the leader's `current`, spends `ColdStart`,
and uses the existing accent path. Startup/takeover live-state reconciliation
covers a notice lost because the prior leader died.

### Non-leader behavior

Non-leaders do not run refresh pipelines, thumbnail passes, pruning, shuffle
timers, lock pokes, or accent jobs. Their shuffle/retention/accent settings are
persisted one key at a time for the leader's existing config watcher. A
non-leader popup reload is an asynchronous read-only snapshot; it never saves,
prunes, or reconciles thumbnails. Successful local applies invalidate any
older popup-reload completion so live navigation state cannot move backward.

## Technical Details

### Lock API and ordering

Create `src/leader.rs` with:

- `Leadership { leader: bool, file: Option<File> }`;
- `acquire(dir: &Path) -> Leadership`;
- `is_leader(&self) -> bool`;
- `try_acquire(&mut self) -> bool`, true only on the non-leader-to-leader edge;
- a blocking-pool-only helper for the short `coordination.lock` critical
  section used by coordination read-modify-write operations;
- `#[cfg(test)] forced(bool)` and `Default`, both retaining leader-by-default
  semantics for existing `Window::default()` fixtures.

Production `init` must acquire leadership **before** catalogue restoration, so
the loser never runs the current destructive startup sweep before learning its
role. A deterministic file-as-directory path tests the fail-open error path;
permission-based "unwritable" fixtures are not reliable under every test user.

### Initialization and duty arming

Factor leader startup into
`arm_leader_duties(live: CurrentWallpaper) -> Task<Message>`: next refresh,
shuffle synchronization, startup thumbnail pass, and startup accent
reconciliation. The helper receives `live` so tests remain hermetic. Initial
leaders use the already-read live value and immediately service a persisted
refresh request whose completion counter is behind. Non-leaders arm only the
leadership retry timer and subscriptions.

Catalogue restore accepts the role explicitly. Active leaders retain the
existing load/rebuild, vanished-entry prune/save, and thumbnail reconciliation.
Non-leaders call `Catalogue::load_or_rebuild` only.

### Runtime gates

Gate every automatic or destructive entry point on `is_active_leader()`:

- `RefreshDue`, `ShuffleDue`, `LockEvent`, and `LockPokeDue`, including valid
  generations;
- both the success tail and the early error-retry return in `finish_refresh`;
- refresh/shuffle rearming after applies and refreshes;
- watcher-driven and direct retention pruning;
- startup/finish thumbnail reconciliation and `ThumbnailsReady` accent retry;
- `start_accent_compute`, plus defensive drops for `AccentComputed` and
  `AccentWriteFinished`;
- all full-entry `set_config` call sites reachable from settings controls.

`RefreshNow` is local only for the active leader; a non-leader sends the peer
request. A non-leader failed `ApplyImage` skips `prune_immediately` because its
catalogue may be stale. It refreshes read-only state on the next popup reload.

Non-leader shuffle/interval/retention changes update memory only after their
raw per-key persist succeeds (or preserve the established memory-only behavior
for ordinary settings when no context exists), and never arm or prune locally.
The non-leader accent toggle is stricter: without a persistable config it warns
and remains unchanged, because displaying an enabled state that no leader can
consume would be false. With a context, it persists the raw flag and updates
the toggler, but never snapshots, computes, restores, or adopts snapshot fields.

### Takeover

Use a one-shot `LeadershipTick(u64)` at approximately 60 seconds. A failed
attempt re-arms with a new generation; stale/already-active ticks do nothing.
On the acquisition edge:

1. mark leadership as owned but not ready and invalidate the retry generation;
2. spawn a blocking hydration read for `AppletConfig`, `CoordinationConfig`,
   and `wallpaper::current_wallpaper()`;
3. if config/coordination watcher events arrived during the read, discard and
   repeat it;
4. otherwise adopt the full accent trio only at this boundary, mark active,
   arm leader duties, and service any refresh request newer than its recorded
   completion.

No filesystem or cosmic-config read is added inline to `Application::update`.
The logind subscription remains alive in every process; only its handlers are
gated, so takeover requires no D-Bus resubscription.

### Asynchronous non-leader reload

Opening a non-leader popup immediately creates the surface and, when no peer
refresh is pending, batches a blocking read of the catalogue and live
wallpaper. `NonLeaderReloaded { generation, ... }` is adopted only if the
window is still non-leader and its generation is current. A successful local
apply bumps that generation. A peer refresh completion/timeout uses the same
reload helper. Leaders keep their in-memory-authoritative catalogue.

## What Goes Where

- Implementation Steps contain repository changes and test gates.
- Post-Completion contains real COSMIC/compositor verification without
  checkboxes.

## Implementation Steps

### Task 1: Leadership lock primitive

**Files:**

- Create: `src/leader.rs`
- Modify: `src/main.rs`
- Test: `src/leader.rs`

- [x] implement the leader-lock API and fail-open logging described above;
      retain the
      opened file on both lock-winner and `WouldBlock` paths so the loser can
      retry the same handle
- [x] implement the blocking-pool-only coordination critical-section helper;
      it must release on success, closure error, and unwind/process exit
- [x] declare `mod leader;`
- [x] **success tests:** two opens in one tempdir produce one leader; dropping the winner
      lets the loser acquire exactly once; still-blocked/already-leader retries
      return false; concurrent coordination critical sections serialize;
      `forced(true)`, `forced(false)`, and defaults report the intended role
- [x] **failure/edge tests:** a closure error releases `coordination.lock`; the
      deterministic file-as-directory leader-lock error resolves to leader and
      logs without using real user paths
- [x] run `PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig cargo test leader`
      and `just check`; both must pass before Task 2

### Task 2: Add the coordination config protocol

**Files:**

- Modify: `src/config.rs`
- Test: `src/config.rs`

- [ ] add `CoordinationConfig`, `PeerApplyNotice`,
      `PeerRefreshCompletion`, and the success/network/disk outcome enum with
      defaults and serde/config derives; keep keys disjoint from `AppletConfig`
- [ ] add tempdir-rooted load helpers that degrade missing/corrupt keys to
      defaults without rewriting them
- [ ] add blocking-worker helpers for incrementing `refresh_request`, writing
      `apply_notice`, and recording completion; reject `u64::MAX`
- [ ] **success tests:** request counters remain strictly monotonic under
      concurrent writers; apply-notice and completion values round-trip as
      atomic single keys
- [ ] **failure/edge tests:** missing/corrupt keys load defaults without
      writes; max-counter allocation and config writes fail without regressing
      the persisted mailbox
- [ ] test an `AppletConfig::write_entry` leaves all coordination key bytes
      unchanged
- [ ] run `PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig cargo test config`
      and `just check`; both must pass before Task 3

### Task 3: Thread leadership through startup and make restoration role-safe

**Files:**

- Modify: `src/app.rs`
- Test: `src/app.rs`

- [ ] add `Leadership`, ready/hydration state, coordination state, and the new
      timer/generation fields to `Window`; keep existing fixtures leader-ready
      through a readiness wrapper whose `Default` is ready, while production
      initializes every field explicitly
- [ ] acquire the lock before catalogue restore; leader restore keeps today's
      prune/save/reconcile behavior, non-leader restore is read-only
- [ ] extract `arm_leader_duties(CurrentWallpaper)` and call it only for the
      initial active leader; a non-leader arms only takeover retry
- [ ] add the second `CoordinationConfig` subscription without changing the
      lockwatch subscription or popup surface routing
- [ ] **success tests:** the leader init-shaped helper arms exactly today's
      duties and consumes an outstanding peer refresh; the non-leader arms only
      takeover retry
- [ ] **failure/edge tests:** non-leader restoration with missing, corrupt, or
      stale catalogue data performs no catalogue save, prune, or cache mutation
- [ ] confirm the existing `Window::default()` suite retains leader behavior
      without fixture churn
- [ ] run focused startup/restore tests and `just check`; both must pass before
      Task 4

### Task 4: Gate runtime automatic, destructive, and persistence paths

**Files:**

- Modify: `src/app.rs`
- Test: `src/app.rs`

- [ ] add every runtime gate listed in Technical Details, including
      `finish_refresh`'s early error-retry branch and non-leader apply-failure
      pruning
- [ ] route non-leader shuffle/interval/retention persists through raw per-key
      writes; skip local timer changes and pruning
- [ ] keep leader control behavior byte-for-byte unchanged
- [ ] **success tests:** current-generation automatic messages still act for a
      ready leader, and leader settings retain existing timer/prune behavior
- [ ] **failure/edge tests:** those same current-generation messages are
      dropped by a non-leader; both successful and failed refresh completions cannot re-arm a
      non-leader, and non-leader apply failure cannot prune
- [ ] test each non-leader setting persist leaves the on-disk
      `accent_snapshot`/`accent_last_written` bytes unchanged; cover missing and
      failing config contexts
- [ ] run focused timer/config/prune tests and `just check`; both must pass
      before Task 5

### Task 5: Proxy non-leader manual refresh and acknowledge completion

**Files:**

- Modify: `src/app.rs`
- Test: `src/app.rs`

- [ ] route non-leader `RefreshNow` through the blocking coordination-request
      helper; show pending only after a successful/coalesced request persist
- [ ] make an active leader consume an outstanding request, attaching it to an
      in-flight refresh or starting one; never start a second concurrent fetch
- [ ] after `RefreshFinished`, persist completion for the latest request the
      leader had observed, including success/network/disk outcome
- [ ] on a matching-or-newer completion, clear the requester's pending state,
      update the existing status class, invalidate its timeout, and request a
      read-only reload
- [ ] add a generation-guarded acknowledgement timeout that clears false
      pending state and reloads without starting work locally
- [ ] **success tests:** success/network/disk outcomes reach the requester;
      multiple requesters and a request arriving during a running refresh
      coalesce onto one fetch and all settle from a covering completion counter
- [ ] **failure/edge tests:** request-write failure never shows pending;
      completion-write failure settles through timeout/reload; stale or
      non-covering completion and timeout messages cannot clear a newer request
- [ ] run focused peer-refresh tests and `just check`; both must pass before
      Task 6

### Task 6: Gate accent ownership and notify the leader after peer applies

**Files:**

- Modify: `src/app.rs`
- Test: `src/app.rs`

- [ ] gate `start_accent_compute` at the choke point and defensively drop
      accent compute/write completions on non-leaders
- [ ] implement the non-leader accent toggler proxy: raw flag only, immediate
      state only after persist success, no lifecycle; warn and remain unchanged
      without a config context
- [ ] keep non-leader `ConfigUpdated` display-only for the accent flag using
      the existing fresh-disk rule; never adopt snapshot/last-written or route
      `set_accent_enabled`
- [ ] after a successful non-leader apply, update local navigation state,
      invalidate stale reloads, and persist `apply_notice` off the UI thread;
      do not arm shuffle or accent locally
- [ ] on the active leader, validate a new notice by reading live wallpaper on
      the blocking pool, drop stale notice completions, and route a current
      file through `on_apply_success`
- [ ] **success tests:** a peer apply produces one leader
      current/ColdStart/accent update and a non-leader toggle changes only the
      raw flag
- [ ] **failure/edge tests:** absent/failing config, stale watcher/notice
      completions, no-file/unknown live wallpaper, and apply-notice persist
      failure run no non-leader lifecycle and cannot regress leader state
- [ ] replay the original two-window accent fight: non-leader config echoes and
      every compute entry point remain inert, while a peer apply notice causes
      only the leader to recompute and the enabled state/snapshot survive
- [ ] run focused accent/peer-apply tests and `just check`; both must pass before
      Task 7

### Task 7: Implement asynchronous takeover hydration

**Files:**

- Modify: `src/app.rs`
- Test: `src/app.rs`

- [ ] implement `LeadershipTick`, failed-attempt rearming, stale/already-active
      drops, and the owns-lock-but-not-ready transition
- [ ] load complete applet config, coordination state, and live wallpaper on
      the blocking pool; add generation/dirty-event guards and a pure
      completion handler accepting injected values
- [ ] on a valid completion, adopt disk state, become active, arm ordinary
      duties, and service an unacknowledged peer refresh request
- [ ] test takeover using two real `Leadership::acquire(tempdir)` values—not
      `forced(false)`—then drop the winner and verify the loser acquires once
- [ ] **success tests:** takeover adopts disk config/accent fields, recovers an
      outstanding request, and arms each leader duty exactly once
- [ ] **failure/edge tests:** config/coordination changes during hydration force
      a fresh read; stale hydration/timer messages, continued lockout, and a
      missing config context cannot arm from stale state
- [ ] run focused takeover tests and `just check`; both must pass before Task 8

### Task 8: Make non-leader popup and completion reloads asynchronous

**Files:**

- Modify: `src/app.rs`
- Test: `src/app.rs`

- [ ] add the generation-guarded read-only reload helper and completion
      message for catalogue plus live wallpaper
- [ ] batch reload with popup creation for a non-leader when no peer refresh is
      pending; never delay or bypass the popup ledger action
- [ ] reuse the helper after peer refresh completion/timeout and invalidate it
      after a successful local apply or takeover
- [ ] **success tests:** reload adopts added/dropped entries and applies
      `synced_current` after popup open and peer refresh settlement
- [ ] **failure/edge tests:** stale completion after apply/takeover,
      pending-refresh suppression, corrupt/missing catalogue fallback, and a
      leader popup cannot overwrite authoritative in-memory state
- [ ] run focused popup/reload tests and `just check`; both must pass before
      Task 9

### Task 9: Verify acceptance criteria

- [ ] audit every caller of `schedule_refresh`, `sync_shuffle`,
      `start_thumbnail_pass_over`, `start_accent_compute`,
      `accent_compute_for_current`, `on_apply_success`, `prune_immediately`,
      `finish_refresh`, poke arming, and full-entry `set_config`
- [ ] verify exactly one active instance owns automatic work and every
      thumbnail producer/sweep; non-leader refresh is a leader request, not a
      local pipeline
- [ ] verify settings and accent toggles work from either popup without stale
      full-entry writes, and a non-leader apply promptly updates leader
      current/ColdStart/accent state
- [ ] verify startup races yield one lock winner; leader death during accent or
      refresh work is reconciled by fresh takeover state without adopting stale
      async completions
- [ ] verify all tests use injected paths and no test/diagnostic can contact
      Bing or real COSMIC config/theme/state
- [ ] verify no popup ledger, dependency, or Fluent catalogue changes leaked
      into the implementation
- [ ] run `just check`

### Task 10: [Final] Update documentation

**Files:**

- Modify: `README.md`
- Modify: `CLAUDE.md`
- Move: `docs/plans/20260817-single-instance-leader.md` to
  `docs/plans/completed/`

- [ ] document multi-output single-owner behavior and transparent peer refresh
      routing in `README.md`
- [ ] update `CLAUDE.md` with `src/leader.rs`, the coordination config keys,
      active-leader gates, per-key non-leader persists, async takeover/reload,
      and the explicit non-leader exception to in-memory accent authority
- [ ] record any implementation deviation in this plan before marking tasks
      complete
- [ ] move the synchronized plan to `docs/plans/completed/`
- [ ] run `just check` after documentation changes

## Post-Completion

### Manual verification

- Install intentionally, restart the panel or session, and confirm one applet
  process per panel output but exactly one holder of
  `~/.local/state/io.github.ercling.CosmicBingWallpaper/leader.lock`.
- Trigger refresh from the non-leader popup. Its existing checking/error status
  must settle from the leader's completion, with one Bing request pipeline and
  one thumbnail producer in logs.
- With accent matching enabled, apply previous/next/newest from the non-leader.
  The wallpaper must change immediately, the leader must recompute the v2
  theme accent, and the toggle/snapshot must remain armed.
- Disconnect the monitor hosting the leader. Confirm its applet process exits,
  the kernel releases the lock, and the surviving applet becomes active within
  one retry interval (about 60 seconds), reloads disk/live state, and resumes
  refresh/shuffle/accent/lock-poke duties. Reconnect and confirm the new applet
  remains non-leader.
- Lock/unlock once and verify exactly one poke ladder.

### Accepted limitations

- Takeover latency is bounded by the retry interval plus state hydration. If
  COSMIC keeps the old output's applet process alive, that process continues
  to hold the lock and remains the functioning leader.
- A coordination persist failure is logged. Refresh requesters recover their
  UI through the acknowledgement timeout/reload; an apply-notice failure is
  reconciled at the next leader startup/takeover or ordinary recompute.
- A filesystem where advisory locking itself errors fails open and may yield
  multiple leaders; the warning distinguishes this from ordinary lock loss and
  behavior is no worse than the current release.
