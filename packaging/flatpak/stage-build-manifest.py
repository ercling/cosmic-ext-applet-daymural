#!/usr/bin/env python3
"""Stage the canonical Flatpak manifest at the repository root.

Flatpak Builder's --sandbox flag rejects directory sources outside the
manifest directory.  The canonical manifest lives in packaging/flatpak, so
local builds use this root-level generated view with root-relative sources.
"""

import json
import os
import pathlib
import sys
import tempfile


def stage(source: pathlib.Path, destination: pathlib.Path) -> None:
    manifest = json.loads(source.read_text(encoding="utf-8"))
    sources = manifest["modules"][0]["sources"]

    directory = next(item for item in sources if isinstance(item, dict) and item.get("type") == "dir")
    if directory.get("path") != "../.." or "cargo-sources.json" not in sources:
        raise ValueError("canonical manifest sources do not match the packaging layout")

    directory["path"] = "."
    sources[sources.index("cargo-sources.json")] = "packaging/flatpak/cargo-sources.json"

    destination.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(
        dir=destination.parent, prefix=f".{destination.name}.", suffix=".tmp"
    )
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            json.dump(manifest, output, indent=4)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary_name, destination)
    except BaseException:
        try:
            os.unlink(temporary_name)
        except FileNotFoundError:
            pass
        raise


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(f"usage: {sys.argv[0]} CANONICAL_MANIFEST STAGED_MANIFEST")
    stage(pathlib.Path(sys.argv[1]).resolve(), pathlib.Path(sys.argv[2]).resolve())
