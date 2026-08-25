"""Safe directory selection for cached Git manifest discovery."""

import os
from collections.abc import Iterator


def child_manifest_directories(root_dir: str) -> Iterator[str]:
    """Yield real child directories that belong to the checked-out worktree."""
    for child in os.scandir(root_dir):
        # Git metadata is not part of the verified checkout, and directory
        # symlinks can point outside it. Neither may supply Cargo metadata.
        if child.name == ".git":
            continue
        if child.is_dir(follow_symlinks=False):
            yield child.path
