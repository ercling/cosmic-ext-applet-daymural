name := 'daymural'
appid := 'io.github.ercling.cosmic-applet-daymural'

# Default to a per-user install (no sudo needed); override with e.g.
# `just prefix=/usr/local install` or `just rootdir=$PKGDIR prefix=/usr install`.
rootdir := ''
prefix := env('HOME') / '.local'

base-dir := absolute_path(clean(rootdir / prefix))
bin-dst := base-dir / 'bin' / name
desktop-dst := base-dir / 'share' / 'applications' / appid + '.desktop'
icon-dst := base-dir / 'share' / 'icons' / 'hicolor' / 'scalable' / 'apps' / appid + '-symbolic.svg'

# linuxbrew's pkg-config shadows the system one and misses /usr/lib64/pkgconfig
# (breaks the xkbcommon probe). Prepending the system dirs is harmless elsewhere.
export PKG_CONFIG_PATH := '/usr/lib64/pkgconfig:/usr/share/pkgconfig:' + env('PKG_CONFIG_PATH', '')

default: build

build:
    cargo build --release

check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

# Install binary, desktop entry (Exec= rewritten to the absolute binary path — cosmic-panel may not have the bindir in PATH), and icon.
install: build
    install -Dm0755 target/release/{{name}} {{bin-dst}}
    install -Dm0644 data/{{appid}}.desktop {{desktop-dst}}
    sed -i 's|^Exec=.*|Exec={{bin-dst}}|' {{desktop-dst}}
    install -Dm0644 data/icons/{{appid}}-symbolic.svg {{icon-dst}}
    @echo
    @echo 'Installed. Run `pkill -x cosmic-panel` to load the new binary (cosmic-session'
    @echo 'restarts the panel and its applets right away), or log out and back in;'
    @echo 'then add "Daymural" in Settings → Desktop → Panel → Configure applets.'

uninstall:
    rm -f {{bin-dst}} {{desktop-dst}} {{icon-dst}}

# Generate the Cargo dependency sources consumed by the offline Flatpak build.
# Requires `uv`; cargo-sources.json is deliberately gitignored.
flatpak-sources:
    flatpak/generate-cargo-sources.sh

# All local Flatpak build modes share this invocation so cache, remote, and
# force-clean behavior cannot drift between recipes. Flags follow the `build`
# target of pop-os/cosmic-flatpak's justfile (ccache, delete-build-dirs,
# sandboxed build) minus its repo/GPG handling, which only applies to their
# OSTree repo publishing.
flatpak-builder-cmd := 'flatpak-builder --ccache --delete-build-dirs --force-clean --install-deps-from=flathub --sandbox --user'

# Fetch the runtime, SDK, and every manifest source into Flatpak's retained
# download cache without compiling the applet.
flatpak-prefetch:
    {{flatpak-builder-cmd}} --download-only build-dir '{{appid}}.json'

flatpak-build:
    {{flatpak-builder-cmd}} build-dir '{{appid}}.json'

# Prove the retained cache is complete: this build is forbidden from fetching.
flatpak-build-offline:
    {{flatpak-builder-cmd}} --disable-download build-dir '{{appid}}.json'

flatpak-install:
    {{flatpak-builder-cmd}} --install build-dir '{{appid}}.json'

flatpak-uninstall:
    flatpak uninstall -y --user '{{appid}}'
