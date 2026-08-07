# COSMIC Bing Wallpaper Applet

A COSMIC panel applet that fetches Bing's image of the day and applies it as your
desktop wallpaper via `cosmic-bg`. Browse previously downloaded images from the panel
popup, optionally shuffle among them on a timer, and control how long images are kept.

> Work in progress — full documentation (screenshot, settings, limitations) lands with
> the final release polish.

## Build & install

Requires Rust (edition 2024, rustc ≥ 1.85) and [`just`](https://github.com/casey/just).

```bash
just build              # release build
just install            # install to ~/.local (binary, .desktop, icon)
just uninstall
```

After installing, add the applet via COSMIC Settings → Desktop → Panel.

## Development

```bash
cargo test
cargo clippy
cargo fmt
```
