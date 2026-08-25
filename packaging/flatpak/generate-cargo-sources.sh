#!/usr/bin/env sh
# Regenerate cargo-sources.json (gitignored) from Cargo.lock for the Flatpak
# manifest's offline build. Run from anywhere in the checkout.
#
# Deliberately no python3 fallback: the vendored generator relies on its
# PEP-723 dependencies, which uv resolves reproducibly.
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
cd "$repo_root"

if ! command -v uv >/dev/null 2>&1; then
    echo "error: 'uv' not found; install it from https://docs.astral.sh/uv/ or your package manager, then re-run" >&2
    exit 1
fi

exec uv run --locked --script "$script_dir/flatpak-cargo-generator.py" \
    "$repo_root/Cargo.lock" -o "$script_dir/cargo-sources.json"
