# Fix the out-of-order `xdg_popup` destroy crash

## Overview

The applet is killed by the compositor with

```
io.github.ercling.CosmicBingWallpaper: wl_surface#90: error 2:
    xdg_popup was destroyed while it was not the topmost popup
io.github.ercling.CosmicBingWallpaper: Error: EventLoop(ExitFailure(1))
io.github.ercling.CosmicBingWallpaper: exited with code 1
```

Eleven such protocol errors are in the user journal (most recently 2026-08-10
18:13:02 and 18:18:02, earlier 2026-08-09 22:27:49). Two user-visible symptoms
are tied to it:

- **Panel icons overlap.** cosmic-panel respawns the applet after the crash;
  its re-layout left our icon in the same slot as the applet to its left,
  rendered behind it ("two icons at the same place, the top one not ours").
  *This link is a hypothesis*: the crash and the overlap were observed in the
  same session and a respawn plausibly explains the layout, but nothing was
  captured proving that specific relayout. Treat its disappearance as
  corroboration, not as the acceptance test.
- **The accent toggle switches itself off after a restart.** `CLAUDE.md`
  documents a crash between the theme write and its `accent_last_written`
  record as accepted exposure — the next start then sees builders ≠ record and
  hits `Disarm { keep_snapshot: true }`. Reproduced against a sandboxed copy of
  the live config: with `accent_last_written = None` and the builders holding
  our written pair, startup logs `accent changed externally: disabling
  accent-from-wallpaper` and persists `accent_enabled = false`. The window is
  ~800 ms wide on this machine (theme keys landed 18:22:32.30 → 18:22:33.09,
  the record at 18:22:33.13). The exposure was accepted *on the premise that
  the applet does not crash*; removing the crash restores that premise.

**Scope of this plan: the crash only.** The accent state machine is not
touched — no new invariants, no write-ahead record.

### Why it happens

`window.popup` can have **two child popups at once**: the tooltip
(`crate::tooltip::tooltip`, `parent_id: window.popup`) and a dropdown menu
(`widget::dropdown::popup_dropdown(.., window.popup, ..)`). They are
**siblings** on the same popup stack, and xdg-shell only permits destroying the
topmost popup of a stack.

Three facts, all verified against the pinned libcosmic rev `8a017a1`:

1. **The runtime cannot clean up siblings.**
   `iced/winit/src/platform_specific/wayland/event_loop/state.rs:1392`
   (`Action::Destroy`) descends into children with
   `.position(|p| p.data.parent.wl_surface() == popup.popup.wl_surface())` —
   one child per level, then a child *of that child*. A second sibling is never
   collected, so destroying `window.popup` destroys the parent while the
   sibling is still mapped.
2. **We can never destroy a dropdown ourselves.**
   `src/widget/dropdown/widget.rs:572` mints the menu's id with
   `window::Id::unique()` into private widget state, and `surface::Action::Popup`
   carries only `Arc<Box<dyn Any + Send + Sync>>` (`src/surface/mod.rs:12`). The
   id first becomes visible in the `DestroyPopup(id)` the widget emits when it
   closes — too late to sequence anything.
3. **So the only workable rule is an invariant, not an ordering.**
   `window.popup` must never have more than **one** child popup at a time. With
   one child, the runtime's own descent already destroys deepest-first and is
   correct; with two, nothing on our side can fix it.

Our code currently violates that invariant: the tooltip arms on hover
regardless of what else is open, and its `on_close` (`src/tooltip.rs:103`) fires
`DestroyPopup(WINDOW_ID)` whenever the pointer leaves — including while a
dropdown sits above it. All tooltip call sites share one `WINDOW_ID`
(`src/tooltip.rs:37`).

### Residual path, not closed by this plan

Compositor-initiated dismissal is outside our reach.
`iced/winit/src/platform_specific/wayland/handlers/shell/xdg_popup.rs::done`
walks **up** the parent chain (`PopupParent::Popup`) and breaks at a
layer-surface/window parent — it never collects children. Our applet popup has
`grab: true`, the tooltip `grab: false` (so it is not in the grab chain), so a
click outside can make the compositor dismiss the parent while the tooltip
child is mapped, and `PopupClosed` only reaches us *after* the destroy.
Maintaining the single-child invariant narrows this (the tooltip is short-lived
and self-destroys on leave) but does not eliminate it. If the soak still shows
protocol errors, note **which** surface id they name before concluding the fix
failed.

## Context (from discovery)

- files/components involved:
  - `src/app.rs` — `Message` enum (`Surface(cosmic::surface::Action)`, l. 361),
    the blind forwarder (l. 2094), `TogglePopup`/`PopupClosed` (l. 1885-1912),
    `Window` struct (l. 67 `#[derive(Default)]`, fields l. 110-148), the
    existing test `popup_closed_only_clears_the_matching_surface` (l. 3558).
  - `src/tooltip.rs` — shared `WINDOW_ID` (l. 37), the settings closure passed
    as `Some(move |bounds| ..)` (l. 69-96), the `on_close` message (l. 103),
    the surface-action mapper (l. 104), and the doc comment at l. 54-56 that
    claims upstream's `has_popup` flag "has no counterpart here" — which this
    plan makes false.
  - `src/view.rs` — `popup_tooltip` (l. 484-490), the two `popup_dropdown` rows
    (l. 409, 426, both passing `Message::Surface`), tooltip call sites
    (l. 323, 474).
- verified libcosmic behaviour this plan depends on:
  - **The tooltip's create is `Action::Task`, not `Action::Popup`.**
    `src/widget/wayland/tooltip/widget.rs:491` builds
    `surface::Action::Task(Arc::new(move || ..))` for the delayed branch and
    publishes it through `on_surface_action` (l. 554). We set `.delay(100 ms)`
    (`src/tooltip.rs:106`), so this is always our branch. `src/app/cosmic.rs:413`
    then runs it as `f().map(|sm| Cosmic(Action::Surface(sm)))` — the resolved
    `Popup`/`Ignore` goes straight to the runtime and **never** comes back
    through `Message`. So the observable signal is *arming*, not creation.
  - **`PopupClosed` fires for every popup, not just ours.**
    `PopupEvent::Done` → `Action::SurfaceClosed(id)` (`src/app/cosmic.rs:608`)
    → `on_close_requested(id)` (l. 1243) → our `Message::PopupClosed(id)`. A
    dropdown dismissed by grab loss publishes no `DestroyPopup`, so this is the
    only signal that it closed.
  - **Upstream suppresses tooltips by withholding the settings closure**:
    `Core::applet_tooltip` passes `(!has_popup).then_some(move |bounds| ..)` as
    `Tooltip::new`'s `settings` (`src/applet/mod.rs:308`). With `None` the
    widget stays in the tree but can never create a popup.
  - **Surface actions must not be cloned.** `src/app/cosmic.rs` recovers the
    settings with `Arc::try_unwrap(settings).ok()`; a surviving clone makes it
    log `"Invalid settings for popup"` and create nothing — silently, since
    logging is off by default.
- related patterns found: generation-counter staleness guards
  (refresh/shuffle/accent) are the existing idiom for "ignore an event that no
  longer applies". iced view code is exempt from tests; the pure helpers beside
  it are not.
- dependencies identified: libcosmic is a **rev-pinned git dep**; the runtime
  sibling hole is upstream's and is worked around locally, not patched.
- **No i18n impact**: no message ids added, renamed or dropped, and the `fl!`
  call sites at `src/view.rs:323`/`474` stay put (they are arguments evaluated
  at the call site), so `every_message_id_is_referenced_by_the_ui`
  (`src/localize.rs:330`) and the 73 catalogues are untouched.

## Development Approach

- **testing approach**: Regular (code first, then tests in the same task)
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in
  that task
  - tests are not optional - they are a required part of the checklist
  - write unit tests for new functions/methods
  - write unit tests for modified functions/methods
  - add new test cases for new code paths
  - update existing test cases if behavior changes
  - tests cover both success and error scenarios
- **CRITICAL: all tests must pass before starting next task** - no exceptions
- **CRITICAL: update this plan file when scope changes during implementation**
- run `just check` after each change (fmt + clippy `-D warnings` + tests)
- maintain backward compatibility

## Testing Strategy

- **unit tests**: required for every task. The ledger transitions are tested by
  driving `Window::update` with the relevant `Message`s, as the existing
  popup/accent tests do.
- **test construction**: `Window` derives `Default` (`src/app.rs:67`) and
  `Window::default()` leaves `config_context: None`, so nothing persists —
  the `CLAUDE.md` rule that tests never touch real user config/state is
  satisfied by construction here, no `TempDir` needed. Surface actions are
  built directly: `Action::Popup(Arc::new(Box::new(()) as Box<dyn Any + Send +
  Sync>), <same>, None)` for a dropdown create, and
  `Action::Task(Arc::new(|| Task::none()))` for a tooltip arm. Neither is
  executed by the test — `update` only inspects the variant.
- **e2e tests**: the project has none and cannot have them here — a Wayland
  protocol error is only observable against a live compositor. The journal soak
  in **Post-Completion** is the real acceptance test.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope
- keep plan in sync with actual work done

## Solution Overview

**Maintain the single-child invariant on `window.popup`.** Since a dropdown can
never be destroyed by us, the fix is to guarantee a tooltip and a dropdown are
never mapped at the same time. Four rules, backed by a small ledger:

1. **Ledger.** `Window` tracks `tooltip_open` and `dropdown_open`, maintained
   from the surface actions the two widgets route through us plus the
   `PopupClosed` notifications the compositor sends for every popup.
2. **Interlock.** When a dropdown create arrives and a tooltip is open, destroy
   the tooltip **first**, chained ahead of forwarding the create — that instant
   is the only moment the tooltip is legally topmost.
3. **Suppression.** While `dropdown_open`, the tooltip widget is built with
   `settings: None` (upstream's `has_popup` shape), so no new tooltip can arm.
4. **Deferral.** A tooltip destroy arriving while `dropdown_open` is held back
   (it would be non-topmost) and flushed when the dropdown closes.

There is deliberately **no** `popup_teardown` ordering function and no
multi-child destroy sequence: with the invariant held there is at most one
child, and the runtime's own descent handles it. `TogglePopup` keeps destroying
`window.popup` directly.

### Key design decisions

- **Invariant over ordering.** An ordering function would need to emit a
  dropdown destroy, which is impossible (Why it happens, fact 2). Enforcing
  "at most one child" is the only rule that is actually actionable, and it
  makes the runtime's existing single-child descent correct.
- **`tooltip_open` means "armed", not "mapped".** The observable signal is
  `Action::Task`, whose future may resolve to `Ignore` (no popup). The flag is
  therefore deliberately biased toward `true`: a false positive costs one
  destroy for a popup that does not exist, which the runtime handles as a no-op
  (`log::info!("No popup to destroy")`, `state.rs:1401`). A false *negative*
  would strand a real tooltip, so bias matters.
- **Tooltip destroys are deferred, never dropped.** Dropping one on a desynced
  flag would strand a mapped tooltip forever. The only suppression is the
  dropdown-open window, and it is always flushed.
- **Never clone a surface action.** Handlers match by reference and move the
  action into `surface_task`; `Debug` logging by reference is safe.
- **Not fixed here**: libcosmic's runtime sibling hole and the compositor-side
  `popup_done` residual path (see Post-Completion for the upstream report).

## Technical Details

### New `Window` state (`src/app.rs`)

```rust
/// Whether a tooltip popup on the shared surface (`tooltip::window_id()`)
/// is armed. Armed, not mapped: the observable signal is the widget's
/// `Action::Task`, whose delayed future may still resolve to `Ignore`. The
/// flag is biased toward `true` on purpose — a spurious destroy is a
/// runtime no-op, a missed one strands a mapped tooltip as a second child
/// of `window.popup`, which is the crash this all exists to prevent.
tooltip_open: bool,
/// Whether a dropdown menu popup is mapped. Its window id is minted inside
/// the widget and cannot be read at creation, so the ledger tracks presence
/// only — which is all the single-child invariant needs.
dropdown_open: bool,
/// A tooltip destroy that arrived while a dropdown was open, held back
/// because it would have destroyed a non-topmost popup. Flushed when the
/// dropdown closes.
tooltip_destroy_deferred: bool,
```

### New messages (`src/app.rs`)

```rust
/// Surface actions from the tooltip widget (`crate::tooltip`), routed here
/// instead of `Surface` so the ledger sees them.
TooltipSurface(cosmic::surface::Action),
/// Surface actions from a `popup_dropdown` menu, same reason.
DropdownSurface(cosmic::surface::Action),
```

`Message::Surface` stays for anything else needing a raw forward.

### Ledger transitions

| message | action variant | effect |
|---|---|---|
| `TooltipSurface` | `Task(..)` | `tooltip_open = true`, forward |
| `TooltipSurface` | `DestroyPopup(_)` and `!dropdown_open` | `tooltip_open = false`, forward |
| `TooltipSurface` | `DestroyPopup(_)` and `dropdown_open` | `tooltip_destroy_deferred = true`, **do not forward** |
| `DropdownSurface` | `Popup`/`AppPopup` | if `tooltip_open`: chain `destroy_popup(tooltip::window_id())` **before** forwarding the create, clear `tooltip_open`; set `dropdown_open = true` |
| `DropdownSurface` | `DestroyPopup(_)` | `dropdown_open = false`, forward, then flush a deferred tooltip destroy |
| `PopupClosed(id)` | `id == self.popup` | clear `self.popup` and **all** flags |
| `PopupClosed(id)` | `id == tooltip::window_id()` | `tooltip_open = false` |
| `PopupClosed(id)` | any other id | `dropdown_open = false`, flush a deferred tooltip destroy |

Every transition logs at `debug` through `tracing` (silent by default), so a
future protocol error can be read off the journal against the popup sequence
that preceded it.

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code and unit tests in this repo.
- **Post-Completion** (no checkboxes): install, the live-compositor soak — the
  only real proof — and the upstream report.

## Implementation Steps

### Task 1: Add the ledger fields and route `PopupClosed` by surface id

**Files:**
- Modify: `src/app.rs`
- Modify: `src/tooltip.rs`

- [x] expose the shared tooltip surface id (`pub fn window_id() -> window::Id`
      over the existing `WINDOW_ID` `LazyLock`, `src/tooltip.rs:37`)
- [x] add `tooltip_open`, `dropdown_open`, `tooltip_destroy_deferred` to
      `Window` with the doc comments from Technical Details (no `init` change
      needed — `Window` derives `Default`)
- [x] extend `Message::PopupClosed` (`src/app.rs:1908`) to clear the flags per
      the last three ledger rows, with a comment on why an id that is neither
      ours nor the tooltip's must be a dropdown
- [x] update the existing `popup_closed_only_clears_the_matching_surface`
      (`src/app.rs:3558`) with the new flag assertions rather than adding a
      parallel test
- [x] write tests: `PopupClosed(tooltip::window_id())` clears only
      `tooltip_open`; `PopupClosed(<other id>)` clears only `dropdown_open`
- [x] run `just check` - must pass before task 2

### Task 2: Route tooltip surface actions through the ledger

**Files:**
- Modify: `src/tooltip.rs`
- Modify: `src/app.rs`

- [x] change `crate::tooltip::tooltip` to emit `Message::TooltipSurface` for
      both its `on_close` message (l. 103) and its surface-action mapper
      (l. 104)
- [x] add `Message::TooltipSurface(..)` and handle the three tooltip rows of
      the ledger table: arm on `Action::Task`, forward-and-clear on destroy,
      defer the destroy while `dropdown_open`
- [x] match the action **by reference** and move it into `surface_task` — add
      a comment that cloning it makes `Arc::try_unwrap` fail and silently
      creates nothing
- [x] log each transition at `debug` via `tracing`
- [x] write tests: `Action::Task` sets `tooltip_open`; a destroy with
      `dropdown_open == false` clears it; a destroy with `dropdown_open == true`
      leaves `tooltip_open` set and sets `tooltip_destroy_deferred`
- [x] run `just check` - must pass before task 3

### Task 3: Route dropdown surface actions and add the interlock

**Files:**
- Modify: `src/view.rs`
- Modify: `src/app.rs`

- [ ] add `Message::DropdownSurface(..)`; on a create action, chain
      `destroy_popup(tooltip::window_id())` ahead of the forwarded create when
      `tooltip_open`, then set `dropdown_open`
- [ ] on a dropdown destroy, clear `dropdown_open` and flush a deferred tooltip
      destroy
- [ ] pass `Message::DropdownSurface` as `on_surface_action` in `interval_row`
      (`src/view.rs:409`) and `retention_row` (`src/view.rs:426`)
- [ ] comment the interlock: the create arriving is the only instant the
      tooltip is still legally topmost
- [ ] write tests: a dropdown create with `tooltip_open` clears it and sets
      `dropdown_open`; a create without a tooltip just sets `dropdown_open`; a
      dropdown destroy clears `dropdown_open` and clears
      `tooltip_destroy_deferred`
- [ ] run `just check` - must pass before task 4

### Task 4: Suppress tooltip arming while a dropdown is open

**Files:**
- Modify: `src/tooltip.rs`
- Modify: `src/view.rs`

- [ ] give `crate::tooltip::tooltip` a `suppressed: bool` parameter that gates
      the settings closure — `(!suppressed).then_some(move |bounds| ..)`,
      matching `Core::applet_tooltip` (`libcosmic src/applet/mod.rs:308`) —
      rather than returning bare content, so the widget stays in the tree and
      the wrapped button keeps its state
- [ ] pass `window.dropdown_open` from `view::popup_tooltip`
      (`src/view.rs:484`)
- [ ] update the `src/tooltip.rs` doc comment at l. 54-56, which currently
      states upstream's `has_popup` flag "has no counterpart here" — it now
      has one, for a different reason (sibling-popup avoidance, not panel
      button suppression)
- [ ] verify the `fl!` call sites at `src/view.rs:323`/`474` are untouched so
      `every_message_id_is_referenced_by_the_ui` still sees every id
- [ ] write a test that the tooltip is suppressed exactly while a dropdown is
      open, asserted through the ledger (after a dropdown create message the
      suppression input is `true`; after its destroy it is `false`) rather than
      as a tautology over the field
- [ ] run `just check` - must pass before task 5

### Task 5: Verify acceptance criteria

- [ ] verify every popup parented to `window.popup` routes through
      `TooltipSurface`/`DropdownSurface` — grep remaining `Message::Surface`
      uses and confirm each is unrelated
- [ ] verify no surface action is cloned on any handler path
- [ ] verify no `fl!` id was added, renamed or removed (no locale churn)
- [ ] run the full suite: `just check` (fmt + clippy `-D warnings` + tests)
- [ ] re-read the ledger table against the implementation and confirm every
      row has a test

### Task 6: [Final] Update documentation

- [ ] add a "popup stack" rule to the **UI conventions** section of
      `CLAUDE.md`: everything parented to `window.popup` is a sibling on one
      xdg-shell stack; libcosmic's runtime collects only a single child chain
      and a dropdown's id is unobservable, so the invariant is **at most one
      child popup at a time**, held by the interlock + suppression + deferral;
      never clone a `surface::Action`
- [ ] fix the stale line in `CLAUDE.md`'s tooltip bullet — "Both `view.rs` and
      `app.rs` build their tooltips through it" is false, `app.rs` has no
      tooltip (the panel button deliberately has none); the only call sites are
      `src/view.rs:323` and `474`
- [ ] note in `CLAUDE.md` that the accent feature's documented crash-window
      exposure was observed in the wild because of this crash, and that its
      premise (the applet does not crash) is what this fix restores
- [ ] record the compositor-initiated `popup_done` residual path so a future
      protocol error is not misread as a regression of this fix
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

*Items requiring manual intervention or external systems - no checkboxes,
informational only*

**Manual verification** (the only check that proves the fix):

- `just build && just install`, then restart cosmic-panel.
- Exercise the popup deliberately, targeting the invariant: hover every
  icon-only control so its tooltip appears; move between controls quickly (the
  shared-surface-id race); **open both dropdowns immediately after a tooltip
  has appeared** (the interlock); select and dismiss entries; dismiss a
  dropdown by clicking outside it (grab loss — the `PopupClosed`-only path);
  close the popup by clicking the panel icon while a tooltip is up.
- Then check the journal:
  `journalctl --user | grep -E 'CosmicBingWallpaper.*(xdg_popup|exited with code 1)'`
  — the baseline is 11 hits, the last two on 2026-08-10 at 18:13:02 and
  18:18:02.
- If an error *does* appear, note which surface id it names before concluding
  the fix failed: the compositor-initiated `popup_done` path (see "Residual
  path" above) is not closed by this work.
- Soak for a day of normal use and re-check; the crash was intermittent
  (roughly one per few hours of interaction), so one clean session is not proof.
- Corroborating signals, not acceptance criteria: no duplicated/overlapping
  panel icons after a session, and the accent toggle surviving a panel restart
  with `accent_enabled` still `true` in
  `~/.config/cosmic/io.github.ercling.CosmicBingWallpaper/v1/accent_enabled`.

**External system updates**:

- Consider filing an upstream libcosmic issue covering both holes:
  `Action::Destroy`
  (`iced/winit/src/platform_specific/wayland/event_loop/state.rs:1392`)
  collects one child per level, so a popup with two children destroys the
  parent while a sibling is mapped; and `xdg_popup.rs::done` walks only up the
  parent chain, so compositor dismissal has the same hole. Every applet
  combining a wayland tooltip with a dropdown under one popup is exposed, and
  `widget::dropdown::popup_dropdown` gives the application no way to close the
  menu it opened.
- Unrelated but noticed while investigating, worth reporting to Fedora:
  `/usr/share/cosmic/com.system76.CosmicTheme.Dark.Builder/v2/palette` (from
  `cosmic-config-fedora`) is malformed RON — it opens with `(` instead of the
  `Dark((` enum wrapper its Light counterpart has, so every reader falls back
  to the built-in dark palette. Harmless here, not our bug, no action needed.
