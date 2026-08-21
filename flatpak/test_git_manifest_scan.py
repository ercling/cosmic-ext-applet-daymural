import os
import tempfile
import unittest
from pathlib import Path

from git_manifest_scan import child_manifest_directories


class ChildManifestDirectoriesTest(unittest.TestCase):
    def test_excludes_git_metadata_and_directory_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "checkout"
            outside = Path(temp) / "outside"
            real_package = root / "real-package"
            git_metadata = root / ".git" / "stale-package"
            outside_package = outside / "poison-package"

            for directory in (real_package, git_metadata, outside_package):
                directory.mkdir(parents=True)
                (directory / "Cargo.toml").write_text(
                    f'[package]\nname = "{directory.name}"\nversion = "0.0.0"\n',
                    encoding="utf-8",
                )
            os.symlink(outside_package, root / "symlink-package", target_is_directory=True)

            discovered = {
                Path(path).relative_to(root).as_posix()
                for path in child_manifest_directories(os.fspath(root))
            }

            self.assertEqual(discovered, {"real-package"})


if __name__ == "__main__":
    unittest.main()
