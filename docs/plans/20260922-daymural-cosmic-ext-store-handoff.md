# Daymural cosmic-ext store handoff

Companion to [the implementation plan](20260922-daymural-cosmic-ext-rename.md).
This is a handoff for external work, not a record that the repository or PR
has been published. No store checkout, PR, remote, or running applet was changed.

## Verified upstream contract

Public App ID: `io.github.ercling.cosmic-ext-applet-daymural`.
Repository/homepage: `https://github.com/ercling/cosmic-ext-applet-daymural`.
Display name, executable, Cargo package, module name, and Fluent domain remain
Daymural / `daymural`. Persistent storage remains
`io.github.ercling.cosmic-applet-daymural`. Native settings/state and shared
host COSMIC config require no migration. Follow
[the upgrade guide](../installation.md#upgrading-to-the-cosmic-ext-identity)
for the private Flatpak catalogue/thumbnail transfer and panel re-addition.

Hermetic tests verify retained settings, accent recovery fields, mailbox,
catalogue and thumbnail identity, both config factories/watchers, shared
leadership, and mixed-identity rejection. `just check` passed with 452 tests,
formatting and clippy clean. Direct comparison against the PR's upstream base
`291b967b9b96c210ec364fa5cf3bfce896696057` confirmed Cargo.lock, runtime/SDK
pins, manifest permissions/build options, and wallpaper, leader, accent, and
thumbnail implementations unchanged. `~/Pictures/BingWallpaper` is unchanged.
Attended native/Flatpak upgrade, accent restore, and multi-output acceptance
remain pending; no live user data was accessed for verification.

## Exact store changes

The existing [PR #298 diff](https://github.com/pop-os/cosmic-flatpak/pull/298/files)
was read during execution. It adds exactly these three files beneath
`app/io.github.ercling.cosmic-applet-daymural/`:

| Existing path | Replacement path |
| --- | --- |
| `app/io.github.ercling.cosmic-applet-daymural/io.github.ercling.cosmic-applet-daymural.json` | `app/io.github.ercling.cosmic-ext-applet-daymural/io.github.ercling.cosmic-ext-applet-daymural.json` |
| `app/io.github.ercling.cosmic-applet-daymural/cargo-sources.json` | `app/io.github.ercling.cosmic-ext-applet-daymural/cargo-sources.json` |
| `app/io.github.ercling.cosmic-applet-daymural/store-metadata.patch` | `app/io.github.ercling.cosmic-ext-applet-daymural/store-metadata.patch` |

In the renamed manifest set `id` to the new public App ID. Replace the three
asset install commands with the matching upstream commands:

```text
install -Dm644 data/io.github.ercling.cosmic-ext-applet-daymural.desktop /app/share/applications/io.github.ercling.cosmic-ext-applet-daymural.desktop
install -Dm644 data/io.github.ercling.cosmic-ext-applet-daymural.metainfo.xml /app/share/metainfo/io.github.ercling.cosmic-ext-applet-daymural.metainfo.xml
install -Dm644 data/icons/io.github.ercling.cosmic-ext-applet-daymural-symbolic.svg /app/share/icons/hicolor/scalable/apps/io.github.ercling.cosmic-ext-applet-daymural-symbolic.svg
```

Keep `/app/bin/daymural`, command/module `daymural`, CARGO_HOME, runtime/SDK,
finish-args and Cargo build commands unchanged. Keep source ordering: upstream
git source, `cargo-sources.json`, then `store-metadata.patch`. Change the git
source URL to `https://github.com/ercling/cosmic-ext-applet-daymural.git`.
Replace its current commit `291b967b9b96c210ec364fa5cf3bfce896696057` only after
the final reviewed upstream commit is published: obtain its full SHA from that
published revision and verify the remote resolves it. No future SHA is known
or supplied here. Local implementation commits are not a publication claim.

## Rebase the metadata patch against the published revision

Use a disposable checkout of that exact published commit. Reapply the current
patch's intended changes to `Cargo.toml`,
`data/io.github.ercling.cosmic-ext-applet-daymural.metainfo.xml`, and
`src/app.rs`, resolving moved test locations against the current source.
Regenerate `store-metadata.patch` from the resulting three-file diff; do not
blindly keep old hunk offsets or old metainfo filenames.

Preserve all existing store improvements:

- Cargo description and AppStream summary:
  `Discover daily Bing wallpapers from your COSMIC panel`.
- Expanded AppStream description: panel preview/attribution/history, automatic
  wallpapers, manual selection, shuffle, 3/8/30-day or indefinite retention,
  opt-in accent matching, respect for Settings choices, panel activation, and
  the existing Microsoft independence paragraph.
- Developer ID `io.github.ercling`, name `Daymural Developers`.
- Bugtracker and help URLs, Utility category, wallpaper/Bing/applet/background
  keywords, and the default screenshot with its existing caption and 371×585
  dimensions at `resources/screenshots/screenshot-main.png`.
- Associated tests for Cargo description, developer name, and the deliberate
  wrong-summary mutation. Preserve the rename's new storage and upgrade guards.

The metainfo ID, launchable
`io.github.ercling.cosmic-ext-applet-daymural.desktop`, homepage, bugtracker,
and patch context must use the new public identity. Set help to
`https://github.com/ercling/cosmic-ext-applet-daymural/blob/<published SHA>/docs/installation.md`
and screenshot to
`https://raw.githubusercontent.com/ercling/cosmic-ext-applet-daymural/<published SHA>/resources/screenshots/screenshot-main.png`.
`<published SHA>` is an instruction placeholder, never a shippable URL; resolve
and verify both URLs against a real published commit before updating the PR.

Retain existing `cargo-sources.json` only after verifying consistency with the
final upstream Cargo.lock and generator inputs. This rename does not change
them; generator formatting/cache differences alone are not dependency changes.
Check that the regenerated patch applies cleanly to the pinned source, run
`just check` on that patched disposable checkout, run desktop-file and strict
no-network AppStream validation, and run the store repository's required checks
and offline build before publishing the store update.

## Remaining external and attended work

Rename the GitHub repository, update the local remote, publish the final
reviewed upstream revision, and verify redirects/links. Update PR #298's
manifest and metadata patch as above, then its description's App ID, source
link and full commit SHA. A response to jackpot51 requires explicit messaging
authorization; reviewer acceptance remains pending. Do not claim legal clearance.

Stop old and new applet instances before attended upgrade testing. Back up
first, preserve shared COSMIC config and accent recovery, transfer only private
`catalogue.json` and `thumbs/`, and retain old data/backups. Confirm settings,
history, applied wallpaper, accent disable/restore, panel re-addition and
multi-output ownership. Different Flatpak roots do not share a leader lock.
