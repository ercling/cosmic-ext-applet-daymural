# Contributing to Daymural

Bug fixes, accessibility improvements, translations, documentation, and
focused feature contributions are welcome.

## Development setup

Daymural is a Rust 2024/libcosmic panel applet, currently developed and tested
with Rust 1.97.1. Install Rust, `just`, `pkg-config`, and the development
libraries required by libcosmic. On Debian, Ubuntu, and Pop!_OS:

```bash
sudo apt install cargo just pkg-config libxkbcommon-dev libwayland-dev
```

On Fedora:

```bash
sudo dnf install cargo just pkgconf-pkg-config libxkbcommon-devel wayland-devel
```

Build and verify the project with:

```bash
just build
just check
```

`just check` runs `cargo fmt --check`, Clippy for all targets with warnings
denied, and the complete test suite. Run it before opening a pull request.

The `justfile` exports the `PKG_CONFIG_PATH` needed on machines where
linuxbrew's `pkg-config` hides the system xkbcommon metadata. If you run Cargo
directly, set it yourself:

```bash
export PKG_CONFIG_PATH=/usr/lib64/pkgconfig:/usr/share/pkgconfig
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

## Testing rules

Add or update hermetic tests for every behavior change, except iced view
construction. Tests must use injected paths or `Config::with_custom_path`
inside a `tempfile::TempDir`. They must never contact Bing or modify real user
configuration, state, themes, wallpapers, or files outside the temporary
directory.

Keep network, download, image decoding, filesystem, and theme work off the UI
thread. Async completions and timer ticks must be checked for stale state.

Do not casually run the applet binary. A cold standalone start can contact Bing
and apply a wallpaper. Intentional runtime diagnosis should use
`RUST_LOG=daymural=debug` in a COSMIC session.

## Translations

The English source catalogue is `i18n/en/daymural.ftl`. The other catalogues
cover the 73 locales shipped by COSMIC and are currently machine-generated, so
native-speaker corrections are particularly valuable.

When adding or renaming a Fluent message:

- update all locale catalogues;
- preserve placeables such as `{ $time }` and `{ $date }`; and
- run `just check` to exercise the locale guards.

## Architecture and design

Start with [AGENTS.md](AGENTS.md) for the shared repository guide and subsystem
invariants. Claude Code imports that same guide through `CLAUDE.md`, while
`docs/plans/` records design evidence and completed implementation plans.
Read the relevant plan before changing popup lifecycle, accent handling,
thumbnail production, lock-screen integration, or multi-output leadership.

Packaging changes should also follow [Flatpak packaging](docs/packaging.md).

## Pull requests

Keep changes focused and preserve unrelated working-tree edits. Update the
README or focused documentation when user-visible behavior, installation,
settings, limitations, or packaging changes. In the pull request, describe the
behavioral change and the verification you performed.
