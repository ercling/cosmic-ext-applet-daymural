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
- **Respects your choices** — if you set a different wallpaper in COSMIC
  Settings, the applet keeps downloading but stops auto-applying until you act
  (prev/next/newest/shuffle).

## Build & install

Requires Rust (edition 2024, rustc ≥ 1.85) and [`just`](https://github.com/casey/just).

```bash
just build              # release build
just install            # install to ~/.local (binary, .desktop, icon)
just uninstall
```

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

- **External wallpaper changes are not watched.** If you change the wallpaper in
  COSMIC Settings while the applet runs, the applet's idea of "current" only
  refreshes on its next apply or on restart. (It does correctly refrain from
  clobbering your choice on background refreshes.)
- **Per-output backgrounds collapse to same-on-all.** Applying a wallpaper sets
  `same-on-all = true` and writes the shared `all` entry — a per-display
  background setup is intentionally replaced on first apply.
- **Timers only run while the panel runs.** There is no daemon; a machine that
  is off at refresh time catches up at next login (same trade-off as the GNOME
  extension).
- Market is auto-detected, resolution is fixed at UHD, and the download folder
  is fixed at `~/Pictures/BingWallpaper`.

## Development

```bash
just check              # fmt --check + clippy -D warnings + test
cargo test
cargo clippy
cargo fmt
```

Note: on machines where linuxbrew's `pkg-config` shadows the system one, cargo
needs `PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig` — the
`justfile` exports this automatically, so prefer `just check` / `just build`.
