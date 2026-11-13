# Known limitations

These limitations apply to the current Daymural release and its pinned COSMIC
dependencies.

## Wallpaper and accent updates

- External wallpaper changes are not observed instantly. Daymural rereads
  cosmic-bg configuration at startup and around refresh and retention work. A
  background refresh still avoids replacing a wallpaper that it recognizes as
  user-selected.
- With **Match accent to wallpaper** enabled, a manual accent choice is noticed
  at the next accent recomputation: after a wallpaper apply, refresh, or applet
  restart. Daymural then disables matching and preserves the user's choice.
- Applying a Daymural image changes a per-output wallpaper setup to COSMIC's
  same-wallpaper-on-all-outputs mode.
- Where COSMIC keeps separate dark and light backgrounds, applying a Daymural
  image sets both modes to that image, whether you apply it yourself or a
  background refresh or shuffle does. Each mode keeps its own scaling, filter,
  and transition settings. Before a background apply Daymural checks only the
  wallpaper of the mode that is active at that moment, so a wallpaper you chose
  for the other mode alone is replaced too.
- Cleanup likewise protects only the active mode's wallpaper. A Daymural image
  that only the other mode still uses can be deleted once it is older than the
  retention period; that mode then falls back to COSMIC's default image.
- Leftover per-mode background settings mislead Daymural on a COSMIC that does
  not use them. If `~/.config/cosmic/com.system76.CosmicBackground.Dark` or
  `.Light` exists while the installed cosmic-bg has no separate dark and light
  backgrounds (after a downgrade, or because another tool created them),
  Daymural reads the displayed wallpaper from those directories instead of the
  one COSMIC shows. A background refresh can then replace a wallpaper you chose
  in COSMIC Settings. Delete both directories to fix it.

## Timers and multiple outputs

- Refresh and shuffle timers exist only while `cosmic-panel` is running. Work
  missed while logged out is caught up at the next start. A timer that becomes
  due during suspend can run late after resume.
- COSMIC can run one applet process per panel output. Daymural elects one leader
  for shared work. If the leader exits, takeover by a surviving process normally
  happens within about 60 seconds.

## Lock and login screens

- The lock screen can briefly show COSMIC's default image before changing to
  the current wallpaper. Daymural works around an upstream cosmic-greeter cache
  problem by prompting a rebuild after lock and resume; healing normally takes
  roughly one to four seconds. See
  [cosmic-greeter#511](https://github.com/pop-os/cosmic-greeter/issues/511).
- The login screen follows the wallpaper only when `cosmic-greeter` is the
  active display manager with its daemon running. GDM, SDDM, and other display
  managers control their own login backgrounds.

## Popup dismissal

Clicking outside an open dropdown can terminate the applet with an
`xdg_popup was destroyed while it was not the topmost popup` protocol error.
The panel restarts the applet and no data is lost. Selecting a dropdown entry
is unaffected. This is an ancestor-first popup destruction bug in the pinned
libcosmic stack; the investigation is recorded in
[the popup destroy-order plan](plans/completed/20260810-popup-destroy-order-crash.md).

## Fixed choices

Bing market selection is automatic, downloads use UHD resolution, and the
image directory is fixed at `~/Pictures/BingWallpaper`.

## Troubleshooting

An empty applet can mean that Bing did not mark any current image as eligible
for wallpaper download. Daymural fails closed rather than downloading an image
without that permission signal. Run with `RUST_LOG=daymural=warn` to see refresh
and eligibility warnings, or `RUST_LOG=daymural=debug` for intentional runtime
diagnosis.
