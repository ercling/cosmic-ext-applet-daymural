name := 'cosmic-bing-wallpaper'
appid := 'io.github.ercling.CosmicBingWallpaper'

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
    cargo clippy -- -D warnings
    cargo test

# Install binary, desktop entry (Exec= rewritten to the absolute binary path — cosmic-panel may not have the bindir in PATH), and icon.
install: build
    install -Dm0755 target/release/{{name}} {{bin-dst}}
    install -Dm0644 data/{{appid}}.desktop {{desktop-dst}}
    sed -i 's|^Exec=.*|Exec={{bin-dst}}|' {{desktop-dst}}
    install -Dm0644 data/icons/{{appid}}-symbolic.svg {{icon-dst}}

uninstall:
    rm -f {{bin-dst}} {{desktop-dst}} {{icon-dst}}
