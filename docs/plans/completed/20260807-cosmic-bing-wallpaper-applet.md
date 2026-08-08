# COSMIC Bing Wallpaper Applet

## Overview

A COSMIC panel applet that brings Bing's image-of-the-day to the COSMIC desktop,
modeled on `examples/bing-wallpaper-gnome-extension` (GJS/GNOME, reference only) but
built as a native libcosmic applet. It fetches Bing's daily wallpaper, applies it via
`cosmic-bg`'s config, lets the user browse previously downloaded images, and optionally
shuffles among them on a timer.

**Scope (v1 = core + shuffle), decided in brainstorm — final:**

- Daily Bing fetch + auto-apply to all displays
- Prev/next browsing of downloaded history (browsing applies immediately), jump-to-newest, refresh-now
- Shuffle among downloaded images on a timer (30 min / 1 h / 6 h / daily)
- Retention setting: keep 3 / 8 / 30 days / forever (default 8)
- All UI lives in the panel popup; status/errors in a footer caption, no notifications

**Explicitly out of scope:** favourites, hidden/trash, market picker (auto only),
resolution picker (UHD hardcoded), custom download folder (`~/Pictures/BingWallpaper`
hardcoded), notifications, lockscreen.

## Context (from discovery)

- Project dir has no source yet; `examples/bing-wallpaper-gnome-extension` is reference material only (do not modify).
- Reference behavior inventory: Bing endpoint `https://www.bing.com/HPImageArchive.aspx?format=js&idx=0&n=8&mbl=1&mkt=` (empty `mkt` = auto — keep it so the fixture URL and client URL agree); image URL = `https://www.bing.com<urlbase>_<res>.jpg&qlt=100`; refresh scheduling math in `extension.js:396-407`; 1 h backoff on error; independent shuffle timer; copyright full-width-paren handling in `extension.js:908-910` (`utils.js:207` handles ASCII parens only).
- **Live-verified facts** (checked 2026-08-07 against the real API and this machine):
  - Bing's JSON `title` field is the literal string `"Info"` — useless. The display
    title must be derived from `copyright` (text before the parenthesised `(© …)` part).
  - UHD assets are ~5 MB JPEG at 3840×2160 — the popup must show a cached thumbnail,
    never decode the full file.
  - `cosmic-bg` config on this machine: single `all` entry (RON) at
    `~/.config/cosmic/com.system76.CosmicBackground/v1/all` with `source: Path(...)`,
    `scaling_mode`, `filter_method`, etc.
- Environment: Fedora, rustc 1.97.1, COSMIC installed and running.
- Repo note: **decided with user 2026-08-07** — standalone repo (`git init` in `cosmic-wallpaper-applet/`); `examples/` stays untracked via `.gitignore` (it is an independent clone with its own `.git`).

## Development Approach

- **Testing approach: Regular** (code first, then tests within the same task)
- Complete each task fully before moving to the next
- Make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
  - Unit tests for new/modified functions, success and error scenarios
  - UI/iced view code is exempt from unit tests (verified manually in the panel); everything pure is not
  - Tests must never touch the real user config/state — inject paths, or point
    `XDG_CONFIG_HOME`/`XDG_STATE_HOME` at a `tempfile::TempDir`
- **CRITICAL: all tests must pass before starting next task** (`cargo test`)
- **CRITICAL: update this plan file when scope changes during implementation**
- `cargo clippy` clean and `cargo fmt` after each task

## Testing Strategy

- **Unit tests**: required for every task touching pure logic — Bing JSON parsing
  (checked-in fixture), URL/filename building, catalogue merge + prune + rebuild
  roundtrip, next-refresh math, shuffle pick + delay, cosmic-bg entry mutation helper.
  Use `tempfile` for filesystem tests; no network in tests (HTTP layer stays thin,
  parsing is pure).
- **e2e tests**: none (no e2e infrastructure for panel applets); manual smoke testing
  in the panel per the Post-Completion checklist.
- Test command: `cargo test`

## Progress Tracking

- Mark completed items with `[x]` immediately when done
- Add newly discovered tasks with ➕ prefix
- Document issues/blockers with ⚠️ prefix
- Keep plan in sync with actual work done

## Solution Overview

Single self-contained libcosmic applet binary (`cosmic::applet::run`), no daemon —
timers only run while the panel runs, same trade-off as the GNOME extension.

- **UI**: `cosmic::Application` implemented in applet mode; panel button is a symbolic
  icon; one popup window holds everything (COSMIC applet popup idiom: padded rows,
  dividers, toggler rows — study `pop-os/cosmic-applets` for exact widgets/helpers).
- **Wallpaper application**: write `source: Path(<file>)` into the
  `com.system76.CosmicBackground` v1 config via the `cosmic-bg-config` crate,
  preserving the user's other fields (`scaling_mode`, `filter_method`, …). cosmic-bg
  watches its config and transitions live — no process management needed.
- **Own settings** (shuffle on/off, interval, retention days) via `cosmic-config` under
  the applet's app ID `io.github.ercling.CosmicBingWallpaper`; **image catalogue** as
  JSON + cached thumbnails in `~/.local/state/io.github.ercling.CosmicBingWallpaper/`
  (one name everywhere: the app ID).
- **Scheduling**: on startup restore from catalogue instantly (no network); empty
  catalogue (cold start) triggers a first fetch ~5 s after startup; otherwise the next
  refresh is computed from the newest `fullstartdate` (exact math in Task 7). On
  HTTP/parse error retry in 1 h and surface it in the status footer. Shuffle is an
  independent timer; manual prev/next resets it.

### Key design decisions

- **Browsing is setting**: prev/next apply the wallpaper immediately — no separate
  "apply" button (keeps the popup one-glance simple).
- **Filename compatibility** with the GNOME extension
  (`<startdate>-<name>_<res>.jpg` in `~/Pictures/BingWallpaper`; we write `_UHD`, but
  read/rebuild/prune accept any resolution suffix so an existing folder from any
  reference-extension setting migrates).
- **Catalogue is rebuildable**: if JSON is corrupt/missing, rescan the folder by
  filename pattern. The filename ↔ `urlbase` mapping is deterministic both ways
  (`20260807-ColorfulCop_ROW6097405388_UHD.jpg` ↔
  `/th?id=OHR.ColorfulCop_ROW6097405388`), so rebuilt entries dedupe cleanly against
  the next fetch — no duplicates, no re-downloads. Titles refill on next fetch.
- **Never delete the currently applied file** during retention pruning.
- **Don't clobber the user's own wallpaper**: auto-apply happens (a) on the very first
  successful fetch after a cold start (the reason the user installed the applet), and
  (b) on later fetches only when the currently applied `source` is a file inside our
  download folder. If the user picked another wallpaper in COSMIC Settings, the applet
  downloads but does not apply until they act (prev/next/newest/shuffle).
- **`same-on-all`**: applying sets `same-on-all = true` and writes the `all` entry —
  matches the "applies to all displays" scope. Per-output background setups are
  intentionally collapsed on first apply (documented in README).
- **Accepted v1 limitation**: wallpaper changes made externally (COSMIC Settings)
  while the applet runs are not watched; the applet's idea of "current" refreshes on
  next apply or restart. Documented in README.

## Technical Details

### Crates

- `libcosmic` (git `pop-os/libcosmic`, applet + tokio + wayland features) — copy
  current feature flags from a small applet in `pop-os/cosmic-applets`
- `cosmic-bg-config` (git `pop-os/cosmic-bg`)
- `image` (thumbnail generation: decode JPEG, resize to 480×270, save)
- `reqwest` (rustls-tls), `serde`/`serde_json`, `chrono`, `dirs`; `tempfile` (dev)
- **Both git deps pinned with `rev = "<sha>"`** and `Cargo.lock` committed — libcosmic
  APIs move fast; an unpinned clone must not break weeks later.
- ⚠️ Verify exact API names against upstream before coding against remembered ones.

### Data structures

```rust
// catalogue.rs
struct ImageEntry {
    urlbase: String,        // dedupe key, e.g. "/th?id=OHR.Foo_EN-US1234"
    startdate: String,      // "YYYYMMDD"
    fullstartdate: String,  // "YYYYMMDDHHMM" UTC
    title: String,          // derived from copyright (Bing's `title` field is "Info")
    copyright: String,      // "(© ...)" portion
    copyrightlink: String,
    filename: PathBuf,      // absolute path of downloaded file (any _<res> suffix)
}
struct Catalogue { images: Vec<ImageEntry> /* sorted ascending by fullstartdate */ }

// config.rs (cosmic-config, version 1)
struct AppletConfig {
    shuffle_enabled: bool,        // default false
    shuffle_interval_secs: u32,   // 1800 | 3600 | 21600 | 86400; default 86400
    retention_days: u16,          // 3 | 8 | 30 | 0 (= forever); default 8
}
```

### Processing flow

1. Startup → load `AppletConfig` + `Catalogue` (rebuild from folder scan on corruption)
   → popup state ready without network → schedule refresh (immediate-ish on cold
   start) and shuffle if enabled.
2. Refresh fires (or "Refresh now") → GET Bing JSON with
   `n = min(8, retention_days)` when retention is 1–8, else 8 → for each image not on
   disk, download `https://www.bing.com<urlbase>_UHD.jpg&qlt=100` + generate 480×270
   thumbnail → merge into catalogue (dedupe by `urlbase`) → prune retention →
   auto-apply per the "don't clobber" rule → reschedule from newest `fullstartdate`.
3. Prev/next/newest/shuffle-tick → pick entry → write cosmic-bg `source` → update
   preview + reset shuffle timer (for manual navigation).

### Popup layout (top → bottom)

1. Thumbnail of currently applied wallpaper (cached 480×270, popup width); click opens the full image in default viewer (`xdg-open`)
2. Title heading + dimmed copyright caption + "About this image" link → `copyrightlink` in browser
3. Control row, 4 icon buttons: ← prev · next → · ⇥ newest · ⟳ refresh now
4. Divider
5. Shuffle toggler row; when on, interval dropdown row (30 min / 1 h / 6 h / daily)
6. Divider
7. "Keep images" dropdown (3 / 8 / 30 days / forever)
8. Status footer caption: "Updated today at 09:12" / "Bing unreachable — retrying in 1 h"

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code, tests, packaging files, docs in this repo
- **Post-Completion** (no checkboxes): manual panel smoke testing (including install + add-to-panel), repo/git decision with user, potential Flathub/COPR packaging

## Implementation Steps

### Task 1: Scaffold buildable applet skeleton

**Files:**
- Create: `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` (if needed), `.gitignore`
- Create: `src/main.rs`, `src/app.rs`

**Upstream API notes (verified 2026-08-07 against live sources):**

- Pinned revs: libcosmic `8a017a15ee7753241c0a631b4294a68edf79b13b`, cosmic-bg
  `1685f7fc99cbb9cbe981ac672d6451ba6faff7db` (both = upstream HEAD today;
  cosmic-applets reference tree read at `ec8ffdc85d1f316b387cf89672609933064e6e88`).
- libcosmic feature flags (copied from cosmic-applets workspace `Cargo.toml`):
  `default-features = false`, features `applet, applet-token, dbus-config,
  multi-window, tokio, wayland, desktop-systemd-scope, winit` — all confirmed to
  exist in libcosmic's `[features]` at the pinned rev. Crates use edition 2024.
- Applet entry point: `cosmic::applet::run::<App>(())`; `App: cosmic::Application`
  with `type Executor = cosmic::SingleThreadExecutor`, `const APP_ID`,
  `init/core/core_mut/update/view/view_window/on_close_requested`, and
  `fn style() -> Option<iced::theme::Style>` returning `Some(cosmic::applet::style())`.
- Popup idiom (current, from `cosmic-applet-power/src/lib.rs`): panel button is
  `self.core.applet.icon_button(name).on_press_down(Msg::TogglePopup)`; open =
  `cosmic::surface::surface_task(cosmic::surface::action::app_popup(|_| Default::default(), |app| { … core.applet.get_popup_settings(core.main_window_id().unwrap(), new_id, None, None, None) }, None))`;
  close = `cosmic::surface::surface_task(cosmic::surface::action::destroy_popup(id))`;
  popup content wrapped in `self.core.applet.popup_container(content)`. Popup-row
  helpers for Task 8: `cosmic::applet::{menu_button, padded_control}` +
  `cosmic::widget::divider`.
- cosmic-bg-config API (`config/src/lib.rs` in pop-os/cosmic-bg): consts
  `NAME = "com.system76.CosmicBackground"`, `BACKGROUNDS`, `DEFAULT_BACKGROUND = "all"`,
  `SAME_ON_ALL = "same-on-all"`; `context() -> Result<Context, cosmic_config::Error>`;
  `Context::{backgrounds, default_background, entry(output), same_on_all, set_same_on_all}`;
  types `Entry { … }` (`Entry::new(output, source)`, `Entry::fallback()`, `entry.key()`),
  `Source::Path(..)`, `ScalingMode`, `FilterMethod`, `SamplingMethod`, `Color`,
  `Gradient`; higher-level `Config::{load(ctx), entry(output), entry_mut, set_entry, load_backgrounds}`.
- Gotcha: cosmic-bg-config depends on `cosmic-config` from the libcosmic repo
  **unpinned**; a `[patch]` to unify it with our rev-pinned libcosmic is rejected by
  cargo while upstream HEAD == the pinned rev ("points to the same source"), so the
  transitive source is pinned via the committed `Cargo.lock` only (two identical-sha
  `cosmic-config` entries in the lock — harmless duplication).
- `rust-toolchain.toml` not needed: system rustc 1.97.1 builds edition-2024 crates.

**Build-environment notes for this machine (discovered during Task 1):**

- ⚠️ linuxbrew's `pkg-config` shadows the system one and misses `/usr/lib64/pkgconfig`,
  so `smithay-client-toolkit` fails on `xkbcommon`. All cargo build/test/clippy
  invocations need `PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig` exported.
- ⚠️ zune-jpeg 0.5.x (image's jpeg decoder) fails to compile on rustc 1.97 unless its
  `log` feature is on (no-op `warn!` in expression position, `src/mcu_prog.rs:463`);
  worked around with a direct `zune-jpeg` dep enabling `["std", "log"]` (features are
  additive) — see comment in `Cargo.toml`.
- Fedora's rustc package ships without rustfmt/clippy and no sudo was available;
  `rustfmt`, `cargo-fmt`, `cargo-clippy`, `clippy-driver` (1.97.1-1.fc44, exactly
  matching system rustc) were extracted from the Fedora RPMs into `~/.local/bin`
  (already on PATH). `cargo fmt` / `cargo clippy` now work normally.

- [x] verify current libcosmic applet API + Cargo git deps by reading one small applet in `pop-os/cosmic-applets` (e.g. cosmic-applet-battery) and `pop-os/cosmic-bg`'s config crate; record exact feature flags and API names as comments/notes here in the plan (used `cosmic-applet-power` — the cleanest icon-button + popup example; notes above)
- [x] `Cargo.toml` with libcosmic (applet features), cosmic-bg-config, image, reqwest(rustls), serde, serde_json, chrono, dirs; `tempfile` as dev-dep (plus tokio, tracing, tracing-subscriber, and the zune-jpeg feature workaround)
- [x] pin both git deps with `rev = "<sha>"` (current upstream HEAD) and commit `Cargo.lock`
- [x] `src/main.rs` + `src/app.rs`: minimal `cosmic::Application` in applet mode — panel icon button toggles an empty popup ("Hello" placeholder)
- [x] `cargo build` succeeds; binary runs standalone without panicking (ran 5 s under `timeout`, clean until SIGTERM)
- [x] add a trivial smoke test (e.g. config default values) so `cargo test` runs green (APP_ID + symbolic-icon smoke tests in `src/app.rs`)
- [x] run tests - must pass before task 2 (`cargo test`: 2 passed; clippy clean; fmt clean)

### Task 2: Bing API module — types, parsing, URL/filename building

**Files:**
- Create: `src/bing.rs`, `tests/fixtures/hpimagearchive.json`

- [x] fetch one real response from `https://www.bing.com/HPImageArchive.aspx?format=js&idx=0&n=8&mbl=1&mkt=` and check it in as the fixture (strip nothing; real shape) (fetched 2026-08-07, 8 images, `tests/fixtures/hpimagearchive.json`)
- [x] serde types for the response (`images[]`: `urlbase`, `startdate`, `fullstartdate`, `copyright`, `copyrightlink`) — do **not** keep Bing's `title` (it is literally `"Info"`) or `wp` (unused; we hardcode UHD)
- [x] title/copyright derivation: `ImageEntry.title` = `copyright` text before the parenthesised part, `ImageEntry.copyright` = the `(© …)` part; handle Japanese full-width parens `（）` (see `extension.js:908-910`) and copyright strings with **no** parens (whole string becomes title, copyright empty — reference crashes on this, we must not) (`split_copyright`; stores the notice **without** the surrounding parens, matching the reference's `match[1]` incl. its `**` stripping)
- [x] pure builders: `image_url(urlbase) -> String` (`…_UHD.jpg&qlt=100`) and `image_filename(startdate, urlbase) -> String` (`<startdate>-<name>_UHD.jpg`, name = urlbase minus `/th?id=OHR.` prefix — must match the GNOME extension's naming)
- [x] inverse mapping for catalogue rebuild: `parse_filename("<8digits>-<name>_<res>.jpg") -> Option<(startdate, urlbase)>` reconstructing `urlbase = "/th?id=OHR." + name` (accept any `_<res>` suffix, not just `_UHD`)
- [x] write tests: fixture parses; derived title is a real title (not `"Info"`); URL/filename builders match known-good values; `parse_filename(image_filename(..)) ` roundtrips; non-UHD suffixes parse
- [x] write tests: malformed/empty JSON → error, not panic; no-parens copyright
- [x] run tests - must pass before task 3 (`cargo test`: 13 passed; clippy clean; fmt clean)

### Task 3: Bing HTTP client + image download + thumbnail

**Files:**
- Modify: `src/bing.rs`
- Create: `src/thumbs.rs`

- [x] async `fetch_image_list(n)` using reqwest with a custom User-Agent; maps HTTP/parse failures into one error enum (`FetchError { Http, Status, Parse, Io }` with Display/Error/source; `http_client()` sets the UA + timeouts, fetch takes `&Client` for reuse)
- [x] async `download_image(entry, dir)` → writes to `~/Pictures/BingWallpaper/<filename>` (path via Task 2 builder); creates dir if missing; skips if file already exists; downloads to `.part` then renames (no torn files) (dir passed by caller — hardcoded `~/Pictures/BingWallpaper` wiring lands with the Task 7 pipeline)
- [x] `thumbs.rs`: `thumbnail_path(image_path, state_dir)` + `ensure_thumbnail(...)` — decode via `image` crate, resize to 480×270, cache in state dir; regenerate if missing/stale; never let the UI decode the full 5 MB UHD file (thumbs cached at `<state_dir>/thumbs/<same filename>`; stale = thumb mtime older than source; atomic `.part` write)
- [x] write tests: skip-if-exists decision, `.part` → final rename behavior (tempfile), error enum display, `thumbnail_path` computation, `ensure_thumbnail` regenerates when missing (tiny in-test generated image, not a real UHD asset). Do not re-test Task 2's path builders. (also: fresh-thumb skip via sentinel content, stale-mtime regeneration, missing-source error, `api_url` shape)
- [x] run tests - must pass before task 4 (`cargo test`: 24 passed; clippy clean; fmt clean)

### Task 4: Catalogue — persistence, merge, prune, rebuild

**Files:**
- Create: `src/catalogue.rs`

- [x] `ImageEntry` + `Catalogue` with JSON load/save at `~/.local/state/io.github.ercling.CosmicBingWallpaper/catalogue.json` (path injectable for tests); atomic write (`.tmp` + rename) (`load`/`save`/`load_or_rebuild`; path always caller-supplied; also `ImageEntry::from_bing` helper for the Task 7 pipeline)
- [x] `merge(new_entries)`: dedupe by `urlbase`; when a fetched entry matches a rebuilt one, fill in the missing title/copyright metadata; sort ascending by `fullstartdate` (rebuilt match also adopts the real `fullstartdate` and keeps the existing filename — no re-download; entries with real metadata are never overwritten)
- [x] `prune(retention_days, currently_applied)`: delete files + entries whose `fullstartdate` is older than `now - N days` (reference semantics, `utils.js:546-550`); `0` = keep forever; never delete `currently_applied`; drop entries whose file vanished externally (`now` injected for tests; malformed dates are kept — never delete on a guess; returns deleted paths)
- [x] `rebuild_from_folder(dir)`: scan `^\d{8}-.+\.jpg$` (any resolution suffix, matching the reference's own migration regex `utils.js:477`) using Task 2's `parse_filename` to reconstruct `urlbase`; entries get empty titles, refilled on next fetch merge; `fullstartdate` synthesized as `startdate + "0000"`
- [x] navigation helpers: `newest()`, `prev(current)`, `next(current)`, `random_other(current)` (`random_other` filters candidates first — structurally cannot return current — and picks via clock nanos, avoiding a rand dep)
- [x] write tests (tempfile): load/save roundtrip, corrupt JSON → rebuild path, merge dedupe + ordering, **rebuilt entry + freshly fetched same image → one entry and no re-download**, prune (fullstartdate boundary, forever, protects current, missing files), non-UHD files rebuilt, navigation incl. edge cases (empty, single image, at ends)
- [x] run tests - must pass before task 5 (`cargo test`: 42 passed; clippy clean; fmt clean)

### Task 5: Applet settings via cosmic-config

**Files:**
- Create: `src/config.rs`
- Modify: `src/app.rs`

- [x] `AppletConfig` (shuffle_enabled, shuffle_interval_secs, retention_days) with `CosmicConfigEntry` derive under app ID `io.github.ercling.CosmicBingWallpaper`, version 1, with defaults (false / 86400 / 8) (`src/config.rs`; derive also generates per-field `set_<field>` write-on-change setters)
- [x] load at startup + write-on-change wiring in `app.rs`; missing/invalid config → defaults, never crash (`init` loads via `AppletConfig::context()`+`load` with default fallback; `watch_config` subscription → `Message::ConfigUpdated` keeps in-memory state in sync; `Window::set_config` persists only on change)
- [x] write tests: default values; field roundtrip — either plain-serde level, or through cosmic-config with `XDG_CONFIG_HOME` pointed at a `TempDir` (must not read/write the real user config) (used `Config::with_custom_path` rooted in a `TempDir` — fully isolated without touching process env; also: empty-config → defaults, corrupt key falls back per-field, setter write-on-change)
- [x] run tests - must pass before task 6 (`cargo test`: 47 passed; clippy clean; fmt clean; binary smoke-ran 5 s)

### Task 6: Wallpaper writer (cosmic-bg config)

**Files:**
- Create: `src/wallpaper.rs`

- [x] pure helper `updated_entry(existing: Option<Entry>, path: &Path) -> Entry`: change only `source: Source::Path(..)`, preserve scaling_mode/filter_method/etc.; `None` → build from cosmic-bg's defaults (also pins `output` to `"all"` — the key we write under)
- [x] `apply(path)`: load cosmic-bg config via `cosmic-bg-config` helper, set `same-on-all = true`, write the `all` entry (decision: per-output setups collapse to same-on-all — verify the exact keys cosmic-settings writes in `pop-os/cosmic-bg` + cosmic-settings source); errors surfaced, not panicking (keys verified against cosmic-bg's own watch handler at the pinned rev — `same-on-all`, `all`, `backgrounds`, `output.*`; writes go through `Config::set_entry` + `Context::set_same_on_all` — both change-only — with the `all` entry written **before** flipping `same-on-all` so no old-image flash; errors map into local `WallpaperError` because cosmic-bg-config's `cosmic_config` error type lives in a crate instance we cannot name)
- [x] `current_source() -> Option<PathBuf>`: read back what's applied; also `is_ours(path) -> bool` (file inside `~/Pictures/BingWallpaper`) for the auto-apply rule (`current_source` returns `None` for color sources, unreadable config, or `same-on-all = false` — per-output mode conservatively counts as "not ours"; `download_dir()` helper added here for Task 7 reuse)
- [x] write tests for `updated_entry` (preserves fields, sets source, `None` case) and `is_ours` (inside/outside/relative paths); live config write verified manually (also: output pinned to `all`, color→path source swap keeps scaling, component-wise not string-prefix containment, dir-itself excluded; live apply deferred to Post-Completion manual smoke test — user's wallpaper untouched during implementation)
- [x] run tests - must pass before task 7 (`cargo test`: 55 passed; clippy clean; fmt clean)

### Task 7: Refresh scheduling + fetch pipeline in the update loop

**Files:**
- Create: `src/schedule.rs`
- Modify: `src/app.rs`

- [x] pure `next_refresh(newest_fullstartdate: Option<&str>, now: DateTime<Utc>) -> Duration` with the reference's exact semantics (`extension.js:396-407`): `None` (cold start) → 5 s; otherwise `diff = (fullstartdate + 86400s) - now; if diff < 60 || diff > 86400 { diff = 60 }; diff += 300` — note the out-of-range **reset to 60 s** (not a clamp) and the +300 s fudge added **after**; malformed date → 3600 s (error path, matches the HTTP backoff) (`src/schedule.rs`; also `fetch_count(retention_days)` — needed by the pipeline now, Task 10 only wires the UI — and the pure `should_auto_apply` decision)
- [x] startup flow in `app.rs`: load catalogue → determine current image from `wallpaper::current_source()` → schedule refresh timer as an iced subscription/task (empty catalogue ⇒ fetch fires ~5 s after startup) (one-shot sleeping task carrying a generation number — a stale `RefreshDue` is ignored, so every reschedule atomically replaces the pending timer; `state_dir()`/`catalogue_path()` helpers live in `app.rs`)
- [x] refresh pipeline as async task feeding messages: fetch list (`n = min(8, retention_days)` when 1–8, else 8) → download missing + thumbnails → merge → prune → auto-apply per the "don't clobber" rule: apply if (cold start's first successful fetch) or (`current_source()` `is_ours`) → reschedule; error → status message + 1 h retry (`run_refresh` in `app.rs`; thumbnail/catalogue-save failures are non-fatal (logged), download/list failures abort the run; auto-apply judged against a fresh `current_source()` read at completion time)
- [x] status footer state: `Updated <relative time>` / `Bing unreachable — retrying in 1 h` (store timestamp + last error) (`last_updated` + `last_error` + `refresh_pending` on `Window`; `status_line()` renders the states in the popup placeholder — relative-time polish is Task 8)
- [x] write tests for `next_refresh` (`None`, normal, in-past → 60+300, far-future → 60+300, boundary 60/86400, malformed) and for the auto-apply decision function (cold-start-first-fetch, ours, not-ours) (also: `fetch_count` (1/3/8/30/0), `state_dir` under APP_ID, `status_line` lifecycle incl. restored-catalogue case)
- [x] run tests - must pass before task 8 (`cargo test`: 68 passed; clippy clean; fmt clean; binary deliberately **not** smoke-run — a cold-start run would trigger a real fetch + wallpaper apply ~5 s in)

### Task 8: Full popup UI + navigation controls

**Files:**
- Modify: `src/app.rs` (split out `src/view.rs` if it grows past ~300 lines)

- [x] popup layout per Technical Details: thumbnail (`cosmic::widget::image` on the **cached 480×270 thumb**), title heading, copyright caption, "About this image" link row, control row (prev/next/newest/refresh icon buttons), dividers, status footer — using COSMIC applet idiom widgets (`padded_control`, `menu_button`, `divider` — exact names per cosmic-applets reference) (view split into `src/view.rs`; missing thumbnail — rebuilt entries never fetched — renders an icon placeholder, the UI never decodes the full UHD file; note: `popup_container` caps popup width at 360 px, so the 480×270 thumb scales down to fit)
- [x] wire messages: Prev/Next/Newest → catalogue navigation + `wallpaper::apply` + update current; RefreshNow → trigger pipeline (debounced while pending); thumbnail click → `xdg-open <full image file>`; link → `xdg-open <copyrightlink>` (one `ApplyImage(PathBuf)` message serves all three nav buttons; `Open(String)` spawns `xdg-open` with a reaper thread — no zombies; shuffle-timer reset on manual nav lands in Task 9 where the timer is born)
- [x] disabled states: prev/next at history ends, refresh while pending, everything except status when catalogue is empty ("No images yet — fetching…") (via `on_press_maybe`; also: newest disabled when already applied; foreign current wallpaper → popup shows newest, prev enters history at its newest end, next disabled; empty catalogue shows refresh button + status only so an offline start is retriable by hand)
- [x] write tests for any new pure logic extracted (e.g. relative-time formatting for the footer); view code exempt (`view.rs`: `displayed`/`prev_target`/`next_target`/`newest_target` incl. foreign-wallpaper + empty cases, `display_title` file-stem fallback for rebuilt entries, `format_updated` today/yesterday/older + month boundary)
- [x] run tests - must pass before task 9 (`cargo test`: 76 passed; clippy clean; fmt clean; binary not smoke-run — cold-start would trigger a real fetch + apply)

### Task 9: Shuffle

**Files:**
- Modify: `src/app.rs`, `src/schedule.rs`

- [x] shuffle toggler row + interval dropdown (30 min / 1 h / 6 h / daily) shown when enabled; persists via `AppletConfig` (toggler row per cosmic-applet-tiling's idiom; dropdown via `popup_dropdown` + a `Message::Surface` forwarder — libcosmic's own applet example idiom, so the menu opens as its own wayland popup instead of clipping to the applet popup; rows shown only while the catalogue is non-empty, matching Task 8's empty-state rule; persisted through the existing write-on-change `set_config`)
- [x] pure `next_shuffle_delay(interval_secs, last_user_action: Option<Instant>, now) -> Duration` in `schedule.rs` — first fire one full interval after enabling; manual prev/next/newest resets the countdown (interval − elapsed-since-action, saturating at zero; `None` = full interval — used for startup-restored shuffle and post-tick cycles; interval change also counts as a reset)
- [x] shuffle timer subscription: on tick pick `catalogue.random_other(current)` and apply; timer active only while enabled and catalogue has ≥2 images (one-shot generation-counter timer, same scheme as the refresh timer; `sync_shuffle` re-evaluates on enable/disable, interval change, external config edits, manual nav, catalogue growth after a fetch, and startup)
- [x] write tests: `random_other` never returns current (n≥2) and returns None (n<2) (already covered by Task 4's `random_other_never_returns_current` / `navigation_single_image` / `navigation_on_empty_catalogue`); interval-secs ↔ dropdown-index mapping roundtrip (+ garbage-value fallbacks to daily); `next_shuffle_delay` (fresh enable, after reset, elapsed incl. exact-boundary and saturation)
- [x] run tests - must pass before task 10 (`cargo test`: 81 passed; clippy clean; fmt clean; binary not smoke-run — cold-start would trigger a real fetch + apply)

### Task 10: Retention setting UI + pruning wiring

**Files:**
- Modify: `src/app.rs`

- [x] "Keep images" dropdown (3 / 8 / 30 days / forever) persisted to `AppletConfig` (`retention_row` in `view.rs`, same `popup_dropdown` + `Message::Surface` idiom as the shuffle interval; placed per the popup layout — after the second divider, above the status footer — and hidden while the catalogue is empty, matching Task 8's empty-state rule; persists through the existing write-on-change `set_config`)
- [x] prune runs after every successful fetch and immediately when retention is reduced; current image always protected (guaranteed by Task 4 — covered by its tests) (post-fetch prune existed since Task 7's `run_refresh`; new `prune_immediately` in `app.rs` fires on `SetRetention` when `schedule::retention_reduced(old, new)` — and on externally edited config via `ConfigUpdated` — re-reads `current_source()` first so the live wallpaper is protected, persists the catalogue, and re-syncs the shuffle timer in case the catalogue dropped below 2 images)
- [x] fetch size follows retention: `n = min(8, retention_days)` when 1–8 (don't download 8 × 5 MB just to delete 5) (already wired in Task 7: `schedule::fetch_count` used by `run_refresh`; nothing new needed)
- [x] write tests: retention-value ↔ dropdown mapping; prune-on-change decision logic; fetch-n computation (3, 8, 30, forever) (`retention_mapping_roundtrips` + `retention_mapping_tolerates_garbage` in `view.rs`; `retention_reduced_only_when_the_new_policy_is_stricter` in `schedule.rs` — forever↔finite in both directions, unchanged, loosened; fetch-n already covered by Task 7's `fetch_count_follows_retention` (1/3/8/30/0))
- [x] run tests - must pass before task 11 (`cargo test`: 84 passed; clippy clean; fmt clean; binary not smoke-run — cold-start would trigger a real fetch + apply)

### Task 11: Packaging — desktop entry, icon, justfile

**Files:**
- Create: `data/io.github.ercling.CosmicBingWallpaper.desktop`, `data/icons/*.svg`, `justfile`, `README.md` (stub)

- [x] `.desktop` file modeled on a shipped applet (`/usr/share/applications/com.system76.CosmicAppletBattery.desktop`): `X-CosmicApplet=true`, `NoDisplay=true`, `Categories=COSMIC;`, `StartupNotify=true`, `Terminal=false`, `X-CosmicShrinkable`/`X-OverflowPriority` as appropriate, `Icon=io.github.ercling.CosmicBingWallpaper-symbolic`, `Exec=` as an **absolute path** (cosmic-panel's environment may not have `~/.local/bin` in PATH) (`data/io.github.ercling.CosmicBingWallpaper.desktop`; passes `desktop-file-validate` (only the same `Categories=COSMIC;` hint the shipped applets get); `X-OverflowPriority=10` like battery, `X-CosmicShrinkable` omitted — fixed icon button; Exec is checked in with a placeholder absolute path and rewritten to the real install path by `just install`, since a static file can't know the prefix)
- [x] simple symbolic SVG icon (template style so it follows panel theming) (`data/icons/io.github.ercling.CosmicBingWallpaper-symbolic.svg`: 16×16 picture-frame + sun + mountains, `fill="#232323"` paths exactly like the shipped `-symbolic` applet icons so the panel recolors it; xmllint-valid, raster-previewed; the in-app panel button keeps the stock `preferences-desktop-wallpaper-symbolic` so the binary renders correctly even when run uninstalled)
- [x] `justfile`: `build`, `install` (binary, desktop file, icon into proper hicolor/applications dirs), `uninstall` (default prefix `~/.local` — no sudo needed, overridable via `just prefix=… rootdir=…`; exports the system `PKG_CONFIG_PATH` dirs so builds work despite linuxbrew's pkg-config; bonus `check` recipe = fmt+clippy+test; `install` sed-rewrites `Exec=` to the absolute bin path; dry-run + `--evaluate` verified, `just build` release build verified — `just install` itself deliberately not run, Post-Completion manual step)
- [x] tests: `cargo test` still green (no new logic; packaging only) (84 passed)
- [x] run tests - must pass before task 12 (`cargo test`: 84 passed; clippy clean; fmt clean)

### Task 12: Verify acceptance criteria

- [x] verify all Overview requirements implemented: daily fetch + auto-apply, prev/next/newest/refresh, shuffle with 4 intervals, retention 4 values, popup layout matches Technical Details, status footer shows success + error states (all cross-checked against `app.rs`/`view.rs`/`schedule.rs`: intervals [1800, 3600, 21600, 86400], retention [3, 8, 30, 0], popup order thumbnail → header/caption → About link → controls → divider → shuffle rows → divider → retention row → footer; footer states "Updated today at HH:MM" / "Bing unreachable — retrying in 1 h" plus checking/empty/restored variants; live panel behavior itself is a Post-Completion manual check)
- [x] verify edge cases: cold start with empty folder fetches within seconds, corrupt catalogue rebuild (incl. non-UHD files) with no duplicate downloads afterwards, offline at startup (popup still works from catalogue), retention protects current image, non-Bing current wallpaper is not clobbered by a background refresh, per-output cosmic-bg config collapses to same-on-all on first apply (by design) (⚠️ found + fixed a real gap: `run_refresh` only skipped downloads when the *UHD-named* file existed, so a catalogue rebuilt from non-UHD files (reference-extension folder) re-downloaded ~5 MB per image and orphaned it — added `Catalogue::existing_file(urlbase)` consulted before `download_image`, with tests (`existing_file_finds_rebuilt_files_at_any_resolution`, `existing_file_ignores_entries_whose_file_vanished`); every other edge case verified in code + covered by existing tests; live-session cases (real fetch, live apply, multi-monitor collapse) deferred to Post-Completion manual verification)
- [x] run full test suite: `cargo test` (86 passed, 0 failed)
- [x] `cargo clippy` — no warnings; `cargo fmt --check` (both clean, `--all-targets`)
- [x] verify test coverage: every pure module (bing, thumbs, catalogue, config, wallpaper helpers, schedule) has success + error case tests (bing: fixture parse + malformed JSON/no-parens/reject paths; thumbs: generate/skip/stale + missing-source error; catalogue: roundtrip/merge/prune/nav + missing/corrupt load errors; config: roundtrip + corrupt-key fallback; wallpaper: updated_entry variants + is_ours rejections; schedule: refresh math + malformed-date backoff)

### Task 13: [Final] Update documentation

- [x] write real `README.md`: what it is, screenshot, build/install via `just`, settings explained, disk usage note (~5 MB/image: ≈40 MB at 8-day retention, ≈150 MB at 30), documented limitations (external wallpaper changes not watched; per-output backgrounds collapse to same-on-all) (features, settings table, GNOME-extension folder compatibility, prefix-override install, dev commands incl. the PKG_CONFIG_PATH note; screenshot left as an HTML-comment TODO — cannot be taken without a live panel session)
- [x] update `CLAUDE.md`: replace the "no source code yet" section with actual build/test/run commands and module architecture (build/test/run via `just` with the PKG_CONFIG_PATH workaround and the ~/.local/bin rustfmt/clippy note, cold-start-run warning, test-isolation rule, per-module architecture map of main/app/view/bing/thumbs/catalogue/config/wallpaper/schedule; `examples/` reference section kept unchanged)
- [x] move this plan to `docs/plans/completed/` (moved by orchestrator at completion)

## Post-Completion

**Manual verification:**
- `just install`, add the applet via COSMIC Settings → Desktop → Panel, verify it appears and the icon follows the theme
- Live smoke test on the running COSMIC session: watch first fetch populate `~/Pictures/BingWallpaper` within seconds of a cold start, click through prev/next/newest, toggle shuffle at 30 min and observe a rotation, unplug network and confirm footer error + 1 h retry, confirm cosmic-bg transitions live on apply
- Set a wallpaper via COSMIC Settings, wait for a refresh: confirm the applet does **not** override it
- Multi-monitor check (if available): `all` entry applies everywhere; per-output starting state collapses to same-on-all on first apply
- Restart panel / re-login: state restores instantly without network
- Drop a GNOME-extension folder (mixed resolutions) into `~/Pictures/BingWallpaper` with no catalogue: verify rebuild + no re-downloads

**External decisions / systems:**
- ~~Repo layout decision~~ — resolved: standalone repo, initialized 2026-08-07
- Later distribution (COPR/Flathub) intentionally not planned for v1
