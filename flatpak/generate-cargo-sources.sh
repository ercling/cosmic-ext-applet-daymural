#!/usr/bin/env sh
# Regenerate cargo-sources.json (gitignored) from Cargo.lock for the Flatpak
# manifest's offline build. Run from anywhere in the checkout.
#
# Deliberately no python3 fallback: the vendored generator relies on its
# PEP-723 dependencies, which uv resolves reproducibly.
set -eu

cd "$(dirname "$0")/.."

if ! command -v uv >/dev/null 2>&1; then
    echo "error: 'uv' not found; install it from https://docs.astral.sh/uv/ or your package manager, then re-run" >&2
    exit 1
fi

exec uv run --locked --script flatpak/flatpak-cargo-generator.py Cargo.lock -o cargo-sources.json
