# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build / test / run

This machine's linuxbrew `pkg-config` shadows the system one and misses
`/usr/lib64/pkgconfig` (breaks the `xkbcommon` probe in `smithay-client-toolkit`).
**Every cargo invocation needs**
`PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig` — the `justfile`
exports it, so prefer `just` recipes; otherwise export it yourself:

```bash
just check        # cargo fmt --check + clippy --all-targets -D warnings + cargo test
just build        # cargo build --release
just install      # install binary + .desktop + icon into ~/.local (no sudo)
just uninstall
just flatpak-sources        # generate cargo-sources.json (requires uv)
just flatpak-prefetch       # retain every Flatpak input in the builder cache
just flatpak-build-offline  # force-clean build with downloads disabled
just flatpak-install
just flatpak-uninstall

# raw cargo (export PKG_CONFIG_PATH first):
export PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Logging goes through `tracing` with an env-filter and is silent by default;
set `RUST_LOG` to see it, e.g. `RUST_LOG=cosmic_bing_wallpaper=debug` (or
`RUST_LOG=info` for everything) when running the binary or a test.

`rustfmt`/`clippy` binaries live in `~/.local/bin` (extracted from Fedora RPMs to
match system rustc 1.97.1 — the distro rustc package ships without them).

Running the binary standalone (`cargo run` / `target/release/cosmic-bing-wallpaper`)
opens a floating applet window; **beware**: a cold start (empty
`~/Pictures/BingWallpaper`) triggers a real Bing fetch and a wallpaper apply ~5 s
in. The normal home is the COSMIC panel (add via Settings → Desktop → Panel after
`just install`).

Tests never touch real user config/state — they inject paths or
`Config::with_custom_path` rooted in a `tempfile::TempDir`. Keep it that way.

## Architecture

Single self-contained libcosmic applet binary, no daemon — timers only run while
the panel runs. The committed `Cargo.lock` pins both git dependencies;
cosmic-bg-config also has a `rev` in `Cargo.toml`, while libcosmic deliberately
uses the bare repo URL to match cosmic-bg-config's transitive cosmic-config
source id. A second `?rev=` spelling splits that repo and breaks offline Flatpak
vendoring. libcosmic APIs move fast, so verify against the locked revision before
coding against remembered names.

Packaging is part of the checked-in architecture. The root
`io.github.ercling.CosmicBingWallpaper.json` developer manifest installs the
binary, desktop entry, app-ID icon, and
`data/io.github.ercling.CosmicBingWallpaper.metainfo.xml` into `/app`. Its
Freedesktop 25.08 sandbox contract grants only the wallpaper directory, COSMIC
config/state, Settings Daemon notifications, logind, Wayland/DRI, and network.
`flatpak/` holds the pinned upstream Cargo source generator and `uv` wrapper;
`cargo-sources.json` is generated and ignored. `.github/workflows/rust.yml`
runs the native checks without generated sources, while
`.github/workflows/flatpak.yml` generates them and emits the Flatpak bundle.

The hand-written packaging identity web is tested by embedding the desktop
entry, metainfo, manifest, justfile, vendoring scripts, and both workflows with
`include_str!` in `src/app.rs`. Those tests cross-check names, install paths,
commands, permissions, runtime versions, and CI inputs, and include deliberate
drift/failure cases. Do not make them read generated `cargo-sources.json`, and
do not duplicate `appstreamcli` or desktop-file syntax validation in Rust.

- `src/main.rs` — entry point: `localize()` then `cosmic::applet::run::<Window>(())`.
- `src/localize.rs` — Fluent i18n: `rust-embed`ded `i18n/<locale>/cosmic_bing_wallpaper.ftl`,
  the `LANGUAGE_LOADER` `LazyLock`, the crate's own `fl!` macro, `localize()`,
  and the locale guard tests. See "i18n" below.
- `src/leader.rs` — cross-process single-owner election for COSMIC's one-applet-
  per-output process model. `Leadership` holds an advisory
  `state_dir()/leader.lock`; the winner owns shared background work, a loser
  retains its file handle and retries every ~60 s, and process exit releases
  the lock. Lock/open errors warn and fail open so a lone instance remains
  useful. `with_coordination_lock` holds the shorter-lived
  `state_dir()/coordination.lock` around blocking mailbox read-modify-write
  operations; neither lock belongs on the UI thread.
- `src/app.rs` — the `cosmic::Application` impl (`Window`): message loop, popup
  open/close, startup restore (catalogue + config, no network), the refresh
  pipeline split in two: `run_refresh`/`fetch_and_download` (async, off the UI
  thread, injectable dirs + base URL for tests: fetch list → download missing →
  thumbnails; returns a `RefreshBatch` — fetched entries, the explicitly
  ineligible `wp: false` URL bases, the response's scheduling anchor, and
  the optional `fallback` path). **The list request is always the full
  `bing::ARCHIVE_WINDOW` (`idx=0&n=8`), never paginated**; retention only
  picks which positions are downloaded (`Downloads { horizon:
  schedule::download_horizon(retention), fallback:
  wallpaper::should_auto_apply(..) }`, resolved at `start_refresh_over`,
  applied by the pure tested `select_downloads`): every eligible entry
  inside the horizon, or — when the horizon holds nothing eligible and
  auto-apply is not suppressed — exactly the newest eligible image beyond
  it, so the applet has *something* to apply (`retention=1` with a
  restricted newest image would otherwise be a permanent daily no-op).
  Never both, never any other out-of-retention image, and no fallback at
  all over a foreign wallpaper — it would be pruned and re-fetched daily)
  and the `RefreshFinished` handler (UI thread, against *live* state: merge →
  eligibility reconciliation (`remove_ineligible_over`: entry **and** file of
  every `wp: false` image, the live wallpaper's exempt, nothing at all while
  the live state is `CurrentWallpaper::Unknown` — the same evidence the
  prune's `prune_retention` guard uses) → prune protecting the live current
  wallpaper, whose thumbnail sweep also collects what the reconciliation
  removed → save → auto-apply per the "don't clobber" rule → reschedule —
  never merge/reconcile/prune in the async task, its snapshot goes stale
  during a long fetch), one-shot generation-counter timers for refresh and
  shuffle (a stale tick is ignored, so rescheduling atomically replaces the
  pending timer), `state_dir()`/`catalogue_path()`, and the tested pure
  decision `refresh_success_plan` — `auto_apply` from the live catalogue's
  `has_images`, `delay` from the *response* anchor (the newest structurally
  valid `fullstartdate`, eligible or not), so a valid response with zero
  eligible images is a successful no-op: history and the current wallpaper
  stay, the error clears, cold start stays armed, peers are acknowledged as a
  success, and the next refresh is the normal daily one rather than
  `next_refresh`'s ~6-minute reset off a stale catalogue entry. The one
  `tracing::warn!` for that day (in `fetch_and_download`) names the explicit
  `false` and absent-`wp` counts separately. A batch's `fallback` is the
  apply target in place of the catalogue's newest (which may be an
  ineligible image kept only because it is on screen) and is exempt from
  the age prune via `Window::protected_fallback` →
  `Catalogue::prune_protecting` until it is applied (the current-wallpaper
  protection takes over) or the next refresh's selection replaces it — so a
  failed apply keeps it through the interim prunes and the next refresh
  re-selects it from disk without a GET. That protection is **in-memory
  only**: a restart between the download and a failed apply lets
  `prune_and_persist` delete the out-of-retention, non-applied fallback and
  the next refresh simply re-downloads it — accepted.
  The out-of-window thumbnail backfill at the end of `fetch_and_download` is
  governed by the `Backfill` policy struct (budget + retention + applied file,
  injected by tests). Its rule: **the budget buys decodes, and no entry is ever
  paid for twice.** Three free skips come first — an entry the imminent prune
  will delete (`ImageEntry::within_retention`, the *same* test `prune` applies,
  applied file exempt), one already cached (`thumbs::is_cached`), and one that
  already failed to decode (`thumbs::decode_failed`) — then every real
  `image::open` costs budget whether it succeeds or not, and a failure is
  recorded. Weakening any one of the three re-opens a starvation or an
  unbounded-retry bug fixed before; do not "optimize" the ordering.
  That backfill (`backfill_thumbnails`) is also run **on its own at startup**
  (`run_thumbnail_pass`, armed by `start_thumbnail_pass_over` from `init`):
  previews come from the cache, a non-empty catalogue is not a cold start, and
  the first refresh can be ~24 h out — or never, offline — so thumbnail
  generation must never depend on a fetch. While either producer is running
  (`refresh_pending` / `thumbnail_pass_pending`, i.e. `may_sweep_thumbnails`)
  the prune's `thumbs::reconcile` sweep is deferred: fetched entries only join
  the catalogue at `RefreshFinished`, so an unconditional sweep deletes what
  the in-flight pass just wrote. Every pass ends in a sweep of its own.
  It also owns the **popup ledger** — the `dropdowns_open` count, the
  `TooltipSurface`/`DropdownSurface` messages, `on_popup_closed` /
  `on_tooltip_surface` / `on_dropdown_surface` and the free `destroy_tooltip`
  task — which holds the at-most-one-child invariant on `window.popup`. See
  "UI conventions → Popup stack".
  It additionally owns the leadership lifecycle. `is_active_leader()` means
  both lock owner and fully hydrated; every automatic/destructive entry point
  (refresh and shuffle timers, download/thumbnail/prune work, automatic apply,
  lock-screen poke, accent compute/write, and leader-owned config transitions) is
  gated on it. A follower's manual refresh is a coordination-mailbox request,
  coalesced into the leader's current or next fetch and settled by a covering
  completion counter. A follower may apply navigation choices directly, then
  posts an apply notice; the leader verifies cosmic-bg's live wallpaper before
  updating `current`, spending `ColdStart`, and recomputing the accent.
  Both roles persist ordinary settings through one asynchronous raw per-key
  queue: leaders adopt before enqueueing, followers only after success. Thus
  neither can rewrite unrelated leader-owned accent records from stale memory.
  On lock takeover, the winner remains inert while a blocking task reloads the
  complete applet config, coordination entry, and live wallpaper; watcher
  events invalidate that snapshot. Follower popup opens and peer-refresh
  settlement similarly reload catalogue/live state asynchronously and adopt
  it only through generation and role guards. The logind subscription remains
  alive in every process, but only the active leader acts on its messages.
- `src/view.rs` — popup UI (thumbnail, title/copyright, About link, prev/next/
  newest/refresh controls, shuffle + accent + retention rows, status footer —
  the accent toggler also renders in the empty-catalogue branch, so a modified
  theme can be switched off with no images) plus the pure
  display helpers (`displayed`/`prev_target`/`next_target`/`newest_target`,
  `format_updated`, dropdown index↔value mappings, `tooltip_suppressed`) which
  *are* unit-tested; iced view code itself is exempt from tests.
- `src/tooltip.rs` — the hover tooltip, as its own wayland popup: upstream's
  `Core::applet_tooltip` plumbing (positioner, 100 ms delay, one shared surface
  id) re-implemented so the tooltip *surface* can be styled — upstream paints it
  in the popup's own background colour, which made the label unreadable over the
  popup. The only call sites are in `view.rs` (`popup_tooltip`, reached from
  the clickable thumbnail and every `nav_button` — prev/next/newest/refresh;
  the About link is a plain `menu_button` with no tooltip) — `app.rs` builds
  none, the panel button deliberately has no tooltip. `tooltip(..)`'s
  `suppressed` argument is upstream's `has_popup` shape, used here for the
  popup-stack invariant (see "UI conventions").
- `src/bing.rs` — Bing API types + parsing (fixture:
  `tests/fixtures/hpimagearchive.json`), title/copyright derivation
  (`split_copyright` — Bing's own `title` field is the literal `"Info"`), pure
  URL/filename builders and the inverse `parse_filename`, reqwest client +
  `fetch_image_list` (always the full `ARCHIVE_WINDOW` of eight, `idx=0` —
  no retention-sized request, no historical pagination) + atomic
  `.part`-then-rename `download_image`.
  **Eligibility rule**: `parse_image_list` validates `startdate` (8 digits),
  `fullstartdate` (12 digits) and `urlbase` (the `/th?id=OHR.` prefix) and
  partitions every structurally valid entry by Bing's per-image `wp` into an
  `ImageArchive` — `eligible` (`Some(true)`, each `ArchiveImage` keeping its
  response `position`), `ineligible` (`Some(false)` urlbases) and an
  `absent_wp` count — plus `anchor`, the newest valid `fullstartdate`
  regardless of eligibility (the scheduling anchor; scheduling off an older
  eligible entry would hit `next_refresh`'s ~6-minute out-of-range reset).
  **An image GET is only ever issued for `Some(true)`**; absent `wp` blocks
  the download but never authorizes a removal — only an explicit `false`
  does (see `catalogue.rs`). `EmptyList` means *no structurally valid entry*;
  an all-ineligible response is a successful no-op, logged by
  `fetch_and_download` with the `false`/absent counts separately so an
  all-restricted day and a payload change that dropped the field stay
  distinguishable.
- `src/thumbs.rs` — 480×270 thumbnail cache in the state dir; the UI never
  decodes the full ~5 MB UHD file. Each cache slot has a `<thumb>.meta`
  sidecar holding the source's *identity* (mtime + size) and the outcome
  (`cached`/`failed`), compared for **exact equality** — never an mtime ordering,
  which cannot answer "unchanged?" and "changed?" with one test and made the
  suite flaky at timestamp ties. So `is_cached`/`decode_failed` are one
  predicate (`slot`), an undecodable image is opened once rather than once per
  refresh, and a repaired file is retried. Only `image::open` failing writes a
  `failed` verdict — a state-dir write failure never condemns a decodable
  image. Cleanup is `reconcile(live_filenames, state_dir)`: a *sweep* of the
  thumbs dir (not a removal list), so artefacts a prune-racing backfill wrote
  are still collected.
- `src/fsutil.rs` — shared atomic-write mechanics: `temp_sibling(dest, suffix)`
  and `write_atomic(dest, suffix, write)` (write to a temp sibling, then
  rename). Used by `bing::download_image`, `Catalogue::save` and
  `thumbs::ensure_thumbnail` — never hand-roll another temp-then-rename.
- `src/catalogue.rs` — `ImageEntry`/`Catalogue`: JSON persistence (atomic write),
  merge-with-dedupe by `urlbase`, retention prune (never deletes the currently
  applied file), `rebuild_from_folder` (filename ↔ urlbase mapping is
  deterministic both ways, so rebuilds dedupe against the next fetch with no
  re-downloads), `remove_ineligible` (Bing's explicit `wp: false` verdicts,
  carried by the `RefreshBatch`: removes entry **and** file so a rebuild
  cannot resurrect the image; mirrors `prune` rule for rule — only a file the
  entry legitimately names inside `images_dir` is unlinked, the entry is
  dropped only once the file is confirmed gone (unlink succeeded or already
  absent; on failure it is kept and the next refresh retries, so no window
  ever has an entry gone with a rebuildable JPEG left), and the currently
  applied file is exempt, entry and file, so `view::displayed` keeps
  attributing the image on screen — it goes on the first refresh after
  another image is applied. The caller in `app.rs` adds the
  `CurrentWallpaper::Unknown` guard: when the displayed file is unknowable
  `currently_applied` protects nothing, so nothing is deleted that refresh.
  Absent `wp` never reaches it — it blocks downloads only), navigation
  helpers. Two guards keep a transient from erasing
  the history: `prune` is a no-op while `images_dir` cannot be enumerated (an
  absent folder is not evidence that its files are gone — otherwise every entry
  looks vanished and the startup sweep *persists* that), and `load_or_rebuild`
  rescans the folder for an **empty** catalogue as well as an unusable one (a
  valid-but-empty JSON would otherwise load fine forever).
- `src/config.rs` — `AppletConfig` (shuffle on/off, interval, retention, and
  the accent feature's `accent_enabled` / `accent_snapshot` /
  `accent_last_written` — colour types imported from `accent.rs`) via
  cosmic-config under app ID `io.github.ercling.CosmicBingWallpaper`, version 1,
  write-on-change setters, watch subscription for external edits. The separate
  `CoordinationConfig` entry uses the same app ID/version but disjoint raw keys:
  monotonically increasing `refresh_request`, covering `refresh_completion`
  (request plus success/network/disk outcome), and optional `apply_notice`
  (generation plus evidence path). Its helpers serialize counter allocation
  under `coordination.lock` and write only the changed key; a full
  `AppletConfig` write therefore cannot clobber the mailbox.
- `src/wallpaper.rs` — cosmic-bg config writer: `updated_entry` mutates only
  `source`, `apply` writes the `all` entry *before* flipping `same-on-all`,
  `current_wallpaper`/`is_ours`/`should_auto_apply` back the don't-clobber rule,
  `download_dir()`. Also hosts the lock-screen poke's write half:
  `poke_state(&Config)` (fresh raw-key read of cosmic-bg's *state*
  `wallpapers`, `lockwatch::toggle_wallpapers`, `ConfigSet::set` back — raw
  keys on **our** pinned cosmic-config instance because cosmic-bg-config's
  `CosmicConfigEntry` comes from a foreign instance this crate cannot name)
  and the prod handle `poke_state_handle()`. Only the cosmic-bg *context*
  plumbing inside `apply`/`current_wallpaper`/`poke_state_handle` is uncovered
  (the context cannot be rooted in a tempdir from this crate — see the comment
  on `apply`); the three-way state mapping is the pure, tested `classify`, and
  `poke_state` itself is tested via injected `Config::with_custom_path`.
- `src/lockwatch.rs` — lock-screen wallpaper workaround for
  [cosmic-greeter#511](https://github.com/pop-os/cosmic-greeter/issues/511)
  (the locker's image cache rebuilds only on a *delivered* cosmic-bg state
  update, and locking never rebuilds — so from the second lock onward it shows
  the bundled default). Event sources: logind's session `Lock` signal plus the
  `PrepareForSleep` **resume** edge (`sleep_edge_to_event`; no `Unlocked`
  variant — nothing on this system ever calls `UnlockSession`), via a zbus
  system-bus `subscription()` that never finishes (transient D-Bus errors
  back off ~30 s inside the stream; "no logind session" warns once and
  parks). Each event bumps `lock_poke_generation` in `app.rs` — cancelling any
  pending ladder — and arms one poke per `POKE_DELAYS` (`[1 s, 4 s]`; the
  first can race the locker re-inserting `surface_names`, the second is the
  safety net), each running `wallpaper::poke_state` async, ending in
  `LockPokeFinished` (log-only). The poke **must change the value**, hence the
  normalizing `toggle_wallpapers`: two independent verified reasons — (a)
  cosmic-config's read side dedupes: the subscription only forwards when
  derive-generated `update_keys` reports changed keys, and that guard is
  value-equality, so an identical rewrite (including cosmic-bg's own 5-minute
  single-file churn) is never delivered; (b) cosmic-bg's `save_state` is
  **read-modify-write** — it mutates the first entry per output and writes the
  rest back verbatim, so no tick ever restores a canonical shape and the
  transform must be self-healing, not just reversible. The toggle normalizes
  first (first entry per output name wins, original order kept), else appends
  a duplicate of the last entry; empty/unreadable state is never written.
  **Invariant: the first entry per output name is never altered or reordered,
  additions go at the end** — every consumer reads first-match-wins. A ladder
  interrupted between its pokes leaves the duplicated shape at rest: accepted,
  tolerated by every consumer, cleaned by the next poke's normalization —
  parity is *not* guaranteed. Once cosmic-greeter#511 ships, the workaround
  self-neutralizes: the toggle still fires but is just a harmless extra
  rebuild. Mechanism pinned by tests: a mirror `CosmicConfigEntry` derive
  proves through `update_keys` that identical rewrites report no changed keys
  and toggled writes do.
- `src/accent.rs` — opt-in accent-from-wallpaper, **off by default** (a stopgap
  until COSMIC ships [cosmic-settings#343](https://github.com/pop-os/cosmic-settings/issues/343)
  natively; the restore path is the exit strategy). The whole colour domain
  lives here — `wallpaper.rs` stays cosmic-bg-only, and `config.rs` imports the
  persisted `[u8; 3]` types `AccentPair`/`AccentSnapshot` from here (arrays keep
  `AppletConfig: Eq` and every comparison exact, no float epsilon). Owns:
  `dominant_hue` (chroma-weighted Oklch hue histogram over the cached 480×270
  thumbnail; `None` = effectively grey), the transplant + gamut map + WCAG
  guard (`tone_band`/`accent_for`), the theme reader/writer (`ThemeHandles`,
  four injectable `Config`s: light/dark × builder/theme), snapshot/restore,
  and the pure tested decision `accent_plan` (analogue of
  `refresh_success_plan`). Invariants:
  - **Hue transplant**: the wallpaper contributes *only* its hue. Tone (Oklch
    L, C) is the mean of the builder's own 8 chromatic palette accents, so the
    mid-luminance legibility trap is avoided by construction and a customised
    palette is respected; then gamut-map by chroma reduction at fixed (L, h)
    (never per-channel clamping — it shifts hue) and a ≥ 6.0 WCAG check
    against the better of white/black. Guard failure and grey wallpapers both
    resolve to the palette's own `accent_warm_grey`.
  - **Single-key builder write**: `set_accent` on each mode's builder — only
    the `accent` key; `write_entry` on a builder would materialise every field
    and pin the user against future COSMIC default changes — then
    `write_theme` with `.build()`'s result: a **changed-keys-only
    transaction** against the on-disk derived theme (cosmic-settings'
    `build_theme` pattern; ~20 of 39 keys on an accent change — every
    component's `focus` ring is the accent — vs the full rewrite that fed the
    2026-08-08 btrfs fsync freeze, and a smaller torn-read window for other
    COSMIC processes, which read the theme dir non-atomically). The diff
    compares **serialized bytes** (`would_rewrite`, same ron/PrettyConfig as
    cosmic-config's `set`), never `PartialEq`: colour-bearing fields
    round-trip lossily (hex `ColorRepr` quantisation), so a value compare
    re-flags every component key forever and rewrites an *unchanged* mode
    byte-identically — upstream's value-space diff has exactly that hole,
    and the mtime-pinned `theme_rewrites_transact_only_the_changed_keys`
    exists because content assertions cannot see it. A virgin
    theme dir (probed via `is_dark`) still gets one full `write_entry`:
    diffing against `get_entry`'s absent-key default would diff against
    `Theme::preferred_theme()`, which is *environment-dependent*. Both writes
    are required — nothing else on the system rebuilds the theme from the
    builder. Reading a builder must probe the `palette` key directly and
    substitute the mode's own default on failure: `get_entry`'s **Ok** path
    silently leaks the dark default palette when the key is absent.
  - **Theme writes never run inline in `update()`** (the 2026-08-08 incident:
    under btrfs I/O pressure each fsync'd key file took ~1 s and one write
    cycle froze the UI thread for minutes). `write_accents`/`restore_accents`
    run as blocking-pool tasks (`AccentInflight` describes the task,
    `accent_job`/`run_accent_job` derive+execute it — tests run the identical
    job via `settle_accent_tasks`), finished by `AccentWriteFinished` against
    live state. The split keeps the old ordering: snapshot persisted *before*
    the task spawns, `last_written` persisted in the completion handler only
    after the write landed (its persist failing chains an async rollback
    task), and the plan's builders travel into the task — no re-read TOCTOU.
  - **Write guard** (`accent_inflight` + generation counter, stale
    completions ignored): at most one theme task ever runs, and while one
    does (a) `AccentComputed` results are dropped-and-rearmed
    (`accent_recompute_queued`), (b) no `ConfigUpdated` flip is routed, and
    (c) toggles are recorded (`accent_flip_requested`, rendered by the
    toggler via `accent_toggler_state`, pinned to disk raw).
    For an active leader, `ConfigUpdated` **never adopts the three accent
    fields from a watcher payload, in-flight or not**: payloads are read at
    event time and can be delivered late, so even after the guard drops an echo can carry
    mid-flight state (adopting its `last_written: None` disarms spuriously
    on the next recompute — the oscillation class); the active leader's
    in-memory accent state is authoritative, and a not-in-flight
    payload flip is only routed after a **fresh disk read** confirms it.
    A follower is the explicit exception: it never owns the accent lifecycle,
    adopts only a freshly confirmed `accent_enabled` value for display, and
    preserves its in-memory snapshot/last-written fields until a takeover
    hydration replaces the complete trio from disk. Its popup toggle persists
    only the raw enabled key and runs no snapshot, restore, compute, or write.
    The completion reconciles once: a recorded user toggle wins (it is the
    newest action whose ordering is known), else a genuine external flip — evidenced by
    the disk flag read **before the completion's own persists** (they
    rewrite that very key: `arm_accent_enable` pins it `true`, a disable
    completion's `set_config` rewrites it `false` — reading after them
    reads our own write back and stomps the flip) differing from the
    flight's **spawn-time baseline** (`accent_disk_enabled_at_spawn`; a
    bare disk-vs-memory compare would misread the enable's
    flag-lands-last ordering as an external disable). Never the
    suppressed echo payloads.
  - **Don't-clobber / disarm**: before each write, current builder accents are
    compared to `accent_last_written` in exact `[u8; 3]` space (we quantise,
    write the `u8/255` f32 via `set_accent` — exact-f32 RON round-trip — and
    persist the same array); before the first successful write the enable-time
    snapshot stands in for `last_written` (so a user pick after a transient
    write failure still counts); any mismatch → **Disarm**: flip the toggle
    off through the config setter, clear last-written, **no restore** — a
    manual choice stands. Whether the snapshot survives the disarm depends on
    what the mismatch can be: after a *recorded* write it is genuinely the
    user (`keep_snapshot: false` — their pick supersedes the record), but in
    the enable→first-write gap (`last_written` still `None`) it is
    indistinguishable from our own **unrecorded** write (crash / failed
    persist between theme write and record), so the gap disarm keeps the
    snapshot (`keep_snapshot: true`) for the next enable's deferred restore —
    the least-lossy rule. When the computed pair *equals* `last_written` the
    plan returns **Skip** — disk is provably right, and rewriting would fire
    theme-change notifications into every COSMIC app on each startup
    reconciliation / same-hue apply. A failed `write_accents` **rolls back**
    whatever (possibly) landed to the accents the plan compared, per mode,
    best-effort — a half-write left on disk (light lands before dark; a
    builder key can land without its theme) is *our* colour, which the guard
    cannot tell from user intervention.
  - **Snapshot lifecycle**: enable snapshots the *live* accents (disable and
    a recorded-write disarm clear the snapshot, so a normal re-enable
    re-snapshots) and clears any stale last-written — **unless** a snapshot
    survived a disabled period (a disable or gap disarm that couldn't
    restore): that one is the only record of the user's pre-feature accents
    while the disk may still hold ours, so enable restores it (the deferred
    restore) and keeps it, refusing to arm if that restore fails. Disable
    restores the snapshot verbatim — including `None` = palette default —
    then clears both; a disable whose restore *fails* (handles missing or the
    write erroring) keeps the snapshot while still turning off — the next
    enable's deferred restore is the retry, so **no failure path ever clears
    the snapshot without a successful restore**. Enable *refuses* without
    theme handles or a persistable applet config (memory-only state breaks
    restart reversibility). External `accent_enabled` flips arriving via
    `ConfigUpdated` route through the same toggle path, never adopt the flag
    silently — and every refusal to enable pins `accent_enabled = false` back
    onto the disk config (raw `ConfigSet::set`; the external flip is already
    persisted, and a disk-enabled/memory-disabled split would re-arm at the
    next startup over state the toggler never built).
  - **Checked persists**: the accent state machine's config persists (the
    enable-time snapshot/toggle, `snapshot_now`, `last_written`) go through
    the derive's per-key setters — which return the error — never the
    warn-and-swallow `set_config`: the snapshot must be on disk *before* the
    first write it undoes (persist failure aborts the write), and a
    `last_written` that cannot be persisted rolls the theme write back so the
    guard still holds. The setters mutate the field before writing, so every
    error path also rolls the in-memory field back. When the rollback *itself*
    fails too (themes and config failing together), the themes keep the new
    pair, memory adopts it — and the **on-disk record is repaired**
    (`finish_rollback`): memory now equals the themes, so every later
    recompute Skips and nothing would ever overwrite the stale on-disk
    record — a restart would hit the destructive
    `Disarm { keep_snapshot: false }`. Best-effort ladder: persist
    `Some(pair)` (a restart then Skips); failing that, clear it to `None`
    (the gap shape, whose disarm keeps the snapshot); only both failing —
    the config wholly unwritable — leaves the destructive shape, with
    nothing writable left to repair it. Remaining exposure is a
    genuine crash between the theme write and its record — accepted, and the
    gap disarm keeping the snapshot makes even that recoverable via
    re-enable. That exposure was **observed in the wild** on 2026-08-10: the
    out-of-order `xdg_popup` destroy (see "UI conventions → Popup stack")
    killed the process inside that ~800 ms window, so the next start read
    builders ≠ record and hit `Disarm { keep_snapshot: true }` — the
    "accent toggle switches itself off after a restart" report. Nothing in
    the accent state machine was wrong; the exposure is accepted *on the
    premise that the applet does not crash*, and the popup fix restores that
    premise. Don't add a write-ahead record here over a repeat report until
    the crash is ruled out.
  - Recompute runs on every successful apply (`app.rs`'s `on_apply_success`,
    all three runtime paths), as a startup reconciliation from `init`
    (startup does not pass through `on_apply_success`), and again at
    `ThumbnailsReady` — the startup compute can land before its thumbnail is
    cached, and the pass ending is what makes a retry able to succeed (the
    steady-state Skip makes the repeat free). Extraction is async
    with the source path as staleness guard, decodes only `thumbs::is_cached`
    slots (never `ensure_thumbnail` here — a `Failed` slot would re-decode the
    full UHD file on every apply), and every failure path (missing/failed
    thumbnail, unwritable config) logs via `tracing` and leaves the user's
    accent untouched.
- `src/schedule.rs` — pure timing math: `next_refresh` (reference-exact,
  including the out-of-range reset to 60 s and the +300 s fudge),
  `shuffle_interval` (sanitizes hand-edited values — `0`/tiny must never
  strobe), `download_horizon(retention_days)` (how many newest archive
  positions a refresh *downloads* — 1–7 → that many, else all eight; the
  list request itself is the constant full window), `retention_reduced`.
- `src/testutil.rs` — test-only loopback HTTP mock server (`spawn_mock`) and
  in-memory JPEG factory; all network branches are tested hermetically, nothing
  ever reaches the real Bing. Its `surface` submodule holds the popup-ledger
  test surface shared by `app::tests` and `view::tests`: builders for the
  `cosmic::surface::Action`s the ledger routes, and `emitted(task)` — which
  drains a returned `Task` into an ordered `Vec<Emitted>`. Assert **emissions**,
  not just `dropdowns_open`: the count is moved by a statement separate from
  the `surface_task(..)` calls, so ledger-only tests keep passing with every
  destroy deleted (they did).

Design decisions, live-verified Bing/cosmic-bg facts, and per-task
implementation notes live in `docs/plans/` (`20260807-cosmic-bing-wallpaper-applet.md`
for the applet itself, `20260808-ux-polish-lockscreen-i18n.md` for tooltips /
disabled styling / i18n / theme conformance, `20260808-accent-from-wallpaper.md`
plus its `-notes.md` for the accent feature, `20260810-popup-destroy-order-crash.md`
for the popup-stack invariant and its libcosmic source-line evidence, and
`20260817-single-instance-leader.md` for multi-output ownership and peer
coordination — each in
`docs/plans/` or, once archived, `docs/plans/completed/`). Gotchas recorded
there worth knowing: the
`zune-jpeg` `log`-feature workaround in `Cargo.toml`, the transitive
`cosmic-config` pin living only in `Cargo.lock`, and the live-verified
lock-screen propagation chain (applet → cosmic-bg config → cosmic-bg state →
greeter state-watch — our end is intact and stays that way).

⚠️ **Both of that plan's lock-screen conclusions were wrong; corrected in place
there (2026-08-08, read against the installed cosmic-greeter 1.5.0 sources;
churn/heal claims re-corrected 2026-08-11 by the lockwatch plan's dedupe
finding).** Two independent facts, worth knowing before anyone "fixes"
`wallpaper.rs` over a lock-screen report:

- **The lock screen genuinely does not follow (observed by the user, then
  traced).** Not our chain — cosmic-greeter's locker cache. A **delivered**
  cosmic-bg state update clears `surface_images` and rebuilds
  (`src/locker.rs:1023-1027`) — and *delivered* means value-changed:
  cosmic-config's subscription only forwards keys the derive's value-equality
  `update_keys` guard reports changed, so an identical rewrite (inotify and
  all) is deduped. The rebuild silently `continue`s past any surface missing
  from `surface_names` (`src/common.rs:148-150`); unlocking removed those ids
  (`src/locker.rs:1004`, `1133`); locking re-inserts the names
  (`src/locker.rs:968`) but never rebuilds — so `view_window` serves the bundled
  `res/background.jpg` (`src/locker.rs:1167-1172`). First lock after login is
  right; later locks are the default — and cosmic-bg's rewrite of the state
  every `rotation_frequency` seconds (`cosmic-bg/src/wallpaper.rs:320-352`)
  never heals a live lock, because with a single-file source every tick
  rewrites an *identical* value that the dedupe swallows; only a genuine value
  change landing mid-lock (daily auto-apply, shuffle) reaches the locker. Note
  the skip is **silent** — an empty journal is not evidence the wallpaper
  arrived, which is exactly the wrong inference made once already. Filed
  upstream as
  [cosmic-greeter#511](https://github.com/pop-os/cosmic-greeter/issues/511)
  (their #460/#497 are the same symptom without a repro). The greeter bug
  itself is not fixable here — but the applet ships a workaround: the
  `src/lockwatch.rs` state poke (see its architecture bullet) writes a
  value-*toggled* `wallpapers` list on lock/resume, and self-neutralizes into
  a harmless extra rebuild once cosmic-greeter#511 ships. Don't remove it as
  "not ours to fix".
- **Permissions are not the login-screen gate; the display manager is.** The
  greeter process never opens the image: `cosmic-greeter-daemon` runs as root and
  reads each user's cosmic-bg state *and* the bytes inside `run_as_user`
  (`daemon/src/main.rs` — HOME swap + `initgroups`/`setegid`/`seteuid`), then
  ships them as RON over the system bus for the greeter to render from memory
  (`bg_path_data`, `src/common.rs`). So a `drwxr-x---` home is no barrier and
  loosening home permissions is not a workaround to suggest. What decides it is
  greetd + `cosmic-greeter.service` + `cosmic-greeter-daemon.service` being the
  login path (on this machine all three are disabled and GDM is the DM).

## UI conventions

Anything that renders *outside* its parent's bounds must be a real wayland
popup, never an iced overlay — an overlay is clipped to the applet popup
surface. Two cases, same rule:

- **Dropdowns** use `widget::dropdown::popup_dropdown(..)` with the
  `Message::DropdownSurface(cosmic::surface::Action)` forwarder (see the
  interval/retention rows in `view.rs`). There is deliberately **no** blind
  `Message::Surface` forwarder any more — see "Popup stack" below.
- **Tooltips** go through `crate::tooltip::tooltip(..)`, never `widget::tooltip`
  (an overlay) and no longer `Core::applet_tooltip` — see `src/tooltip.rs` for
  why upstream's copy is unusable here (its surface is painted in the *same*
  colour as the popup, so the label had no readable background). Inside the
  popup call `view::popup_tooltip(window, content, text)`, which pins the two
  non-obvious arguments: `parent_id: window.popup` (parent to the popup, not
  the panel) and `suppressed: tooltip_suppressed(window)` (see "Popup stack").
  **The panel button deliberately has no tooltip** (`app::Window::view`):
  status applets don't announce their own name on hover, and ours was the only
  tray icon doing it. libcosmic's applet example does wrap the panel button —
  don't "restore" it from there, and don't reintroduce a `panel-tooltip`
  message id.

**Popup stack: `window.popup` may have at most ONE child popup at a time.**
Everything parented to it — the tooltip and either dropdown menu — is a
*sibling* on one xdg-shell stack, and the protocol only permits destroying the
**topmost** popup of a stack; violating it is a fatal `xdg_popup was destroyed
while it was not the topmost popup` (the 2026-08-10 crash, plan
`docs/plans/completed/20260810-popup-destroy-order-crash.md`). Two libcosmic
facts make the invariant the only actionable rule — verified against the
pinned rev:

- the runtime's `Action::Destroy`
  (`iced/winit/src/platform_specific/wayland/event_loop/state.rs`) descends
  one child *per level* (`.position(|p| p.data.parent…)`), so with two children
  it destroys the parent while a sibling is still mapped;
- a dropdown's window id is minted inside the widget
  (`window::Id::unique()` into private state) and first becomes visible in the
  `DestroyPopup` it emits when it *closes* — we can never destroy a menu
  ourselves, so no ordering function can be written.

With one child the runtime's own descent is already correct. **Scope, so this
is not over-read:** the invariant governs the destroys *we* emit. It does not
and cannot cover compositor-initiated dismissal, which is an independent
upstream destroy-order bug that trips with a single child — see residual path
(a) below, and read it before blaming this ledger for any `xdg_popup` error.
On the create side upstream is partly self-healing already (a create whose
requested parent is not the last entry of `self.popups` destroys everything
above it, topmost-first, then retries — `state.rs`, `parent_mismatch`), so the
ledger's real job is the destroys and the ordering, not the creates. The
invariant is held by **one counter** on `Window` (`dropdowns_open`) plus one
idempotent task — see `Window::on_tooltip_surface`, `on_dropdown_surface`,
`on_popup_closed` and the free `destroy_tooltip` in `app.rs`:

- **`destroy_tooltip()` is idempotent** — the runtime's `Destroy` arm logs
  `"No popup to destroy"` and returns *before touching state* for an unmapped
  id. That is what removes the need for a second flag: emit it whenever a
  tooltip *might* be mapped instead of tracking whether one is. (A "tooltip
  open" flag could only be set on the widget's `Action::Task` — arming, not
  creation, since the create the delayed future resolves to goes straight to
  the runtime and never back through `Message` — and five tooltip widgets
  publishing arm/leave in *widget-tree* order rather than pointer order make
  such a flag wrong exactly when it matters.)
- **Interlock** — a dropdown create chains `destroy_tooltip()` *ahead* of the
  forwarded create (chained, never batched): that instant is the last moment a
  tooltip is legally topmost. Unconditional, and legal either way — no tooltip
  mapped is the no-op, and a tooltip that reached the surface while a menu was
  already up was mapped *above* it, i.e. is itself topmost.
- **Suppression** — while `Window::dropdown_open()`,
  `view::tooltip_suppressed` makes `tooltip(..)` withhold its settings
  closure, so no tooltip can arm. In the
  real pointer flow this is the rule that actually holds the invariant: the
  widget publishes `on_leave` on the `CursorMoved` that takes the pointer off
  the control, which necessarily precedes clicking a dropdown button.
- **Drop, then re-emit** — while `Window::dropdown_open()`, *everything* the
  tooltip publishes is dropped (a create would map a second child; a destroy
  would target a non-topmost popup). Nothing is lost, because a dropdown destroy —
  and any `PopupClosed` — chains `destroy_tooltip()` *after* the menu is gone.
  This is the old "deferral" collapsed into the destroy's idempotence; don't
  reintroduce a `tooltip_destroy_deferred` flag.

Rules for touching any of this:

- **Every popup parented to `window.popup` must route through a ledger-aware
  message.** Don't add a raw `surface::Action` forwarder back; a blind forward
  is exactly what maps an unaccounted second child.
- **Never clone a `surface::Action`.** The runtime recovers a create's settings
  with `Arc::try_unwrap`, so a surviving clone makes it log
  `"Invalid settings for popup"` and create nothing — silently, logging being
  off by default. Match by reference, move the value into `surface_task`.
- **`PopupClosed` fires for *every* popup we lose, self-initiated destroys
  included.** The `Destroy` arm sends `PopupEvent::Done` for each popup it
  tears down, byte for byte the same emission the compositor path makes, and it
  is translated against the winit-side `surface_ids` map, not the state's own
  (`…/wayland/sctk_event.rs`, `…/wayland/mod.rs`). Do **not** write "an
  explicit destroy sends no event" — an earlier revision of this file did, and
  it is false at the pinned rev. `PopupClosed` is still the *only* signal for a
  dropdown dismissed by grab loss, which publishes no `DestroyPopup`.
- **`TogglePopup` resets the ledger itself** — not because no event comes, but
  because it comes *late*, after `self.popup.take()`, so the "ours" row can no
  longer match it. Every child dies with our popup, so both that path and the
  "ours" row of `on_popup_closed` reset the count to zero; the
  by-elimination row *decrements* instead.
- **A popup session's late closes are booked as a debt, and paid before the
  live count is touched.** This is the popup-session generation counter, kept
  as a debt because `PopupClosed` carries no generation to compare: its payload
  is a bare `window::Id` handed to us by `on_close_requested`. Ending a session
  books one owed close per menu that was mapped, plus (on the `TogglePopup`
  path only) our own popup, whose `Done` is still in flight. The two halves are
  **not** interchangeable:
  - Menus are **anonymous** of necessity — a menu's id is minted inside the
    widget, so a stale menu close cannot be told from a live one by inspection,
    only *counted*. Where the ending is keyed the count rides on it
    (`ClosingPopup::menus_owed`, the `TogglePopup` path); the compositor's
    "ours" row has no id to key to and uses the bare `stale_menu_closes: u32`.
  - Our own popup is booked **by id** (`closing_popups: Vec<ClosingPopup>`) and
    settled only by a `PopupClosed` naming it, because its `Done` may never
    come at all: `self.popup` is set inside the create's *settings* closure,
    which libcosmic runs (`…/src/app/cosmic.rs`, the `Action::AppPopup` arm)
    before the sctk thread is asked for the surface, and a create that then
    fails — or is dropped after five 30 ms deferrals — is only logged, leaving
    an id nothing ever mapped; destroying such an id logs `"No popup to
    destroy"` and emits nothing. An **anonymous** unit for that popup would
    therefore never be paid by its own `Done` and would swallow the next live
    menu's close instead, stranding `dropdowns_open` at one with no menu
    mapped. Don't "simplify" the two halves into one counter. Unclaimed
    entries are never evicted either: evicting one hands its `Done` back to the
    by-elimination row, which is the un-pausing direction.
  - A create that **displaces** an already-set `self.popup` books it the same
    way (`Window::adopt_popup` — the create's settings closure never does a
    bare `self.popup.replace`). `TogglePopup` decides its create branch in
    `update()`, but the id is minted a message round later: libcosmic drains
    *every* queued message before running the actions they produced
    (`iced/winit/src/lib.rs`), and the create is one of those actions
    (`surface_task` is `crate::task::message`, so it re-enters as a message
    and only the next round's `Action::AppPopup` arm runs the closure). Two
    panel clicks in one drain therefore both open. Upstream then destroys the
    displaced popup for real — a create whose parent is not the topmost popup
    takes the `parent_mismatch` path (`…/wayland/event_loop/state.rs`) and
    destroys everything above it, `Done` included — so an unbooked id lands on
    the by-elimination row and decrements a *newer* session's live menu count.
    The count is deliberately **not** reset on a displacement (unlike the
    `TogglePopup` path): nothing is subtracted, so the displaced popup's menus
    stay counted and pair with their own closes.

  **Delivery genuinely is asynchronous** — measured against
  the pinned rev, a self-initiated destroy travels `update()` → `Task` → the
  event queue → `run_action` → `PlatformSpecific::send_action` → a
  `calloop::channel::Sender` → **a separate `std::thread`** running the sctk
  event loop (`…/wayland/event_loop/mod.rs`, `SctkEventLoop::new` spawns it) →
  the `Action::Destroy` arm → `send_event` → an unbounded `Control` channel →
  back onto that same event queue. Four queue hops and a thread boundary, so a
  reopen *and* a fresh menu create can be processed before the `Done`s land
  (they need only be queued behind the closing click — an input burst, or one
  stalled frame). Do **not** write "the destroy is handled locally in
  `state.rs` and pushes its `Done` synchronously" — an earlier review round
  certified exactly that, and it is false. Without the debt those stale closes
  decrement the *new* session to zero with its menu mapped. Paying a debt can
  only withhold a decrement, so the ledger keeps its "biased toward open"
  safety.
- **But a debt must never outlive the session that booked it.** A menu create
  the runtime *drops* (deferred while `state.destroyed` is non-empty — which
  the interlock's own tooltip destroy populates — then abandoned after five
  30 ms retries; or a `pending_popup` a second deferred create replaces) is
  counted and never mapped, so no `Done` will ever pay for it. Re-booked
  unconditionally at every session end, that one phantom unit swallowed the
  next session's live menu close, was re-booked from the count it stranded, and
  killed hover tooltips for the rest of the *process*. Each half is therefore
  bounded, by evidence and never by a heuristic decrement:
  - `ClosingPopup::menus_owed` **dies with its entry**. Within one teardown
    upstream emits every child's `Done` before the parent's (`Action::Destroy`
    collects popup + children, reverses, then iterates `.into_iter().rev()`,
    `send_event`ing each), so once the booked id comes back, anything still
    owed is a menu that never mapped. Only the order *inside* that teardown is
    relied on — never an ordering against the rest of the queue, which is what
    the debt exists for.
  - `stale_menu_closes` is **discarded in `adopt_popup`**, i.e. when the next
    session's popup id is adopted. It is booked only on the compositor path,
    and there `…/handlers/shell/xdg_popup.rs::done` queues the whole dismissed
    chain's closes in one call into `sctk_events`, drained into the same
    channel that carries pointer events — so they precede the click that opens
    the next popup, which is still two message rounds from adopting an id.

  Tests: `a_dropped_menu_create_owes_nothing_past_the_session_that_leaked_it`,
  `a_dismissed_session_owes_no_menu_closes_once_the_next_popup_opens`. Don't
  "simplify" either bound into a plain decrement or a timeout.
- **`dropdowns_open` is a saturating count, biased toward "open", and only a
  *close* lowers it.** A spurious non-zero only pauses tooltips; a spurious
  zero lets one arm beside a mapped menu. It is a count and not a bool because
  menu window ids are minted inside the widget and never visible here, so
  creates and closes can only be paired by arithmetic — and a `Done` for an
  *already gone* menu can be delivered after a newer create, which a bool would
  read as "nothing open" with a menu mapped. The pairing is exact upstream:
  every popup torn down is removed from `self.popups` first (both
  `…/handlers/shell/xdg_popup.rs::done` and the `Action::Destroy` arm), so one
  mapped popup yields at most one `Done`. The create increments; `PopupClosed`
  decrements (unless it settles a debt first — a `closing_popups` entry by id
  or a `stale_menu_closes` / `menus_owed` unit — or names our
  own popup, which resets) and `TogglePopup` resets; a
  `DestroyPopup` **request deliberately does not**. Both rows publish through
  one `Message::DropdownSurface`, and a widget left with a stale `is_open`
  (grab-loss dismissal never reaches its `ButtonPressed` arm, and `iced`'s
  `Row::update` hands the event to *every* child regardless of `capture_event`)
  emits a destroy for an already-dead popup in the same pass as the *other*
  row's create — decrementing on the request would end that pass at zero with a
  menu mapped. A stale destroy is a runtime no-op, so it emits no `Done`; a
  real one always does. Don't "simplify" this back into the destroy arm, and
  don't collapse the count back into a bool.
- **Residual paths, not closed by this work — don't read either as a
  regression.** (a) **Compositor-initiated dismissal is an upstream
  destroy-order bug, and it needs no second child.**
  `…/handlers/shell/xdg_popup.rs::done` builds `to_destroy` by walking *up*
  from the dismissed popup (`[dismissed, parent, …]`, breaking at a
  layer-surface/window parent — it never collects children) and then iterates
  `.into_iter().rev()`, i.e. **ancestor first**. It is missing the
  `to_destroy.reverse()` that `state.rs`'s `Action::Destroy` arm has between
  its up-walk and its down-walk, and each `SctkPopup` dropped in that loop
  destroys its `xdg_popup` (sctk's `impl Drop for PopupInner`), so the wire
  order is inverted. A dropdown menu has `grab: true` and so does our popup, so
  clicking outside an open menu `popup_done`s the chain and libcosmic destroys
  `window.popup` **before** the menu — the fatal error, with one child and no
  tooltip anywhere. Nothing on our side can prevent it: we never learn the
  menu's id, and `PopupClosed` only reaches us afterwards. The same shape hits
  a mapped tooltip when our popup is dismissed. **Clicking the dropdown button
  again while its own menu is open is this path too** — that click is outside a
  `grab: true` popup, so the compositor dismisses the chain before the widget's
  `ButtonPressed` arm can emit an ordered destroy (the same premise that keeps
  `DestroyPopup` from decrementing). Only *picking an entry* is safe: that
  click lands inside the menu, and the widget's own destroy takes the correct
  `Action::Destroy` path. (b) The arm→create gap: the
  interlock fires while the tooltip's 100 ms future is still pending, so it is
  a no-op and the future can still map the tooltip after the menu; the widget
  re-checks `is_hovered` at resolution, so only a no-leave activation
  (touch/keyboard) survives. If a protocol error reappears, note **which
  surface id** it names before concluding the ledger broke — a crash right
  after clicking outside an open dropdown is (a), not us.

Disabled icon buttons: the theme's own disabled styling is a **no-op** for
`Button::Icon` — `on_disabled` differs from `on` in alpha only, and the SVG
rasteriser tints RGB while keeping source alpha; the background it half-fades
is already fully transparent. Dim explicitly instead:
`icon::from_name(..).icon().opacity(icon_opacity(enabled))` in `view.rs`
(`nav_button` open-codes what `button::icon` builds, since that constructor
exposes no path to the inner `Icon`). Don't "fix" this with
`.class(theme::Button::Icon)` — it is already set.

## i18n

Every user-visible string goes through the crate's `fl!` macro; none are
written inline. Ids live in `i18n/en/cosmic_bing_wallpaper.ftl` (the fluent
domain is the crate name) and `i18n-embed-fl` resolves them **at compile time**,
so a typo or a missing id is a build error. `i18n/` holds all 73 locales COSMIC
ships; the 72 non-English ones are machine-generated. Notes:

- `localize()` (the only `DesktopLanguageRequester` caller) is reachable from
  `main` alone — the crate's `fl!` deliberately does *not* call it, unlike
  libcosmic's copy. That is what pins the test binary to `en`, so tests keep
  asserting literal English strings; keep it that way (`loader_is_pinned_to_english`).
- The loader sets `set_use_isolating(false)` — otherwise every placeable comes
  back wrapped in U+2068/U+2069. **It only affects bundles that already exist**,
  and `select`/`load_languages` swap in brand-new ones with fluent's
  `use_isolating: true` default, so it must be re-applied after *every* language
  load: go through `localize::disable_bidi_isolation` / `select_languages`, never
  call `select` directly.
- Localized label arrays must be functions returning `Vec<String>`
  (`shuffle_interval_labels`/`retention_labels`), never consts or `LazyLock` —
  a static would freeze the labels before the language is selected.
- **Adding or renaming a message id means editing all 73 catalogues**, not just
  `i18n/en/`: `every_locale_defines_every_english_message` asserts each locale
  defines *exactly* `en`'s ids, so a lone English addition turns `just check`
  into 72 failures. Same for placeables — a new `{ $variable }` must appear in
  every locale's copy of that message
  (`every_locale_preserves_the_english_placeables`), and no locale may invent a
  `{ reference }` `en` does not have.
- Adding a locale means adding a directory *and* listing it in `COSMIC_LOCALES`
  in `localize.rs` (a sorted array, compared as a set — a locale *swapped* for
  another is caught too). The guard tests there also assert that every locale
  defines exactly `en`'s ids (catching both missing keys and broken fluent
  syntax, which fluent otherwise only logs), that placeables survive
  translation, and that every id is actually rendered by `app.rs`/`view.rs`
  (`every_message_id_is_referenced_by_the_ui` — dropping a tooltip must not
  leave 73 orphaned strings behind).
- `data/…desktop`'s `Comment[<locale>]=` lines are separate from Fluent
  (desktop-entry spec, POSIX locale tags); `Name=` stays untranslated.

## examples/bing-wallpaper-gnome-extension

An independent clone of `git@github.com:neffo/bing-wallpaper-gnome-extension` (version 53)
with its own `.git` directory. **Reference material only — do not modify it, and do not
expect changes here to be tracked by the parent repo.** It is a GJS/GNOME Shell extension
(not COSMIC), so treat it as a source of behavioral patterns rather than code to port.

The patterns worth reading before designing the equivalent COSMIC applet:

- `utils.js` — the data layer. Bing's image-of-the-day is fetched from
  `https://www.bing.com/HPImageArchive.aspx?format=js&n=8`; the downloaded image
  catalogue is persisted as JSON in a GSettings key and manipulated by pure helpers
  (`getImageList` / `setImageList` / `mergeImageLists`, favourite and hidden flags,
  `dateFromLongDate` for Bing's `YYYYMMDDHHMM` timestamps).
- `extension.js` — the panel indicator (`BingWallpaperIndicator`), and all of the
  scheduling logic. Note the two independent timers: `_restartTimeout` (refresh, driven
  off Bing's own update time, backing off to `TIMEOUT_SECONDS_ON_HTTP_ERROR` = 1h on HTTP
  failure) and `_restartShuffleTimeout` (rotate the wallpaper from already-downloaded
  images). Wallpaper is applied by writing `picture-uri` into the
  `org.gnome.desktop.background` schema — this is the part with no COSMIC analogue and
  will need `cosmic-bg` config instead.
- `prefs.js` + `ui/prefsadw.ui` + `schemas/*.gschema.xml` — settings are declared once in
  the GSettings schema and bound to a libadwaita UI file.

Its own commands (run from inside that directory, only if you need to exercise the
reference):

```bash
npm run lint          # eslint *.js
./buildzip.sh         # compile schemas + gettext catalogues, produce the extension zip
./install.sh          # buildzip, unzip into ~/.local/share/gnome-shell/extensions, enable
```
