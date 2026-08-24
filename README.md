# Daymural

A COSMIC panel applet that brings Microsoft Bing's image of the day to the COSMIC desktop.
It fetches Microsoft Bing's daily wallpaper (UHD, 3840×2160), applies it via `cosmic-bg`,
lets you browse previously downloaded images from the panel popup, optionally
shuffles among them on a timer, and prunes old images per a retention setting.

Inspired by the [Bing Wallpaper GNOME extension](https://github.com/neffo/bing-wallpaper-gnome-extension),
rebuilt as a native libcosmic applet. Downloaded files use the same
`~/Pictures/BingWallpaper/<date>-<name>_<res>.jpg` naming, so an existing folder
from the GNOME extension is picked up as-is — no re-downloads.

<!-- TODO: screenshot of the panel popup (requires a live COSMIC session) -->

## Features

- **Daily fetch + auto-apply** — checks Bing shortly after its daily update time,
  downloads new images, and applies the newest to all displays.
- **History browsing** — prev / next / jump-to-newest buttons in the popup;
  browsing applies the wallpaper immediately.
- **Refresh now** — manual fetch button; errors surface in the popup footer
  ("Bing unreachable — retrying in 1 h") and retry automatically.
- **Multi-monitor safe** — COSMIC may start one copy of the applet for each
  panel output, but they coordinate so only one performs downloads, scheduled
  work, cleanup, and accent updates. Every popup remains usable: a manual
  refresh from another output is transparently routed through the active copy,
  and browsing still applies the selected wallpaper immediately.
- **Shuffle** — rotate among downloaded images every 30 min / 1 h / 6 h / daily.
- **Retention** — keep 3 / 8 / 30 days of images, or forever.
- **Match accent to wallpaper** — opt-in (off by default): derives the COSMIC
  accent colour from the applied wallpaper's dominant hue and keeps it in step
  across browsing, shuffle, and daily refreshes. Legibility-guarded, with
  separate light- and dark-mode tones. Switching it off restores the accent you
  had when you switched it on; picking an accent yourself in COSMIC Settings
  switches it off and keeps your choice.
- **Image details and credit** — the popup shows the photographer/rightsholder
  credit supplied by Bing. Click the thumbnail to open the full-size image in
  your default viewer, or use "About this image" to open Bing's supplied info
  link in the browser (both launched via `xdg-open`, so `xdg-utils` is needed
  at runtime).
- **Respects your choices** — if you set a different wallpaper in COSMIC
  Settings, the applet keeps downloading but stops auto-applying until you act
  (prev/next/newest/shuffle).
- **Tooltips** — every icon-only control in the popup (prev/next/newest/refresh,
  thumbnail) names what it does on hover; unavailable buttons are visibly dimmed.
  The panel button itself stays silent, like COSMIC's own status applets.
  Hover tooltips pause while a *Shuffle every* or *Keep images* menu is open
  (both open as popups on the applet popup, which may only have one child at a
  time).
- **Localized** — the UI follows your desktop language, with catalogues for the
  73 locales COSMIC itself ships (see [Translations](#translations)).

## Image rights and attribution

This is an unofficial, independent project. It is not affiliated with,
authorized by, sponsored by, or endorsed by Microsoft. The Flatpak and native
packages contain no Bing homepage photographs: each installation fetches image
metadata and JPEGs directly from Bing.

Microsoft says that most Bing daily images may be downloaded for use as
wallpaper, while licensing restrictions make some images unavailable for
download. Its Services Agreement limits Bing and MSN photos and other material
to personal, noncommercial use; downloading, copying, redistributing, or using
that material to build another product otherwise requires authorization from
Microsoft or the relevant rightsholder, or permission under applicable
copyright law. See Microsoft's [Bing homepage guidance](https://support.microsoft.com/en-us/bing/explore-the-homepage-1),
[Services Agreement](https://www.microsoft.com/en-us/servicesagreement#14f_BingandMSN),
and [copyright guidance](https://www.microsoft.com/en-us/legal/intellectualproperty/copyright/permissions).
Attribution identifies the rightsholder but does not itself grant additional
rights.

For an image fetched with Bing metadata, the applet saves the JPEG bytes
exactly as delivered and creates `catalogue.json` (in its state directory)
from the response metadata, storing the caption-derived title, credit, and
information link there. The popup displays that credit and exposes
the link as **About this image**. The applet does not add the JSON attribution
to the JPEG file itself, so copying a JPEG alone does not necessarily carry the
separate credit or link. If the catalogue must be rebuilt from filenames, the
applet immediately asks Bing again and restores the title, credit, and link
for every image still in Bing's eight-image window that Bing still marks
downloadable; older or no-longer-eligible images keep an honest filename
fallback (or are removed, see below).

Downloads honor Bing's per-image `wp` eligibility flag and fail closed: an
image is fetched only when Bing marks it `wp: true`. An image Bing marks
`wp: false` is never downloaded, and one that was downloaded earlier (before
this rule, or after a later licensing change) is removed — entry and JPEG —
so it cannot come back through a catalogue rebuild. An image whose `wp` field
is absent is not downloaded either, but nothing is deleted on that evidence:
a change in Bing's payload must never erase your history. The one exception
is the wallpaper currently on screen, which stays while it is displayed, so
the popup never credits a different image than the one displayed; it is
removed by the first refresh after another image replaces it while Bing
still lists it as ineligible (Bing's window is eight days — an image kept
on screen longer simply ages out through retention like any other). On a
per-output wallpaper setup the applet cannot tell which image is on screen,
so that same caution applies to every image: nothing is removed for
eligibility there. The list request always asks for Bing's supported eight-image
window; the retention setting decides which of those are downloaded, and if
none of them is eligible only the newest eligible image in the rest of the
window is fetched as a fallback (never more, never older pages) — and only
while the applet would actually apply it: over a wallpaper you picked
yourself nothing is auto-applied, so no fallback is downloaded either.

The `HPImageArchive` endpoint has no public third-party product license
identified by this project, so `wp` enforcement and attribution do **not**
amount to a claim that Store publication or every automated download has been
verified as authorized; that remains gated on the applicable Microsoft terms
and any written authorization from Microsoft or the rightsholder.

Troubleshooting: an applet that stays empty with no error shown is usually an
all-restricted day or a payload without `wp`; run it with
`RUST_LOG=daymural=warn` to see the per-refresh eligibility
counts (explicit-`false` and absent-`wp` are reported separately).

Downloaded photographs remain the property of their respective
rightsholders. Use them only as personal, noncommercial wallpaper unless the
rightsholder grants broader permission; do not redistribute them based on this
project. Attribution identifies the rightsholder but does not grant
redistribution permission. The project's GPL-3.0-only license covers the
applet's code and project-owned assets (icon, metadata, translations), not
photographs downloaded from Bing.

## Native build & install

Requires Rust (edition 2024; developed and tested with rustc 1.97),
[`just`](https://github.com/casey/just), and the native libraries libcosmic
links against — `pkg-config`, the libxkbcommon development headers, and the
Wayland development libraries. On Fedora:

```bash
sudo dnf install cargo just pkgconf-pkg-config libxkbcommon-devel wayland-devel
```

```bash
just build              # release build
just install            # install to ~/.local (binary, .desktop, icon)
just uninstall
```

`just uninstall` removes only what `just install` put in place (binary,
desktop entry, icon). Downloaded images (`~/Pictures/BingWallpaper`), the
catalogue/thumbnails (`~/.local/state/io.github.ercling.cosmic-applet-daymural/`),
and settings (`~/.config/cosmic/io.github.ercling.cosmic-applet-daymural/`) are
left behind — delete them by hand if you want a clean sweep.

With either installation method, if **Match accent to wallpaper** is on, switch
it off *before* uninstalling when you want the applet to restore your previous
accent automatically:
the derived accent is written into the system theme and outlives the applet,
while the snapshot needed to restore your previous accent lives in the
applet's settings — delete those and the only way back is picking an accent by
hand in COSMIC Settings.

`just install` needs no sudo (per-user install). For a system-wide or packaged
install, override the prefix: `just prefix=/usr/local install` or
`just rootdir=$PKGDIR prefix=/usr install`.

After installing, add the applet via **COSMIC Settings → Desktop → Panel →
Configure panel applets**.

## Flatpak

The repository includes a developer manifest for local builds. The intended
packaged distribution channel is the **COSMIC Store**, through the
[`pop-os/cosmic-flatpak`](https://github.com/pop-os/cosmic-flatpak) repository;
until that external submission is published, build this checkout locally.

Local Flatpak builds require `flatpak-builder`,
[`uv`](https://docs.astral.sh/uv/), `appstreamcli`, and a user-scoped Flathub
remote. Add the remote once if it is not already configured:

```bash
flatpak remote-add --user --if-not-exists flathub \
  https://flathub.org/repo/flathub.flatpakrepo
```

Generate the offline Cargo sources before building. The recipes use the root
manifest `io.github.ercling.cosmic-applet-daymural.json` and a retained
`build-dir` cache:

```bash
just flatpak-sources        # generate gitignored cargo-sources.json with uv
just flatpak-prefetch       # fetch the runtime, SDK, and declared sources
just flatpak-build-offline  # force-clean build with downloads disabled
just flatpak-install        # build and install for the current user
just flatpak-uninstall
```

`just flatpak-build` is also available when an online build is preferable.
`appstreamcli validate --pedantic --explain --strict --no-net --override cid-contains-uppercase-letter=error
data/io.github.ercling.cosmic-applet-daymural.metainfo.xml` validates the
AppStream metadata without probing the not-yet-published project homepage.
Run the network-enabled form before Store submission, once that homepage is
public. The developer metadata currently has no screenshot; adding and hosting
one remains part of the external publication work.

Repository acceptance on 2026-08-21 passed all 374 native checks, regenerated
Cargo sources covering all 713 sourced `Cargo.lock` packages, prefetched every
manifest input, and completed a force-clean Flatpak build with downloads
disabled. The resulting build contains the desktop entry, AppStream metadata,
symbolic icon, executable `/app/bin/daymural`, and the documented
scoped permissions. `flatpak build-export` preserves the desktop entry's bare
`Exec=daymural`; that name matches both the manifest command and
the `/app/bin` executable. Interactive COSMIC-session checks were not performed
by this automated acceptance pass and remain release checks before external
Store publication.

The sandbox permissions are deliberately narrow:

- Wayland and DRI for the libcosmic UI, and network access for Bing.
- `~/Pictures/BingWallpaper` for downloaded wallpapers, plus
  `~/.config/cosmic` for applet, cosmic-bg, and theme settings.
- `~/.local/state/cosmic` for cosmic-bg's lock-screen state update.
- the COSMIC Settings Daemon names on the session bus and logind on the system
  bus for live theme/config notifications and lock/resume events.

No home-wide or host-wide filesystem grant, unrestricted D-Bus socket, X11
socket, or persistent sandbox-home path is requested. Flatpak itself may show
additional read-only access to GTK 3/4 settings, `kdeglobals`, and color
schemes in `flatpak info --show-permissions`; those are runtime-added rather
than manifest permissions.

Wallpaper files and COSMIC configuration are intentionally shared with a
native installation. The catalogue, thumbnail cache, leadership lock, and
other applet state stay private under
`~/.var/app/io.github.ercling.cosmic-applet-daymural/.local/state/`. On the first
Flatpak start, the applet rescans the shared `~/Pictures/BingWallpaper` folder,
so existing images reappear without being downloaded again even though the
native catalogue is not copied.

Use Flatpak 1.13 or newer in a standard COSMIC session. It must provide the
private `XDG_STATE_HOME` layout above. Inside the sandbox the applet is in a
PID namespace (observed as PID 2), so logind cannot resolve the session from
its process ID; it relies on COSMIC's forwarded `XDG_SESSION_ID` fallback for
lock and resume monitoring. The Freedesktop 25.08 runtime does not contain the
six themed icons used by the popup, so they resolve through Flatpak's standard
host icon passthrough on `XDG_DATA_DIRS`.

**Do not keep native and Flatpak installs active at the same time.** Their
leadership locks live in separate private state directories while both can
write the shared COSMIC configuration, so two instances can race. Run
`just uninstall` before installing or testing the Flatpak; run
`just flatpak-uninstall` before returning to `just install`.

## Settings

Everything lives in the panel popup; there is no separate settings window.

| Setting | Values | Default | Notes |
|---|---|---|---|
| Shuffle | on / off | off | Rotates the wallpaper among downloaded images. Manual browsing resets the countdown. |
| Shuffle interval | 30 min / 1 h / 6 h / daily | daily | Shown only while shuffle is on. |
| Match accent to wallpaper | on / off | off | Recomputes the system accent from each applied wallpaper (a grey wallpaper falls back to the palette's warm grey). Off restores the accent from enable time; changing the accent in COSMIC Settings turns the toggle off and leaves your choice alone. |
| Keep images | 3 / 8 / 30 days / forever | 8 days | Older images are deleted after each fetch and immediately when you reduce the setting. The currently applied image is never deleted. |

Settings persist via `cosmic-config` under the app ID
`io.github.ercling.cosmic-applet-daymural`. Images live in
`~/Pictures/BingWallpaper`. With a native install, the catalogue and cached
thumbnails live in `~/.local/state/io.github.ercling.cosmic-applet-daymural/`;
with Flatpak they live under
`$XDG_STATE_HOME/io.github.ercling.cosmic-applet-daymural/` (normally
`~/.var/app/io.github.ercling.cosmic-applet-daymural/.local/state/io.github.ercling.cosmic-applet-daymural/`).

## Disk usage

Bing's UHD images are roughly **5 MB each**. Expect about:

- **≈ 40 MB** at the default 8-day retention
- **≈ 150 MB** at 30-day retention
- unbounded growth (~150 MB/month) with "forever"

## Limitations (v1)

- **External wallpaper changes are not watched live.** The applet re-reads
  cosmic-bg's config at startup, around every fetch, and when it prunes — not
  the instant you change the wallpaper in COSMIC Settings. Between those reads
  its idea of "current" can be stale, but it still correctly refrains from
  clobbering your choice on background refreshes.
- **Accent changes in COSMIC Settings are noticed at the next recompute, not
  instantly.** With *Match accent to wallpaper* on, picking an accent yourself
  turns the toggle off — but only when the applet next derives an accent: on
  the next wallpaper apply (browse, shuffle, refresh) or at applet startup.
  Until then your pick simply stands; nothing overwrites it in between.
- **Per-output backgrounds collapse to same-on-all.** Applying a wallpaper sets
  `same-on-all = true` and writes the shared `all` entry — a per-display
  background setup is intentionally replaced on first apply.
- **Timers only run while the panel runs.** There is no daemon; a machine that
  is off at refresh time catches up at next login (same trade-off as the GNOME
  extension). Timers also don't advance during suspend, so a refresh that came
  due while the machine slept fires late after resume rather than immediately.
- **Multi-monitor ownership transfer is not instantaneous.** If the output
  hosting the active applet disappears and COSMIC stops that applet process, a
  surviving copy takes over shared work after its next leadership check,
  normally within about 60 seconds. If COSMIC keeps the old process alive, it
  remains the active owner.
- **The lock screen may show its own default for a few seconds before the
  wallpaper appears — an upstream cosmic-greeter bug the applet works around.**
  The applet writes nothing lock-screen-specific: it writes cosmic-bg's config,
  cosmic-bg records the applied path in its state file, and cosmic-greeter reads
  that. Our side of the chain is verified live (config write → cosmic-bg's
  inotify watch → state file rewritten 42 ms later with the applied path → the
  locker's own watch on that state file). What breaks is cosmic-greeter 1.5.0's
  own image cache: every *delivered* cosmic-bg state update makes the locker
  clear `surface_images` and rebuild it (`src/locker.rs:1023-1027`), but the
  rebuild silently skips any surface whose id is no longer in `surface_names`
  (`src/common.rs:148-150`) — and unlocking removed exactly those ids
  (`src/locker.rs:1004`, `1133`). Locking re-inserts the names
  (`src/locker.rs:968`) without rebuilding, so from the second lock onward
  `view_window` falls back to the bundled `res/background.jpg`
  (`src/locker.rs:1167-1172`). And cosmic-bg's own state churn cannot heal a
  lock that is already up: it does rewrite the state every
  `rotation_frequency` seconds even for a single-file source
  (`cosmic-bg/src/wallpaper.rs:320-352`), but with one file the value never
  changes, and cosmic-config's read side only delivers *changed* values — an
  identical rewrite produces no update, so a lock stays on the default
  indefinitely unless a genuine wallpaper change (daily auto-apply, shuffle)
  happens to land mid-lock. The workaround: the applet watches the system
  D-Bus for the session's `Lock` signal and the resume edge of
  `PrepareForSleep`, then rewrites cosmic-bg's state with a semantically
  identical but value-different `wallpapers` list (once ~1 s after the event,
  again at ~4 s as a safety net) — a change the locker does deliver and
  rebuild from. So the lock screen may show the default background for roughly
  one to four seconds before healing to the real wallpaper. Upstream needs one
  `update_wallpapers` call on lock, or to stop dropping the names on unlock:
  [pop-os/cosmic-greeter#511](https://github.com/pop-os/cosmic-greeter/issues/511),
  filed from this trace (same symptom as its #460 / #497, which had no
  reproduction); once that ships, the applet's poke becomes a harmless extra
  rebuild.
- **The login screen needs cosmic-greeter to *be* your display manager.** It
  shows your wallpaper only under greetd + `cosmic-greeter.service` with
  `cosmic-greeter-daemon.service` running: that daemon reads your cosmic-bg
  state and the image bytes — as you, via `seteuid`, so a standard `drwxr-x---`
  home is no obstacle — and hands the bytes to the unprivileged greeter over
  D-Bus. Under any other display manager (GDM, SDDM, …) the login screen keeps
  its own background and nothing the applet does can reach it.
- **Clicking outside an open dropdown menu can still kill the applet**
  (`xdg_popup was destroyed while it was not the topmost popup`; the panel then
  restarts it, so nothing is lost but the open popup). The applet keeps at most
  one popup open under its own popup and orders every destroy it emits, so it
  never gets this wrong itself — but the dismissal path is entirely inside
  libcosmic. When the compositor dismisses a popup chain, the runtime collects
  the dismissed popup *and its ancestors* and then destroys them in the wrong
  direction, ancestor first
  (`iced/winit/src/platform_specific/wayland/handlers/shell/xdg_popup.rs`'s
  `done`, which is missing the `to_destroy.reverse()` its sibling code path
  in `event_loop/state.rs` has). Since a dropdown menu and the applet popup are
  both in the same grab chain, the applet popup gets destroyed while the menu
  is still mapped, which is the protocol violation. So it is not rare — it is
  close to deterministic for that one interaction — and no tooltip needs to be
  involved. Dismissing a menu by **picking an entry** is unaffected: that click
  lands inside the menu, the widget asks for the menu's own destroy, and the
  runtime's other code path handles it correctly. Clicking the **dropdown button
  again** is *not* unaffected — it is a click outside a grabbing popup, so the
  compositor dismisses the chain exactly as a click anywhere else does, and it
  can take the applet down the same way. Fixing it needs a libcosmic change.
- Market is auto-detected, resolution is fixed at UHD, and the download folder
  is fixed at `~/Pictures/BingWallpaper`.

## Translations

Strings live in Fluent catalogues under `i18n/<locale>/daymural.ftl`,
embedded into the binary at build time — there are no runtime data files to
install. All 73 locales COSMIC ships are present, plus `Comment[…]` lines in the
desktop entry for the dozen largest.

**Only `en` is human-written.** The other 72 catalogues (and the desktop-entry
comments) are machine-generated; a handful were spot-checked, the rest were not.
Native-speaker corrections are the most useful contribution this project can
get — edit your locale's `.ftl` and open a PR. Two rules keep the guard tests
green: keep every message id from `i18n/en/…`, and keep each message's `{ $time }`
/ `{ $date }` placeables (reordering them within the sentence is fine and
expected). Dates themselves are formatted as English month abbreviations
("Aug 5") in every locale — only the sentence frame around them is translated.

The language is taken from the desktop's `LANG`/`LC_MESSAGES` once at startup;
there is no in-app language setting. **To get the English UI back** — because
your locale's machine translation reads poorly, or to report what it says —
launch the applet with `LC_MESSAGES=C`, e.g. by adding `Env=LC_MESSAGES=C` to
the panel's launcher or running
`LC_MESSAGES=C daymural` from a terminal. Corrections are welcome
either way.

## Development

```bash
just check              # fmt --check + clippy --all-targets -D warnings + test
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Note: on machines where linuxbrew's `pkg-config` shadows the system one, cargo
needs `PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig` — the
`justfile` exports this automatically, so prefer `just check` / `just build`.

## License

[GPL-3.0-only](LICENSE), like the GNOME extension that inspired it. The license
covers this project's code and assets only — not the photographs the applet
downloads from Bing; see [Image rights and attribution](#image-rights-and-attribution).
