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

### Residual paths, not closed by this plan

> **Read this before interpreting the soak.** Review (2026-08-11, second
> pass) established two things that change how a reappearing error must be
> read.
>
> First, the runtime *already* refuses to map two children: on any popup
> create `state.rs` computes `parent_mismatch` from the last entry of
> `self.popups` and, on mismatch, legally destroys every popup above the
> requested parent (topmost-first) before retrying the create 30 ms later. So
> the two-children state this plan targets is hard to reach from the *create*
> side.
>
> Second — and this is the correction that matters — path (a) below is **not
> tooltip-specific and needs no second child at all**. It is an upstream
> destroy-*order* bug that fires on the ordinary "dismiss a dropdown by
> clicking outside it" interaction (item 4 of the soak list). The soak is
> therefore expected to still reproduce the error on that one step, and doing
> so is **not** evidence that the ledger broke. What the ledger is answerable
> for is the *other* steps: tooltip + menu interleavings, and any destroy this
> applet emits itself.

**(a) Compositor-initiated dismissal** is outside our reach, and is an
upstream destroy-order bug.
`iced/winit/src/platform_specific/wayland/handlers/shell/xdg_popup.rs::done`
builds `to_destroy` by walking **up** the parent chain (`PopupParent::Popup`),
breaking at a layer-surface/window parent — it never collects children, so
`to_destroy == [dismissed, parent, grandparent, …]`, deepest first. It then
iterates `to_destroy.into_iter().rev()`, i.e. **ancestor first**, because it is
missing the `to_destroy.reverse()` that `event_loop/state.rs`'s
`Action::Destroy` arm performs between its up-walk and its down-walk. Each
`SctkPopup` is dropped inside that loop and sctk's
`impl Drop for PopupInner` (`src/shell/xdg/popup.rs`) calls
`xdg_popup.destroy()`, so the destroy order on the wire is inverted.

Consequence, with **one** child and no tooltip anywhere: a dropdown menu is
created with `grab: true` (`src/widget/dropdown/widget.rs`) and our applet
popup also has `grab: true` (`src/applet/mod.rs`), so both are in one grab
chain. A click outside makes the compositor `popup_done` the chain, and either
delivery order breaks:

- `done(menu)` first → `to_destroy = [menu, our_popup]` → `.rev()` destroys
  **our popup first**, while the menu is still mapped;
- `done(our_popup)` first → `to_destroy = [our_popup]` (its parent is the panel
  layer surface, so the walk breaks) → destroys it with the menu child mapped.

Either way: `xdg_popup was destroyed while it was not the topmost popup`. The
same shape hits a mapped tooltip when our popup is dismissed (the tooltip has
`grab: false`, so it is not in the grab chain and is simply left orphaned or
destroyed out of order). Maintaining the single-child invariant narrows the
tooltip variant (the tooltip is short-lived and self-destroys on leave) but has
no effect at all on the menu variant. Fixing it needs a libcosmic patch — see
**Post-Completion**. If the soak shows protocol errors, note **which** surface
id they name, and what the last interaction was, before concluding the fix
failed.

**(b) The arm→create gap.** The tooltip's create is produced by a 100 ms
delayed future that bypasses `Message` entirely, so a dropdown create arriving
inside that window chains a `destroy_popup(tooltip)` that is a no-op, and the
future can still map the tooltip *after* the menu. The widget re-checks
`is_hovered` at resolution (`widget/wayland/tooltip/widget.rs`), which closes
the pointer-driven case — moving the pointer toward the dropdown button
publishes `on_leave` first — so only a no-leave activation (touch/keyboard)
survives, and upstream's `parent_mismatch` cleanup covers even that.

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

~~`Message::Surface` stays for anything else needing a raw forward.~~ It has no
other constructor, so it was removed instead — see the deviation note under
Task 3.

### Ledger transitions

**Revised 2026-08-11 after review** — the three-flag ledger below the line was
replaced by a single `dropdown_open` bit plus the idempotent `destroy_tooltip()`
task; see "Post-review revision". Current table:

| message | action variant | effect |
|---|---|---|
| `TooltipSurface` | anything, `dropdown_open` | **drop it** (no forward, no emission) |
| `TooltipSurface` | anything, `!dropdown_open` | forward untouched |
| `DropdownSurface` | `Popup`/`AppPopup` | `dropdown_open = true`; chain `destroy_tooltip()` **before** the forwarded create |
| `DropdownSurface` | `DestroyPopup(_)` | forward, then chain `destroy_tooltip()` — **ledger untouched** (see the second review revision) |
| `DropdownSurface` | anything else | forward untouched |
| `PopupClosed(id)` | `id == tooltip::window_id()` | nothing |
| `PopupClosed(id)` | `id == self.popup` | clear `self.popup`, `dropdown_open = false`, `destroy_tooltip()` |
| `PopupClosed(id)` | any other id | `dropdown_open = false`, `destroy_tooltip()` |
| `TogglePopup` | popup open | `dropdown_open = false`, then destroy `self.popup` |

<details><summary>Superseded three-flag table (as implemented in Tasks 1-5)</summary>

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

</details>

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

- [x] add `Message::DropdownSurface(..)`; on a create action, chain
      `destroy_popup(tooltip::window_id())` ahead of the forwarded create when
      `tooltip_open`, then set `dropdown_open`
- [x] on a dropdown destroy, clear `dropdown_open` and flush a deferred tooltip
      destroy
- [x] pass `Message::DropdownSurface` as `on_surface_action` in `interval_row`
      (`src/view.rs:409`) and `retention_row` (`src/view.rs:426`)
- [x] comment the interlock: the create arriving is the only instant the
      tooltip is still legally topmost
- [x] write tests: a dropdown create with `tooltip_open` clears it and sets
      `dropdown_open`; a create without a tooltip just sets `dropdown_open`; a
      dropdown destroy clears `dropdown_open` and clears
      `tooltip_destroy_deferred`
- [x] run `just check` - must pass before task 4

⚠️ **Deviation: `Message::Surface` is gone, not kept.** "Technical Details"
said it "stays for anything else needing a raw forward", but the two dropdown
rows were its *only* constructors — rerouting them left the variant
unconstructed and `clippy -D warnings` fails on `dead_code`. Rather than
`#[allow(dead_code)]` a forwarder that by design must not exist (a blind
forward is exactly what could map a second child of `window.popup`), the
variant and its `update` arm were removed. Task 5's "grep remaining
`Message::Surface` uses" is therefore trivially satisfied: there are none.
`CLAUDE.md`'s UI-conventions line naming the `Message::Surface` forwarder is
now stale and is corrected in Task 6.

### Task 4: Suppress tooltip arming while a dropdown is open

**Files:**
- Modify: `src/tooltip.rs`
- Modify: `src/view.rs`

- [x] give `crate::tooltip::tooltip` a `suppressed: bool` parameter that gates
      the settings closure — `(!suppressed).then_some(move |bounds| ..)`,
      matching `Core::applet_tooltip` (`libcosmic src/applet/mod.rs:308`) —
      rather than returning bare content, so the widget stays in the tree and
      the wrapped button keeps its state
- [x] pass `window.dropdown_open` from `view::popup_tooltip`
      (`src/view.rs:484`)
- [x] update the `src/tooltip.rs` doc comment at l. 54-56, which currently
      states upstream's `has_popup` flag "has no counterpart here" — it now
      has one, for a different reason (sibling-popup avoidance, not panel
      button suppression)
- [x] verify the `fl!` call sites at `src/view.rs:323`/`474` are untouched so
      `every_message_id_is_referenced_by_the_ui` still sees every id
- [x] write a test that the tooltip is suppressed exactly while a dropdown is
      open, asserted through the ledger (after a dropdown create message the
      suppression input is `true`; after its destroy it is `false`) rather than
      as a tautology over the field
- [x] run `just check` - must pass before task 5

[decision] `view::popup_tooltip` reads the ledger through a named pure helper
`view::tooltip_suppressed(window)` rather than passing `window.dropdown_open`
inline. It is what the plan's "asserted through the ledger, not as a tautology
over the field" test needs a name for, and it carries the *why* (sibling of the
menu on one xdg-shell stack) at the point the view decides.

### Task 5: Verify acceptance criteria

- [x] verify every popup parented to `window.popup` routes through
      `TooltipSurface`/`DropdownSurface` — grep remaining `Message::Surface`
      uses and confirm each is unrelated
- [x] verify no surface action is cloned on any handler path
- [x] verify no `fl!` id was added, renamed or removed (no locale churn)
- [x] run the full suite: `just check` (fmt + clippy `-D warnings` + tests)
- [x] re-read the ledger table against the implementation and confirm every
      row has a test

**Verification results.**

- *Routing.* The only popup creators in `src/` are `widget::dropdown::popup_dropdown`
  (`src/view.rs:409`, `426` — both `on_surface_action: Message::DropdownSurface`),
  `crate::tooltip::tooltip`'s `SctkPopupSettings` (`src/tooltip.rs:85`, both its
  `on_close` and its mapper going to `Message::TooltipSurface`), and
  `TogglePopup`'s own `app_popup` (`src/app.rs`), which creates `window.popup`
  itself — parented to the panel, not to `window.popup`, so it is not a child
  and needs no ledger row. `Message::Surface` has no remaining uses: the
  variant was removed in Task 3 (see the deviation note there), so the grep is
  empty by construction.
- *No clones.* Both handlers `match &action` and move the untouched value into
  `surface_task`; `grep -n 'clone()' src/tooltip.rs src/view.rs src/app.rs`
  shows no `surface::Action` clone on any path (the tooltip's two clones are a
  `Cow<str>` label and a `widget::Id`).
- *No locale churn.* `git diff 7b03cf9..HEAD -- i18n/ data/` is empty and the
  diff over `src/` touches no `fl!(` line, so the `fl!` sites at
  `src/view.rs:323`/`474` and all 73 catalogues are untouched.
- *Ledger rows.* Every row of the table is driven through `Window::update`:
  arm (`a_tooltip_arm_marks_the_tooltip_open`), destroy without a dropdown
  (`a_tooltip_destroy_clears_the_flag_when_no_dropdown_is_open`), destroy
  behind one (`a_tooltip_destroy_is_deferred_while_a_dropdown_is_open`),
  create + interlock (`a_dropdown_create_closes_an_open_tooltip_first`,
  `a_dropdown_create_without_a_tooltip_only_marks_the_dropdown_open`),
  dropdown destroy + flush
  (`a_dropdown_destroy_clears_the_flag_and_flushes_a_deferred_tooltip_destroy`),
  and the three `PopupClosed` ids
  (`popup_closed_only_clears_the_matching_surface`,
  `popup_closed_for_the_tooltip_surface_clears_only_the_tooltip`,
  `popup_closed_for_an_unknown_surface_clears_only_the_dropdown`,
  `a_dropdown_close_flushes_the_deferred_tooltip_destroy`). Two gaps were
  found and closed — see below. `just check`: fmt + clippy `-D warnings`
  clean, 271 tests pass (was 268).

➕ **Gap 1 — the create row names `Popup`/`AppPopup`, only `Popup` was tested.**
The handler does match both, but the two are distinct arms: a create arriving
as the untested variant would fall through to the catch-all, map a menu, and
leave the ledger believing nothing is open — precisely the two-children state
the invariant forbids. Added `an_app_popup_dropdown_create_interlocks_the_same_way`.

➕ **Gap 2 — `TogglePopup` left the ledger stale.** The
`PopupClosed(id) | id == self.popup` row cannot fire for a popup we close
ourselves: `TogglePopup` `take()`s `self.popup` before emitting the destroy, so
by the time the `Done` arrives the id no longer matches and the close is read
as a menu's. Closing the popup from the panel icon while a dropdown was open
therefore left `dropdown_open == true` until some other close cleared it —
tooltips paused, tooltip destroys swallowed. Fixed by clearing the bit in
`TogglePopup` itself; covered by `closing_our_own_popup_clears_the_ledger` and
`a_late_close_for_a_popup_we_already_took_is_harmless`. Add this to the manual
soak: open a dropdown, close the popup from the panel icon, reopen it, and
confirm tooltips still appear.

> ⚠️ **Correction (2026-08-11 review).** This gap was originally written up as a
> defect whose cause was "the runtime's `Action::Destroy` sends no event at
> all". **That premise is false at the pinned rev** and was propagated into
> `CLAUDE.md` and the code comments before being corrected. `state.rs`'s
> `Destroy` arm runs `for popup in to_destroy.into_iter().rev() { … send_event(
> … PopupEventVariant::Done …) }` for *every* popup it tears down — byte for
> byte the compositor path's emission (`handlers/shell/xdg_popup.rs`) — and
> `sctk_event.rs` translates it against the winit-side `surface_ids` map, not
> the just-cleared `state.id_map`, so `src/app/cosmic.rs` turns it into
> `Action::SurfaceClosed(id)` → `Message::PopupClosed`. Self-initiated destroys
> **do** come back, for the popup and each child taken with it. The fix stands;
> only its justification changed (late delivery, not no delivery), and both
> `on_popup_closed` branches were made to do the same thing so the
> misclassification is harmless rather than merely unlikely.

### Task 6: [Final] Update documentation

- [x] add a "popup stack" rule to the **UI conventions** section of
      `CLAUDE.md`: everything parented to `window.popup` is a sibling on one
      xdg-shell stack; libcosmic's runtime collects only a single child chain
      and a dropdown's id is unobservable, so the invariant is **at most one
      child popup at a time**, held by the interlock + suppression + deferral;
      never clone a `surface::Action`
- [x] fix the stale line in `CLAUDE.md`'s tooltip bullet — "Both `view.rs` and
      `app.rs` build their tooltips through it" is false, `app.rs` has no
      tooltip (the panel button deliberately has none); the only call sites are
      `src/view.rs:323` and `474`
- [x] note in `CLAUDE.md` that the accent feature's documented crash-window
      exposure was observed in the wild because of this crash, and that its
      premise (the applet does not crash) is what this fix restores
- [x] record the compositor-initiated `popup_done` residual path so a future
      protocol error is not misread as a regression of this fix
- [x] move this plan to `docs/plans/completed/` (performed in the post-review
      pass, 2026-08-11)

**Documentation results.** All four `CLAUDE.md` edits were written against the
*current* code, not the plan's original design text:

- The dropdowns bullet in **UI conventions** now names
  `Message::DropdownSurface` and states that no blind `Message::Surface`
  forwarder exists (Task 3's deviation), so nobody reintroduces one.
- A new **"Popup stack"** rule states the invariant, both libcosmic facts that
  make it the only actionable rule, and the four maintenance rules the
  implementation actually rests on: ledger-aware routing for every child popup,
  never clone a `surface::Action`, `tooltip_open` is "armed" and biased toward
  `true`, and `clear_popup_ledger` must be called from both paths that end our
  popup (Task 5's Gap 2 — `Action::Destroy` emits no event, only compositor
  dismissal produces `PopupClosed`). The residual `popup_done` path closes the
  rule, framed as "note which surface id it names".

  > ⚠️ **Correction (2026-08-11).** This bullet records the Task 6 text as
  > written, and two of its claims were later superseded: `tooltip_open` and
  > `clear_popup_ledger` no longer exist (the post-review revision below
  > reduced the ledger to the single `dropdown_open` bit with unconditional
  > tooltip destroys), and "`Action::Destroy` emits no event" is false at the
  > pinned rev (see the Gap 2 correction above — self-initiated destroys *do*
  > deliver `PopupClosed`, just late). The current rules live in `CLAUDE.md`'s
  > **UI conventions → Popup stack**, which was rewritten accordingly.
- The `src/tooltip.rs` architecture bullet drops the false "`app.rs` builds
  tooltips too" and gains the `suppressed` parameter.
- The accent bullet's "remaining exposure" paragraph records that the exposure
  *fired* on 2026-08-10 through this crash, with the explicit instruction not
  to add a write-ahead record until a repeat report rules the crash out.

`just check`: fmt + clippy `-D warnings` clean, 271 tests pass (docs-only
change, no code touched, so no new tests — the i18n guard tests are unaffected,
`CLAUDE.md` holds no `fl!` ids).

## Post-review revision (2026-08-11)

Code review of the finished branch produced 7 major + 29 minor findings. Three
of them converged on the same conclusion from opposite directions — one asking
for *more* guarding on the tooltip flags (the interlock could fire a destroy
for a non-topmost tooltip when a menu was already open), one showing the flags
were unreliable anyway (five tooltip widgets share one surface id and publish
arm/leave in widget-tree order, not pointer order), one asking for the flags to
be deleted as unnecessary bookkeeping. The resolution taken was the third,
which subsumes the first two:

- **`tooltip_open` and `tooltip_destroy_deferred` are gone.** The ledger is now
  the single `dropdown_open` bit. Both flags only ever decided whether to emit
  `destroy_popup(tooltip::window_id())`, which the runtime already treats as a
  no-op for an unmapped id — so it is emitted unconditionally instead: ahead of
  every dropdown create, after every dropdown destroy, and after every
  `PopupClosed` that is not the tooltip's own.
- **The deferral became a drop.** While `dropdown_open`, *every* tooltip action
  is dropped rather than only its destroy being held; the unconditional
  post-menu destroy is what makes that lossless.
- **The interlock is unconditional** and legal in both directions: no tooltip
  mapped → no-op; a tooltip mapped over an already-open menu → it is itself the
  topmost, so destroying it is allowed.
- **`clear_popup_ledger` was inlined** into the one bit each caller now clears.
- **`on_popup_closed`'s two non-tooltip branches were merged.** Stale ids —
  a `Done` for a popup `TogglePopup` already took, or for a menu that closed
  before another opened — cannot be distinguished from a live menu's close, so
  both branches were made to do the same clear-and-sweep, which makes the
  misclassification harmless instead of merely improbable.
- **Emissions are now asserted.** The previous tests only read the flags, which
  are set by statements separate from the `surface_task(..)` calls: deleting
  every destroy left all 271 tests green. `testutil::surface::emitted(task)`
  drains a returned `Task` into an ordered `Vec<Emitted>`, and the ledger tests
  assert the full sequence (`[DestroyTooltip, Create]`, `[DestroyOther,
  DestroyTooltip]`, `[]`). Verified by mutation: removing the interlock destroy,
  the post-menu destroy, or the drop each fails tests.
- **The `surface::Action` builders moved to `src/testutil.rs`**, ending the
  duplicate copies in `app::tests` and `view::tests`.

Net: 269 tests (was 271 — five ledger tests replaced by seven emission-asserting
ones, and the two flag-only duplicates dropped), ~90 fewer lines of ledger code
and comments. `just check` clean.

Documentation corrected in the same pass: the false "`Action::Destroy` emits no
event" premise (see the Gap 2 correction above) in `CLAUDE.md`, this plan and
the code comments; `CLAUDE.md`'s tooltip bullet listing "the About button" as a
tooltip call site (it is the thumbnail and the four `nav_button`s); the plan
inventory and the hardcoded plan path in `CLAUDE.md`; the `README.md` tooltip
bullet and its "Limitations" section.

### Second review pass (2026-08-11)

A re-check of the revised branch produced three major findings; all three were
confirmed against the pinned libcosmic rev and acted on.

- **`DestroyPopup` no longer clears `dropdown_open`.** The two dropdown rows
  share one `Message::DropdownSurface`, and a row whose widget kept a stale
  `is_open` emits a destroy for an already-dead popup. That is not exotic:
  grab-loss dismissal is consumed by the compositor, so
  `widget/dropdown/widget.rs`'s `ButtonPressed` arm never runs and nothing
  resets `state.is_open` — the applet only ever sees `PopupClosed`, which the
  widget does not. The next click on the *other* row is then dispatched to
  both widgets (`iced/widget/src/row.rs`'s `update` iterates every child
  regardless of `capture_event`), and tree order (interval before retention)
  publishes the create first and the stale destroy second, ending the pass with
  the bit clear and a menu mapped — tooltips un-paused beside an open menu,
  the exact state the invariant forbids. The fix needs no id we cannot
  observe: a stale destroy is a runtime no-op ("No popup to destroy", before
  any state mutation) and so emits no `Done`, while every real teardown does,
  so `PopupClosed` alone owns the clear. The chained post-destroy
  `destroy_tooltip()` stayed where it was. Regression test:
  `a_stale_destroy_after_a_create_leaves_the_menu_recorded`.
- **Residual path (a) was re-characterised** — it is an upstream *destroy
  order* bug (missing `to_destroy.reverse()` in `xdg_popup.rs::done`) that
  fires with a single child and no tooltip, not a tooltip-specific rarity. See
  the rewritten "Residual paths" section above; `CLAUDE.md` and `README.md`
  were corrected to match, and the soak instructions now say which step is
  expected to keep failing.
- **The upstream-report item now names that bug first**, with the one-line fix
  and a minimal repro, instead of only the two secondary holes.

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
- If an error *does* appear, note which surface id it names **and which of the
  steps above produced it** before concluding the fix failed. In particular,
  "dismiss a dropdown by clicking outside it" is expected to keep failing: it
  is residual path (a), an upstream destroy-order bug that needs neither a
  tooltip nor a second child, so a hit there is a *confirmation* of the
  analysis and not a regression of the ledger. A hit on any of the other
  steps — tooltip interleavings, selecting an entry, clicking the dropdown
  button again, closing the popup by the panel icon — is ours to explain.
- Soak for a day of normal use and re-check; the crash was intermittent
  (roughly one per few hours of interaction), so one clean session is not proof.
- Corroborating signals, not acceptance criteria: no duplicated/overlapping
  panel icons after a session, and the accent toggle surviving a panel restart
  with `accent_enabled` still `true` in
  `~/.config/cosmic/io.github.ercling.CosmicBingWallpaper/v1/accent_enabled`.

**External system updates**:

- File an upstream libcosmic issue. **The actionable bug — lead with this
  one — is a missing `to_destroy.reverse()` in
  `iced/winit/src/platform_specific/wayland/handlers/shell/xdg_popup.rs`'s
  `PopupHandler::done`.** It walks *up* the parent chain, so `to_destroy` is
  `[dismissed, parent, grandparent, …]` (deepest first), and then iterates
  `.into_iter().rev()` — destroying the **ancestor first**. Dropping each
  `SctkPopup` calls `xdg_popup.destroy()` through sctk's
  `impl Drop for PopupInner`, so the wire order is inverted and the client
  dies with `xdg_popup was destroyed while it was not the topmost popup`. The
  sibling code path, `Action::Destroy` in
  `iced/winit/src/platform_specific/wayland/event_loop/state.rs` (~l. 1414),
  does exactly the right thing: it reverses between the up-walk and the
  down-walk. The one-line fix is to reverse in `done` too — or simply iterate
  `to_destroy` forward, since without a down-walk the vector is already
  deepest-first. Minimal repro: an applet popup (`grab: true`) with a
  `widget::dropdown::popup_dropdown` menu open (`grab: true`), then click
  outside — no tooltip and no second child needed.
  Secondary, worth mentioning in the same issue: `Action::Destroy`'s down-walk
  collects one child *per level* (`.position(|p| p.data.parent…)`), so a popup
  with two children destroys the parent while a sibling is still mapped; and
  `popup_dropdown` gives the application no way to close the menu it opened
  (the window id is minted inside the widget), so an application cannot even
  order the destroys itself as a workaround.
- Unrelated but noticed while investigating, worth reporting to Fedora:
  `/usr/share/cosmic/com.system76.CosmicTheme.Dark.Builder/v2/palette` (from
  `cosmic-config-fedora`) is malformed RON — it opens with `(` instead of the
  `Dark((` enum wrapper its Light counterpart has, so every reader falls back
  to the built-in dark palette. Harmless here, not our bug, no action needed.
