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

## Upgrading to the cosmic-ext identity

The public App ID is now `io.github.ercling.cosmic-ext-applet-daymural`;
Daymural's display name and `daymural` executable are unchanged. The internal
storage ID remains `io.github.ercling.cosmic-applet-daymural`. Native settings,
history, thumbnail cache and accent recovery records remain in place.
This upgrade preserves data; the separate historical rename below does not.

### Native upgrade

1. Remove the old Daymural panel entry on every output and stop its processes
   before changing installation files. Back up the shared configuration and
   native state directories listed under Data locations, and keep the backups.
2. Before installing the replacement, remove only the old desktop entry and icon
   (adjust the prefix if your previous installation was not under `~/.local`):

   ```bash
   rm -f "$HOME/.local/share/applications/io.github.ercling.cosmic-applet-daymural.desktop" \
     "$HOME/.local/share/icons/hicolor/scalable/apps/io.github.ercling.cosmic-applet-daymural-symbolic.svg"
   ```

3. From the new checkout, run `just install`. The new `just uninstall` does not
   remove old-ID assets. Do not remove the unchanged `daymural` binary after
   installing its replacement. Native data needs no transfer.
4. Restart `cosmic-panel` or log out and back in, then re-add **Daymural** through
   **COSMIC Settings → Desktop → Panel → Configure panel applets** on each output.

### Flatpak upgrade

Perform these steps on the host before the new applet's first launch. Keep all
applet instances stopped during the state transfer. The build and installation
commands in step 5 may need network access to download build inputs.
Do not run old and new instances together: their private state roots have
separate leadership locks even though the storage suffix is unchanged.

1. Stop all old and new Daymural instances. Remove their panel entries on every
   output so the panel cannot restart them during the transfer. If necessary,
   stop running sandboxes with `flatpak kill io.github.ercling.cosmic-applet-daymural`
   and `flatpak kill io.github.ercling.cosmic-ext-applet-daymural`.
2. Back up the old private state and shared configuration before making changes.
   Also back up any existing new private state. Keep the backups outside both
   sandbox roots. Shared COSMIC settings, accent snapshots/last-written records
   and the coordination mailbox need no transfer. Never copy sandbox-local
   configuration over host settings or accent records.
3. Inspect these exact default private state roots (only the outer App ID changes):

   Old: `~/.var/app/io.github.ercling.cosmic-applet-daymural/.local/state/io.github.ercling.cosmic-applet-daymural/`

   New: `~/.var/app/io.github.ercling.cosmic-ext-applet-daymural/.local/state/io.github.ercling.cosmic-applet-daymural/`

   Respect effective XDG overrides: determine the old and new applet's actual
   `XDG_STATE_HOME` roots if customized, then append the retained storage ID.
   Shared configuration follows `HOST_XDG_CONFIG_HOME` in Flatpak, falling back
   to `$HOME/.config`; do not substitute the sandbox's private `config/` tree.
4. Transfer only `catalogue.json` and `thumbs/` from old private state to new
   private state, using a copy that preserves the old data. Include every
   thumbnail sidecar and failed-decode record within `thumbs/`. Create the new
   state directory if absent. Stop on any destination conflict; never overwrite
   or merge existing destination entries. Do not copy `config/` or lock files.
   If either source entry is absent, leave that entry absent at the destination.
   Stop on a failed transfer; do not launch against partially transferred state.
   Verify both copied entries against their sources before proceeding. Resolve
   a conflict or failure manually using the backups while all instances remain
   stopped; do not treat a partially populated destination as a completed upgrade.
5. Keep the old data and backups after the upgrade. Uninstall the old identity
   with `flatpak uninstall --user io.github.ercling.cosmic-applet-daymural`
   without `--delete-data`. From the new checkout, run `just flatpak-sources`
   and `just flatpak-install`; these install the new public identity.
6. Launch only after the transfer is complete. Restart `cosmic-panel` or log out
   and back in, then re-add **Daymural** in **COSMIC Settings → Desktop → Panel →
   Configure panel applets**. Verify settings, history, the current wallpaper,
   and accent disable/restore before discarding any installation files.

### Rollback

Stop the new instances and remove their panel entries first. For native rollback,
run the new checkout's `just uninstall`, then reinstall the previous revision
with its `just install` and re-add its panel entry. For Flatpak rollback,
uninstall the new identity without `--delete-data`, reinstall the old build,
and re-add its panel entry. Keep the old data and backups; the old Flatpak
private state remains available. Shared configuration may have changed since
upgrade: restore its backup only deliberately, with all instances stopped and
with awareness that accent recovery records must match the theme being restored.
Keep `~/Pictures/BingWallpaper` and the applied wallpaper unchanged. Do not
restore or copy leadership lock files. Never run both revisions concurrently.

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

The retained storage ID keeps native state and shared native/Flatpak COSMIC
configuration at these default locations:

```text
~/.config/cosmic/io.github.ercling.cosmic-applet-daymural/
~/.local/state/io.github.ercling.cosmic-applet-daymural/
```

The new Flatpak keeps the catalogue, thumbnails, and other private state below:

```text
~/.var/app/io.github.ercling.cosmic-ext-applet-daymural/.local/state/io.github.ercling.cosmic-applet-daymural/
```

Native paths respect `XDG_CONFIG_HOME` and `XDG_STATE_HOME`; Flatpak shared
configuration respects `HOST_XDG_CONFIG_HOME`, falling back to `$HOME/.config`.

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
