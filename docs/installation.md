# Installation

Daymural currently supports a per-user native installation and a local
developer Flatpak build from a source checkout. Publication through the COSMIC
Store is planned but not yet available. The Flatpak build is the recommended
installation method.

## Recommended: local Flatpak installation

You need `flatpak-builder`, [`uv`](https://docs.astral.sh/uv/), `appstreamcli`,
and a user-scoped Flathub remote. Package names vary by distribution. Add the
remote once if necessary:

```bash
flatpak remote-add --user --if-not-exists flathub \
  https://flathub.org/repo/flathub.flatpakrepo
```

From the source checkout, generate the Cargo sources and install the Flatpak
for the current user:

```bash
just flatpak-sources
just flatpak-install
```

For a reproducible offline verification build, prefetch all declared inputs
first:

```bash
just flatpak-prefetch
just flatpak-build-offline
```

An online `just flatpak-build` recipe is also available. Remove the Flatpak
with:

```bash
just flatpak-uninstall
```

Use Flatpak 1.13 or newer in a standard COSMIC session. More details about the
manifest and sandbox are in [Flatpak packaging](packaging.md).

## Alternative: native installation

Install Rust, [`just`](https://github.com/casey/just), `pkg-config`, and the
development libraries required by libcosmic. Package names vary by
distribution. On Debian, Ubuntu, and Pop!_OS:

```bash
sudo apt install cargo just pkg-config libxkbcommon-dev libwayland-dev
```

On Fedora:

```bash
sudo dnf install cargo just pkgconf-pkg-config libxkbcommon-devel wayland-devel
```

Build and install Daymural for the current user:

```bash
just install
```

The default prefix is `~/.local`, and `sudo` is not required. To use another
prefix or create a package staging tree:

```bash
just prefix=/usr/local install
just rootdir="$PKGDIR" prefix=/usr install
```

Restart `cosmic-panel` or log out and back in. Then add **Daymural** through
**COSMIC Settings → Desktop → Panel → Configure panel applets**.

Uninstall files placed by the native recipe with:

```bash
just uninstall
```

This removes only the executable, desktop entry, and icon. It deliberately
leaves downloaded images, settings, the catalogue, and thumbnails in place.

## Upgrading from the previous applet identity

Early development builds used the native binary `cosmic-bing-wallpaper` and
Flatpak ID `io.github.ercling.CosmicBingWallpaper`. Remove both old installation
types before starting Daymural. The identities use separate leadership locks,
so an old process and Daymural can otherwise both manage
`~/Pictures/BingWallpaper`.

Remove a previous per-user native installation:

```bash
rm -f "$HOME/.local/bin/cosmic-bing-wallpaper" \
  "$HOME/.local/share/applications/io.github.ercling.CosmicBingWallpaper.desktop" \
  "$HOME/.local/share/icons/hicolor/scalable/apps/io.github.ercling.CosmicBingWallpaper-symbolic.svg"
```

Remove the previous Flatpak identity if it was installed:

```bash
flatpak uninstall --user io.github.ercling.CosmicBingWallpaper
```

The old panel entry is not migrated. Remove it, then add Daymural through
**COSMIC Settings → Desktop → Panel** after installation. Old settings,
catalogue state, thumbnails, leadership locks, and the coordination mailbox are
not migrated. The `~/Pictures/BingWallpaper` image folder is kept and rescanned;
do not delete it as part of the identity change.

## Do not run both installations

Native and Flatpak applets use separate leadership locks but share the same
wallpaper directory and COSMIC configuration. If both are active, each can
believe it owns background work and they can race while applying wallpapers or
changing settings.

Run `just uninstall` before testing the Flatpak. Run `just flatpak-uninstall`
before returning to the native installation.

## Data locations

Downloaded images are shared by both installation types:

```text
~/Pictures/BingWallpaper
```

Native settings and state use:

```text
~/.config/cosmic/io.github.ercling.cosmic-applet-daymural/
~/.local/state/io.github.ercling.cosmic-applet-daymural/
```

Flatpak keeps the catalogue, thumbnails, and other private state below:

```text
~/.var/app/io.github.ercling.cosmic-applet-daymural/.local/state/io.github.ercling.cosmic-applet-daymural/
```

It still uses the shared image directory and COSMIC configuration. On first
start, it rescans existing JPEGs, so changing installation type does not require
downloading them again.

## Removing all data

Before uninstalling, switch off **Match accent to wallpaper** if you want
Daymural to restore the accent saved when the setting was enabled. The derived
accent is part of the system theme and can outlive the applet; after deleting
Daymural's settings, restoration requires choosing an accent manually in
COSMIC Settings.

After uninstalling, remove the image, settings, or state directories manually
only if you no longer want their contents.
