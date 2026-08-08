# COSMIC Bing Wallpaper Applet

A COSMIC panel applet that brings Bing's image of the day to the COSMIC desktop.
It fetches Bing's daily wallpaper (UHD, 3840×2160), applies it via `cosmic-bg`,
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
- **Shuffle** — rotate among downloaded images every 30 min / 1 h / 6 h / daily.
- **Retention** — keep 3 / 8 / 30 days of images, or forever.
- **Image details** — click the thumbnail to open the full-size image in your
  default viewer, or use "About this image" to open Bing's info page in the
  browser (both launched via `xdg-open`, so `xdg-utils` is needed at runtime).
- **Respects your choices** — if you set a different wallpaper in COSMIC
  Settings, the applet keeps downloading but stops auto-applying until you act
  (prev/next/newest/shuffle).
- **Tooltips** — every icon-only control (panel button, prev/next/newest/refresh,
  thumbnail) names what it does on hover; unavailable buttons are visibly dimmed.
- **Localized** — the UI follows your desktop language, with catalogues for the
  73 locales COSMIC itself ships (see [Translations](#translations)).

## Build & install

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
catalogue/thumbnails (`~/.local/state/io.github.ercling.CosmicBingWallpaper/`),
and settings (`~/.config/cosmic/io.github.ercling.CosmicBingWallpaper/`) are
left behind — delete them by hand if you want a clean sweep.

`just install` needs no sudo (per-user install). For a system-wide or packaged
install, override the prefix: `just prefix=/usr/local install` or
`just rootdir=$PKGDIR prefix=/usr install`.

After installing, add the applet via **COSMIC Settings → Desktop → Panel →
Configure panel applets**.

## Settings

Everything lives in the panel popup; there is no separate settings window.

| Setting | Values | Default | Notes |
|---|---|---|---|
| Shuffle | on / off | off | Rotates the wallpaper among downloaded images. Manual browsing resets the countdown. |
| Shuffle interval | 30 min / 1 h / 6 h / daily | daily | Shown only while shuffle is on. |
| Keep images | 3 / 8 / 30 days / forever | 8 days | Older images are deleted after each fetch and immediately when you reduce the setting. The currently applied image is never deleted. |

Settings persist via `cosmic-config` under the app ID
`io.github.ercling.CosmicBingWallpaper`. Images live in
`~/Pictures/BingWallpaper`; the catalogue and cached thumbnails in
`~/.local/state/io.github.ercling.CosmicBingWallpaper/`.

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
- **Per-output backgrounds collapse to same-on-all.** Applying a wallpaper sets
  `same-on-all = true` and writes the shared `all` entry — a per-display
  background setup is intentionally replaced on first apply.
- **Timers only run while the panel runs.** There is no daemon; a machine that
  is off at refresh time catches up at next login (same trade-off as the GNOME
  extension). Timers also don't advance during suspend, so a refresh that came
  due while the machine slept fires late after resume rather than immediately.
- **The login screen keeps its own background.** The *lock* screen follows along
  (it runs as you, watches cosmic-bg's state file, and picks up an applet-applied
  wallpaper within milliseconds — verified live). The *login* greeter runs as the
  unprivileged `cosmic-greeter` user, which cannot traverse a standard
  `drwxr-x---` home directory to read `~/Pictures/BingWallpaper/*.jpg`, so it
  falls back to its own default. Nothing the applet can fix — it is the same gap
  as cosmic-greeter's "TODO: fallback to background config if background state is
  not set". Loosening the permissions on your home directory would work around
  it; that trade is yours to make, not ours to ship.
- Market is auto-detected, resolution is fixed at UHD, and the download folder
  is fixed at `~/Pictures/BingWallpaper`.

## Translations

Strings live in Fluent catalogues under `i18n/<locale>/cosmic_bing_wallpaper.ftl`,
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

[GPL-3.0-only](LICENSE), like the GNOME extension that inspired it.
