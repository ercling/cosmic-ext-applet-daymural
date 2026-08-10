# Lock-screen wallpaper: poke cosmic-bg state on lock/resume (workaround for cosmic-greeter#511)

## Overview

The COSMIC lock screen usually shows the bundled default background instead of
the applet-applied wallpaper. The bug is in cosmic-greeter's locker
([cosmic-greeter#511](https://github.com/pop-os/cosmic-greeter/issues/511),
filed from this repo with the full trace); the user asked for a workaround
**in the applet**, without touching cosmic-greeter.

The workaround: the locker rebuilds its wallpaper cache on every **delivered**
cosmic-bg *state* update, and that rebuild works whenever lock surfaces
exist. So the applet listens for "screen just locked / system just resumed"
on the system D-Bus and then writes cosmic-bg's state with a **semantically
identical but value-different** `wallpapers` list (the normalizing toggle,
below). The locker's state handler (`locker.rs:1023-1027` at `epoch-1.5.0`)
then sets the new state, **re-reads the image bytes from the paths in it**
(`load_wallpapers_as_user()`), clears `surface_images` and rebuilds — while
the lock surfaces exist — so the real wallpaper appears on the lock screen
within a second or two.

### Why the write must change the value (plan-review finding, verified)

An identical-content rewrite does **not** work. The locker consumes the state
through `cosmic_config::config_state_subscription`, whose `Waiting` arm only
forwards an update `if !changed.is_empty()` after `update_keys`
(`cosmic-config/src/subscription.rs:123-136` at our pinned `8a017a15`;
`:125-135` at the greeter's `8cb10bf6` — **both revs verified to carry the
identical guard**), and the derive generates `update_keys` with a
value-equality guard — `if self.field != value { keys.push(..) }`
(`cosmic-config-derive/src/lib.rs:131-157`). inotify fires on any rewrite,
but an equal value produces no message, no cache clear, no rebuild.
(cosmic-config's *write* side has no equality skip — `ConfigSet for
Config::set` at `cosmic-config/src/lib.rs:489-497` always commits an
`AtomicFile` — but the read side dedupes.)

**Consequently cosmic-bg's own 5-minute churn does not heal a lock.** The
rotation tick rewrites the state file every `rotation_frequency` (300 s here;
observed live: identical 107-byte writes at 23:56:22, 00:01:23, 00:06:23) but
with a single-file source the value never changes, so the locker never hears
it. Only a *genuine* value change (the applet's daily auto-apply or shuffle
landing while locked, or a session restart) reaches it.

### The state file has a second writer, and it is read-modify-write

cosmic-bg's `save_state` (`cosmic-bg/src/wallpaper.rs:72-90`, pinned rev,
verified) does **not** overwrite the state with a canonical value: it
`get_entry`s the current list, and per layer either mutates the **first**
entry whose output name matches or pushes a new one — everything else is
written back verbatim. Three consequences the design must respect:

- an extra entry we leave in the list is **permanent** until something
  removes it — no tick "restores canonical";
- a tick that changes nothing writes back exactly what it read → deduped →
  *not* a delivered rebuild;
- after a genuine wallpaper change lands while our duplicate is present,
  cosmic-bg updates only the first match, leaving `[(out, new), (out, old)]`
  — a **stale second entry** that would otherwise survive forever, and whose
  path retention-prune eventually deletes (the greeter's shared loader then
  logs `failed to read wallpaper` per load, and the login daemon ships an
  extra ~5 MB blob over the bus while the file still exists).

So the transform must be **self-healing**, not just reversible.

### The normalizing toggle

The poke must write a `wallpapers` value that (a) compares unequal to the
previous one under Rust `PartialEq`, (b) renders identically, and (c) every
state consumer tolerates — including cosmic-bg's RMW above. Chosen
transform, pure and always value-changing:

```
toggle_wallpapers(list):
    empty                       -> None (skip: nothing to heal; never write
                                   a default over a value that failed to read)
    normalized = first entry per output name, original order preserved
    normalized != list          -> Some(normalized)      // cleanup poke
    else (list is canonical)    -> Some(list + [clone of last entry])
```

- **Always a change**: either the normalization removed something, or a
  duplicate of the last entry is appended (a `Vec` of different length is
  unequal under any equality).
- **Self-healing**: the cleanup arm removes both our own resting duplicate
  *and* the stale `[(out, new), (out, old)]` shape cosmic-bg's RMW can leave
  — every later poke normalizes, so artifacts are bounded at one extra entry
  per output and live only until the next lock/resume.
- **Invariant — the first entry per output name is never altered or
  reordered, and additions go at the end.** Every consumer reads
  first-match-wins: the locker `break`s after the first `Path` match
  (`common.rs:154-176` — the upstream `//TODO: what to do about duplicates?`
  is about the insert, not iteration), and cosmic-bg's own `save_state`
  mutates the first match. A "prepend" variant would break rendering.
- **Round-trip**: on a canonical list, toggling twice is the identity
  (append then cleanup). Stated over the transform's own output only — the
  field contains legitimate shapes that normalize lossily, e.g. two unnamed
  outputs both keyed `""` (`save_state` keys by
  `output_info.name.unwrap_or_default()`), which normalize to one entry;
  that removal is itself a delivered change and cosmic-bg re-pushes what it
  needs on its next tick. Harmless, but the identity claim must not be
  tested against arbitrary field shapes.
- **Rest shape is explicitly accepted**: a ladder interrupted between its
  two pokes (relock inside the 3 s window bumping the generation, panel
  restart, failed write) leaves the duplicated shape at rest. That is a
  normal outcome, tolerated by every consumer below, and cleaned by the next
  poke — do **not** document a "the second poke always restores canonical"
  claim; parity is not guaranteed.

**State consumers, enumerated** (all verified at the installed/pinned revs):
the locker (first-match `break`, above); cosmic-bg (`save_state` RMW, above;
the state is never read for rendering); cosmic-greeter's login daemon and
locker share `load_wallpapers_as_user` (`daemon/src/lib.rs:76-102`), which
**dedupes by path** (`!bg_path_data.contains_key(path)`) and `retain`s to
listed paths — a duplicate entry costs nothing there; `cosmic-workspaces`
subscribes to the same state and merely stores it
(`cosmic-workspaces-epoch/src/main.rs:935-936`, `1118-1121`);
`cosmic-settings` embeds the `wallpapers` key read-only. GDM (this
machine's DM) never sees cosmic-bg state.

**Rejected value transforms** (recorded so nobody re-walks them):
- *Identical rewrite* — deduped, see above.
- *Path alias* (`/./home/…`, `//home/…`, trailing slash): defeated by
  `Path`/`PathBuf` `PartialEq`, which compares `Components` and normalizes
  exactly those forms (`.` segments, duplicate/trailing separators; note
  `..` is *not* normalized, so this is not a universal law — but every
  natural alias is dead, and the entry toggle is strictly better).
- *Two-write ladder* (empty-then-real): delivery races the second write — if
  the watcher processes event 1 after write 2 landed it reads the final
  value twice and dedupes both; timing-dependent, untestable.
- *Symlink alias in our state dir*: works but adds filesystem state, cleanup,
  and cross-consumer path assumptions for no gain over the entry toggle.
- *Rewriting cosmic-bg config*: same dedupe on cosmic-bg's own subscription
  unless a user-owned field is actually changed — which `updated_entry`
  deliberately never does; also makes cosmic-bg redo real work.
- *Lowering `rotation_frequency`*: clobbers a user field and (per the RMW +
  dedupe) wouldn't deliver anyway for a single-file source.

### Root cause (systematic-debugging result, 2026-08-10/11)

Fully traced in `docs/plans/completed/20260808-ux-polish-lockscreen-i18n.md`
(Task 1, second correction), re-verified this session against the fetched
`epoch-1.5.0` sources (the installed Fedora build):

1. Every **delivered** cosmic-bg state update makes the locker reload
   wallpaper bytes, clear `surface_images` and rebuild
   (`locker.rs:1023-1027`).
2. The rebuild **silently skips** any surface missing from `surface_names`
   (`common.rs:148-150`).
3. Unlocking removes those ids (`locker.rs:1004`, `1133`); locking re-inserts
   them (`locker.rs:968`) **but never rebuilds** — only `init`,
   `OutputEvent::Created` and the state handler rebuild.
4. With `surface_images` empty, `view_window` serves the bundled
   `res/background.jpg` (`locker.rs:1167-1172`).

**Why it "sometimes works after boot"** (the user's question, answered from
logs + config + sources): the **first lock after login** is correct —
`surface_names` is still populated from `OutputEvent::Created` at session
start, so the startup state delivery built the cache while the names existed.
Every lock after the first unlock shows the default, and stays default no
matter how long it is up (the churn is deduped) — **unless** a genuine
wallpaper change (daily auto-apply, shuffle) happens to land mid-lock. That
matches "in most cases it does not apply, but sometimes it works, after a
while it shows the default".

### Lock-detection evidence (this machine, COSMIC 1.5.0)

- The lock keybinding is `LockScreen: "loginctl lock-session"`
  (`/usr/share/cosmic/com.system76.CosmicSettings.Shortcuts/v1/system_actions`,
  no user override) and `cosmic-idle` also embeds `loginctl lock-session` —
  both paths make logind emit the **`Lock` signal on the session object**.
- `cosmic-greeter` embeds `org.freedesktop.login1.Session`,
  `GetSessionByPID` **and `PrepareForSleep`** — it locks on suspend directly
  (`logind.rs` in its sources uses the session resolved from
  `parent_id()`), so a suspend lock may never emit a session `Lock` signal.
  The applet must watch **both** the session's `Lock` signal and the
  manager's `PrepareForSleep`.
- No binary contains `LockedHint`/`SetLockedHint` — the property is never set
  on this system, so property watching is a dead end; signals only.
- Nothing calls `UnlockSession` either (the greeter unlocks the compositor,
  not logind), so the session's `Unlock` signal likely never fires here —
  the design must not depend on it (there is no `Unlocked` event; a
  post-unlock poke is a harmless invisible toggle).

## Context (from discovery)

- Files involved: `src/app.rs` (subscription, message routing, generation
  counter), `src/wallpaper.rs` (cosmic-bg plumbing home), new
  `src/lockwatch.rs`, `Cargo.toml` (+`zbus`), `README.md`, `CLAUDE.md`.
- **Two `cosmic-config` instances** (verified in `Cargo.lock`: our pinned
  `?rev=8a017a15…` and cosmic-bg-config's unpinned `#8a017a15…` — same
  commit today, distinct packages): `cosmic_bg_config::state::State`'s
  `get_entry`/`write_entry` come from the *foreign* instance's
  `CosmicConfigEntry` trait, which this crate **cannot name** (the constraint
  already recorded at `src/wallpaper.rs:12-15`). The poke therefore uses
  **our** pinned instance with raw keys:
  `cosmic_config::Config::new_state(cosmic_bg_config::NAME,
  cosmic_bg_config::state::State::version())` +
  `ConfigGet::get::<Vec<(String, Source)>>(.., "wallpapers")` /
  `ConfigSet::set` — `NAME`, `State::version()` and `Source` are public, the
  serde/RON output is identical (same commit, same `PrettyConfig::new()`),
  and the handle injects via `Config::with_custom_path` for tests. Adding a
  direct unpinned `cosmic-config` git dep to unify instances is **rejected**:
  it violates the repo's rev-pinning rule for no benefit.
- Patterns to follow: one-shot generation-counter timers
  (`timer_generation`/`shuffle_generation`, stale ticks ignored); async tasks
  end in a completion `Message`; injectable `Config::with_custom_path` for
  hermetic tests (`config.rs:115`, `accent.rs:327-338`, the `accent_window`
  harness at `app.rs:4073-4094`); **`settle_accent_tasks`
  (`app.rs:4167-4182`) as the model for testing spawned work** — a `Task`
  returned from `update()` is never polled in unit tests (`drop(window.
  update(..))`), so the poke needs its own settle helper (Task 4).
- **Ordering dependency**: `docs/plans/20260810-popup-destroy-order-crash.md`
  is still open on this branch and also edits `app.rs`
  `update()`/`subscription()` (popup ledger). Finish/merge that work first;
  this plan's app.rs wiring must not touch the ledger paths.
- i18n: **no user-visible strings** — no FTL changes, and none may be added
  without touching all 73 catalogues.

## Development Approach

- **Testing approach**: repo convention — TDD for the pure decision logic
  and the injectable-Config write path; the D-Bus stream and iced wiring
  stay thin and untested with an explicit exemption comment (same class as
  `wallpaper.rs::apply`).
- Complete each task fully before moving to the next; small, focused changes.
- **CRITICAL: every task MUST include new/updated tests** for code changes in
  that task (success and error scenarios).
- **CRITICAL: all tests must pass (`just check`) before starting next task.**
- **CRITICAL: update this plan file when scope changes during implementation.**
- Tests never touch real user config/state — tempdir-rooted
  `Config::with_custom_path` only; nothing may write the real
  `~/.local/state/cosmic/com.system76.CosmicBackground`.

## Testing Strategy

- **Unit tests**: required for every task (see above). No e2e framework;
  iced view/subscription code exempt per repo convention.
- **Mechanism proof, hermetic**: a test defines a local mirror struct
  `#[derive(Default, CosmicConfigEntry)] struct MirrorState { wallpapers:
  Vec<(String, Source)> }` (`Default` is required — the generated
  `get_entry` calls `Self::default()`; bring
  `cosmic::cosmic_config::CosmicConfigEntry` into scope; the derive is
  available — `macro` is a default cosmic-config feature and `config.rs:16`
  already uses it) on a tempdir config, asserting through `update_keys` —
  the exact guard the locker's subscription uses — that (a) an identical
  rewrite reports **no** changed keys (proving the dedupe premise) and (b) a
  toggled write reports `wallpapers` changed and round-trips. The guard is
  verified identical at both our `8a017a15` and the greeter's `8cb10bf6`
  libcosmic revs, so the residual rev-mismatch risk is small; the live lock
  check in Post-Completion is still the end-to-end word.
- The actual lock-screen behaviour is only verifiable by hand — recorded
  under Post-Completion (note there: the locker's success log line is
  info-level and **invisible by default**).

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- keep plan in sync with actual work done

## Solution Overview

1. **`src/lockwatch.rs`** — the lock-event domain:
   - `LockEvent { Locked, Resumed }` (no `Unlocked` — the signal likely never
     fires here, and a late poke is an invisible toggle; see evidence).
   - `POKE_DELAYS: [Duration; 2] = [1 s, 4 s]` — retry ladder: the first poke
     can race the locker inserting `surface_names` (`locker.rs:968`); the
     second is the safety net on a slow lock. Both pokes are full toggles —
     a "normalize-only" final poke was considered and rejected: if the first
     poke fired too early and the second found an already-canonical list it
     would write nothing, get deduped, and the heal would be lost.
   - Pure, tested `toggle_wallpapers(Vec<(String, Source)>) ->
     Option<Vec<(String, Source)>>` implementing the normalizing toggle
     exactly as specified in the Overview.
   - Pure, tested `sleep_edge_to_event(bool) -> Option<LockEvent>`
     (`false` → `Resumed`, `true` → `None` — a write racing suspend is
     useless).
   - `subscription() -> Subscription<LockEvent>`: zbus system-bus stream.
2. **`src/wallpaper.rs`** — `poke_state(config: &cosmic_config::Config) ->
   Result<bool, WallpaperError>` (returns whether a write happened): fresh
   raw-key read per poke (no stale write-back if the wallpaper changed since
   the event; a read **error** also skips — the alternative writes a default
   over a real value), apply `toggle_wallpapers`, `ConfigSet::set` the result
   back. Thin prod wrapper `poke_state_handle() -> Option<Config>` building
   `Config::new_state(NAME, State::version())` once in `init` (note:
   `new_state` `create_dir_all`s cosmic-bg's state dir as a side effect —
   harmless, but worth the comment on machines that never ran cosmic-bg).
3. **`src/app.rs`** — wiring: `lockwatch::subscription()` merged via
   `Subscription::batch`; messages `LockEvent(lockwatch::LockEvent)`,
   `LockPokeDue(u64)`, `LockPokeFinished(bool)`; fields
   `lock_poke_generation: u64` and `poke_config: Option<Config>` (built in
   `init` from `poke_state_handle()`, **injected tempdir-rooted by tests**,
   mirroring `config_context` in the `accent_window` harness). On
   `LockEvent`: bump the generation (atomically replaces any pending ladder)
   and spawn one sleeping task per `POKE_DELAYS` entry carrying the new
   generation. On `LockPokeDue(gen)`: drop if stale, else run
   `wallpaper::poke_state` on the cloned handle inside a spawned async task
   resolving to `LockPokeFinished(wrote)`, which only logs (`debug` on
   success, `warn` on error) — pokes never touch other applet state. (Async
   rather than inline: `apply` does run inline today, but a state-dir write
   under I/O pressure is exactly the 2026-08-08 freeze shape — cheap
   insurance, and the completion message keeps the repo's
   every-async-path-ends-in-a-message rule.)

## Technical Details

- Poke write = single raw `wallpapers` key under
  `~/.local/state/cosmic/com.system76.CosmicBackground/v1/` via
  cosmic-config's atomic temp+rename. Racing cosmic-bg's own tick is
  **bounded-loss, not zero-loss** (review correction): both writers are
  atomic and whichever value lands last is valid and renders identically
  (first-match invariant), but a tick whose RMW write lands after a rung's
  toggled write and *before* the locker's watcher reads the file restores
  the pre-toggle value, so that rung's delivery is deduped — the other rung,
  or the next lock/resume, heals. Artifacts are bounded and cleaned by the
  next poke's normalization — **not** by cosmic-bg, whose `save_state`
  is read-modify-write and preserves whatever shape it finds.
- Generation counter follows the documented repo model (`app.rs:6-8`): bump
  = cancel; a `LockPokeDue` carrying a stale generation is dropped. Rapid
  re-locks: the last event's ladder wins.
- zbus: `zbus = { version = "5", default-features = false, features =
  ["tokio"] }` — matches how libcosmic enables it (its `tokio` feature sets
  `zbus?/tokio`); default features would drag `async-io` in. Verify
  `Cargo.lock` is **unchanged except the new direct-dep edge** (it already
  contains zbus 5.18.0).
- Session resolution, in order: `Manager.GetSessionByPID(std::process::id())`
  (works here — verified live: panel PID resolves to `session-2.scope` →
  `/org/freedesktop/login1/session/_32`), falling back to
  `$XDG_SESSION_ID` → `Manager.GetSession`. **Distinguish failure classes**:
  transient D-Bus errors retry with ~30 s backoff *inside* the stream; "this
  process has no logind session" (e.g. a session under `user@.service` with
  no session scope) logs `warn` **once** and parks the stream — an infinite
  warn loop is not acceptable, and the subscription must never return `None`
  (iced does not restart a finished subscription; model:
  `futures::stream` with an inner loop / `pending()` park).
- Subscription identity: a `struct LockWatchSubscription;` +
  `Subscription::run_with(TypeId::of::<LockWatchSubscription>(), ..)` —
  cosmic-greeter's own `logind.rs:50-52` is the model.
- Minimal hand-written `#[zbus::proxy]` traits: `org.freedesktop.login1`
  Manager (`GetSessionByPID`, `GetSession`, `PrepareForSleep` signal) and
  Session (`Lock` signal). No generated-code drop-in.
- Steady state: the subscription yields nothing until a signal arrives — no
  polling, no wakeups.

## What Goes Where

- **Implementation Steps** (`[ ]`): code, tests, docs in this repo.
- **Post-Completion** (no checkboxes): manual lock-screen verification,
  upstream follow-up.

## Implementation Steps

### Task 1: Lock-event domain, normalizing toggle, and hermetic mechanism proof (`lockwatch.rs`)

**Files:**
- Create: `src/lockwatch.rs` (module + `#[cfg(test)]` tests)
- Modify: `src/main.rs` (declare module)

- [x] create `src/lockwatch.rs` with `LockEvent { Locked, Resumed }`,
      `POKE_DELAYS`, `toggle_wallpapers`, `sleep_edge_to_event` — each
      doc-commented with the cosmic-greeter#511 rationale, the dedupe
      finding, the cosmic-bg RMW finding, the first-entry/append-at-end
      invariant, and why there is no `Unlocked` variant
- [x] write tests for `toggle_wallpapers`:
      `canonical_list_gets_a_trailing_duplicate` (also asserts `!=` input;
      cover single-output, multi-output, and a `Color` source),
      `toggling_twice_on_a_canonical_list_is_the_identity`,
      `first_entry_per_output_and_order_are_preserved`,
      `stale_post_change_shape_is_normalized_away`
      (`[(out,new),(out,old)]` → `[(out,new)]`),
      `unnamed_output_duplicates_normalize_to_one` (the legitimate
      `[("",s),("",s)]` cosmic-bg shape — removal is still a change),
      `empty_list_is_not_poked`
- [x] write test `sleep_edge_to_event` (`false` → `Resumed`, `true` → `None`)
- [x] write the mechanism-proof test (Testing Strategy): mirror
      `#[derive(Default, CosmicConfigEntry)]` struct on a tempdir
      `Config::with_custom_path`; assert an identical rewrite reports no
      changed keys and a toggled write reports `wallpapers` changed — this
      pins the dedupe premise against future libcosmic bumps
- [x] run `just check` — must pass before task 2

### Task 2: State poke with injectable Config (`wallpaper.rs`)

**Files:**
- Modify: `src/wallpaper.rs`

- [x] add `poke_state(config: &cosmic_config::Config) -> Result<bool, WallpaperError>`:
      raw `ConfigGet::get::<Vec<(String, Source)>>(config, "wallpapers")` —
      skip (Ok(false)) on read error or `None` from `toggle_wallpapers` —
      else `ConfigSet::set` the toggled list; doc-comment cites the
      two-instance constraint (module comment, `Cargo.lock`) and the
      value-change requirement
- [x] add `poke_state_handle() -> Option<cosmic_config::Config>` (prod
      handle via `Config::new_state(cosmic_bg_config::NAME,
      State::version())`), with the same untestable-context exemption
      comment style as `apply`, noting the `create_dir_all` side effect
- [x] write tests (tempdir `Config::with_custom_path`, assert by **inode**
      (`MetadataExt::ino`) — never mtime, the flakiness class `thumbs.rs`
      abandoned): `poke_toggles_the_wallpapers_key` (inode changed, value =
      toggled input, parses as `Vec<(String, Source)>`),
      `second_poke_restores_the_canonical_value`,
      `poke_normalizes_a_stale_two_entry_shape`,
      `poke_skips_an_empty_state` (returns false, file inode unchanged),
      `poke_skips_an_absent_or_unreadable_key` (no file created),
      `poke_writes_the_freshly_read_value` (externally rewrite the key
      between two pokes; the second poke toggles the *new* value)
- [x] run `just check` — must pass before task 3

### Task 3: logind subscription stream (`lockwatch.rs`)

**Files:**
- Modify: `src/lockwatch.rs`
- Modify: `Cargo.toml` (add zbus — first task that uses it)

- [x] add `zbus = { version = "5", default-features = false, features = ["tokio"] }`;
      verify `Cargo.lock` unchanged except the new direct-dep edge
      (verified: the diff adds only `"zbus"` to this crate's dependency
      list; zbus stays 5.18.0)
- [x] add minimal `#[zbus::proxy]` traits (Manager: `GetSessionByPID`,
      `GetSession`, `PrepareForSleep` signal; Session: `Lock` signal)
      (`gen_blocking = false` on both — the blocking API is never used)
- [x] add `subscription() -> Subscription<LockEvent>` per Technical Details:
      `run_with` + `TypeId` identity, session resolution with the
      `GetSessionByPID` → `$XDG_SESSION_ID` fallback, merged `Lock` +
      `PrepareForSleep` streams mapped through `sleep_edge_to_event`,
      transient-vs-permanent failure handling (inner ~30 s backoff loop vs
      warn-once-and-park), stream never returns `None`, `tracing` `debug`
      per event
- [x] mark the stream fn with the repo's untestable-plumbing exemption
      comment (system bus cannot be faked hermetically; every decision it
      makes lives in the already-tested pure fns)
- [x] run `just check` — must pass before task 4

### Task 4: Wire pokes into the app (`app.rs`)

**Files:**
- Modify: `src/app.rs`

- [x] add `lock_poke_generation: u64` and `poke_config: Option<Config>` to
      `Window` (`poke_config` built in `init` via
      `wallpaper::poke_state_handle()`); messages `LockEvent(..)`,
      `LockPokeDue(u64)`, `LockPokeFinished(bool)`
- [x] handle `LockEvent`: bump generation, spawn one sleeping task per
      `POKE_DELAYS` entry carrying it (shape of `arm_refresh_timer`);
      handle `LockPokeDue`: drop stale, else clone `poke_config` into a
      spawned async task running `wallpaper::poke_state`, resolving to
      `LockPokeFinished`; handle `LockPokeFinished`: log only — never touch
      other applet state, never touch the popup-ledger paths
      (the stale/handle-less decision is factored as `Window::due_lock_poke`
      so the settle helper shares it; the poke body is the shared
      `run_lock_poke`, `spawn_blocking` inside a `cosmic::task::future` like
      the accent tasks)
- [x] merge `lockwatch::subscription()` into `subscription()` with
      `Subscription::batch`, mapped into `Message::LockEvent`
- [x] add the test-side `settle_lock_pokes(&mut Window)` helper in the shape
      of `settle_accent_tasks` (`app.rs:4167-4182`): run
      `wallpaper::poke_state` on the injected handle synchronously, feed
      `LockPokeFinished(wrote)` back through `update` — a `Task` returned
      from `update()` is never polled in unit tests, so on-disk assertions
      must go through the settle helper, and message-level tests assert
      **generation bookkeeping only**
      (signature deviation: `settle_lock_pokes(&mut Window, generation) ->
      bool` — the explicit generation lets the staleness tests settle a
      *dead* rung through the production `due_lock_poke` decision instead of
      duplicating it in the test)
- [x] write tests (harness style of `accent_window`, `poke_config` injected
      tempdir-rooted): `settled_lock_poke_toggles_the_injected_state`
      (value actually toggled on disk, via `settle_lock_pokes`),
      `a_stale_poke_due_is_a_noop` (generation bookkeeping; injected file's
      inode unchanged), `a_second_lock_event_invalidates_the_first_ladder`,
      `resumed_pokes_like_locked`, `poke_with_no_config_handle_is_a_noop`
- [x] run `just check` — must pass before task 5

### Task 5: Verify acceptance criteria

- [x] re-read Overview: lock signal → ladder of two pokes; resume → same;
      each poke toggles the value (never an identical write); the toggle
      normalizes before appending (self-healing); empty/unread state never
      written; generation replaces pending ladders; no new user-visible
      strings (i18n guards untouched)
      (verified against code: both `LockEvent` variants route to
      `arm_lock_pokes` → one task per `POKE_DELAYS` rung; `poke_state`
      fresh-reads then `toggle_wallpapers` — normalize-first, empty → `None`,
      read error → `Ok(false)` skip; `due_lock_poke` drops stale
      generations; `git diff main...HEAD -- i18n/` empty, no `fl!` in
      `lockwatch.rs`)
- [x] verify edge cases: D-Bus unavailable at startup (applet still works,
      warn logged once or retry armed per failure class); no-session
      environment parks quietly; rapid lock/relock (last ladder wins,
      duplicated rest shape accepted and cleaned by the next poke)
      (verified: `WatchEnd::Transient` → 30 s backoff loop, debug-logged;
      `WatchEnd::NoSession` → one `warn` then `future::pending()` park,
      stream never finishes; relock covered by
      `a_second_lock_event_invalidates_the_first_ladder` and
      `stale_post_change_shape_is_normalized_away`)
- [x] run full suite: `just check` (fmt + clippy `-D warnings` + tests)
      (290 tests pass, fmt + clippy clean)
- [x] run the binary once (`cargo run`) with
      `RUST_LOG=cosmic_bing_wallpaper=debug` **with a warm
      `~/Pictures/BingWallpaper`** (a cold start triggers a real Bing fetch
      and a wallpaper apply ~5 s in — CLAUDE.md) and confirm the
      subscription connects and logs no errors; do **not** trigger a real
      lock from automation — that locks the user's session (manual check is
      Post-Completion)
      (ran 15 s under `timeout`, warm dir with 8 images: logged
      `logind lock watch connected session=/org/freedesktop/login1/session/_32`,
      no lockwatch/poke errors, no fetch; no lock triggered)

### Task 6: [Final] Update documentation

- [x] README.md: **correct** the existing lock-screen paragraph (lines
      ~122-142) — it still claims "a lock that is up when a state write
      lands switches to the real wallpaper mid-lock", which the dedupe
      finding falsifies for identical-value churn — and describe the
      workaround: on lock/resume the applet writes a value-toggled cosmic-bg
      state so the locker rebuilds; the lock screen may show the default for
      ~1-4 s before healing
- [x] CLAUDE.md: add `src/lockwatch.rs` to the architecture list (event
      sources, ladder, generation cancel, the normalizing toggle and *why*:
      the subscription/derive dedupe guard **and** cosmic-bg's RMW
      `save_state` — cite both; the first-entry/append-at-end invariant;
      the accepted duplicated rest shape); note the workaround
      self-neutralizes once cosmic-greeter#511 ships (the toggle still
      fires, the extra rebuild is harmless)
- [x] update `docs/plans/completed/20260808-ux-polish-lockscreen-i18n.md`
      with a short pointer correction (its step 1-2 "every state write
      reaches the locker" holds only for *changed* values — link this plan)
- [x] move this plan to `docs/plans/completed/`

## Post-Completion

**Manual verification** (interactive session required):
- `just install`, restart the panel applet, then: lock via Super+Escape →
  wallpaper should appear within ~1-4 s (a brief default-background flash is
  expected and accepted); unlock, lock again → same; suspend, wake →
  wallpaper within ~1-4 s of the resume; leave locked across a shuffle →
  still correct. **The visual check is the primary proof.**
- Journal signals, calibrated: the locker's success line `updating wallpaper
  for "<output>"` (`common.rs:152`) is **info-level and invisible by
  default** — the locker's `EnvFilter` defaults to WARN. To see it, export
  `RUST_LOG=cosmic_greeter=info` into the systemd *user* environment and
  re-login (the locker is long-lived and inherits the session env). Without
  that, the usable default-visibility signal is the **absence** of the
  warn-level `output {}: failed to find wallpaper data for source {:?}`
  (`common.rs:165-170`) after pokes. With
  `RUST_LOG=cosmic_bing_wallpaper=debug`, our own poke logs bracket the
  window.

**External follow-up:**
- Watch [cosmic-greeter#511](https://github.com/pop-os/cosmic-greeter/issues/511);
  when the upstream fix ships in Fedora's cosmic-greeter, the poke becomes a
  redundant extra rebuild — consider removing it (or leave it; it is
  self-neutralizing) and update README/CLAUDE.md accordingly.
- The login-screen (GDM on this machine) can never show the Bing wallpaper —
  separate, already-documented limitation; out of scope here.
