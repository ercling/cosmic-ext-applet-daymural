# Flatpak packaging

This document covers the repository's developer Flatpak and the contract to
preserve when preparing a COSMIC Store submission. Normal users should follow
[Installation](installation.md).

The authoritative manifest is:

```text
packaging/flatpak/io.github.ercling.cosmic-ext-applet-daymural.json
```

It targets Freedesktop 25.08 with the Rust stable SDK extension. The intended
distribution channel is the COSMIC Store through
[`pop-os/cosmic-flatpak`](https://github.com/pop-os/cosmic-flatpak); that
external submission has not yet been published.

## Prerequisites

Local builds require `flatpak-builder`,
[`uv`](https://docs.astral.sh/uv/), `appstreamcli`, and a user-scoped Flathub
remote:

```bash
flatpak remote-add --user --if-not-exists flathub \
  https://flathub.org/repo/flathub.flatpakrepo
```

## Build workflow

Generate the Cargo source manifest beside the developer manifest:

```bash
just flatpak-sources
```

The generated `packaging/flatpak/cargo-sources.json` is intentionally ignored
by Git. It must cover every registry and Git source in the committed
`Cargo.lock`.

Prefetch all declared inputs and prove the retained cache is complete with an
offline build:

```bash
just flatpak-prefetch
just flatpak-build-offline
```

Other available recipes are:

```bash
just flatpak-build       # online build
just flatpak-install     # build and install for the current user
just flatpak-uninstall
```

The recipes stage an ignored, root-relative manifest view and run Flatpak
Builder with `--sandbox`. Keep the shared recipe in the `justfile` as the single
source of builder flags.

Validate AppStream metadata without network access:

```bash
appstreamcli validate --pedantic --explain --strict --no-net \
  --override cid-contains-uppercase-letter=error \
  data/io.github.ercling.cosmic-ext-applet-daymural.metainfo.xml
```

Run the network-enabled validation before Store submission after the project
homepage and hosted screenshot are public.

## Identity contract

These values must remain aligned and are cross-checked by tests in `src/app.rs`:

- application ID `io.github.ercling.cosmic-ext-applet-daymural`;
- executable and manifest command `daymural`;
- desktop entry and AppStream metadata names;
- exported symbolic icon; and
- Flatpak destination `/app/bin/daymural`.

The source desktop entry uses the bare `Exec=daymural`, which Flatpak resolves
under `/app/bin`. Native `just install` rewrites only its installed copy to the
selected absolute prefix.

Keep the committed `Cargo.lock` and dependency pins intact. In particular,
libcosmic deliberately uses the bare Git repository URL so its source ID
matches the transitive `cosmic-config` source. Adding a `?rev=` source ID splits
the repository during offline vendoring.

## Sandbox contract

The manifest grants only the host integrations required by existing behavior:

- Wayland and DRI for the libcosmic interface;
- network access for Bing metadata and image downloads;
- `~/Pictures/BingWallpaper` for shared wallpaper files;
- `xdg-config/cosmic` for applet, cosmic-bg, and theme configuration;
- `~/.local/state/cosmic:create` for cosmic-bg's lock-screen state update;
- COSMIC Settings Daemon session-bus names for live configuration updates; and
- logind on the system bus for lock and resume events.

Do not broaden this to home-wide or host-wide filesystem access, unrestricted
D-Bus sockets, X11, or persistent sandbox-home paths. The state grant must use
`:create`: the lock-screen integration needs the directory to be created when
it does not exist on the host.

Flatpak can report additional read-only GTK, KDE, and colour-scheme paths that
the runtime adds itself. Those are not manifest permissions.

Wallpaper images and COSMIC configuration are intentionally shared with a
native installation. Catalogue, thumbnail, and leadership state remain private
under Flatpak's `XDG_STATE_HOME`. Consequently, native and Flatpak applets must
not be active simultaneously; see [Installation](installation.md).

## Release verification

Before handing off packaging changes:

1. Run `just check`.
2. Regenerate and inspect Cargo sources.
3. Prefetch every manifest input.
4. Complete a force-clean build with downloads disabled.
5. Inspect the exported desktop entry, metadata, icon, and executable.
6. Perform an intentional COSMIC-session test of refresh, wallpaper apply,
   history, external links, accent matching, and lock/resume behavior.

CI and local format validators cover declarative syntax. The hermetic Rust tests
cover deliberate identity, command, path, and permission drift. Detailed design
evidence remains in
[the Flatpak distribution plan](plans/completed/20260820-flatpak-distribution.md) and
[the completed packaging plan](plans/completed/20260825-centralize-flatpak-packaging.md).
