import shutil
import tempfile
import unittest
from pathlib import Path

import check_version
import set_version
from test_check_version import write


class SetVersionTest(unittest.TestCase):
    def stale_workspace(self, directory):
        root = Path(directory)
        write(root, version="1.2.2", dependency="1.2.2", app="1.0.0",
              lock_app="1.2.2", lock_core="1.2.2")
        return root

    def test_mismatched_workspace_becomes_consistent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self.stale_workspace(directory)
            changed = set_version.apply(root, "1.2.3")
            self.assertEqual(
                sorted(changed),
                ["Cargo.lock", "Cargo.toml", "crates/app/Cargo.toml"],
            )
            self.assertEqual(check_version.check(root, "1.2.3"), [])

    def test_unrelated_lines_are_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self.stale_workspace(directory)
            set_version.apply(root, "1.2.3")
            self.assertIn('serde = "1"', (root / "Cargo.toml").read_text())
            lock = (root / "Cargo.lock").read_text()
            self.assertIn('name = "serde"\nversion = "1.0.0"\nsource =', lock)

    def test_second_run_changes_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self.stale_workspace(directory)
            set_version.apply(root, "1.2.3")
            self.assertEqual(set_version.apply(root, "1.2.3"), [])

    def test_invalid_version_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self.stale_workspace(directory)
            with self.assertRaises(ValueError):
                set_version.apply(root, "v1.2.3")

    def test_repository_files_can_be_moved_to_another_version(self):
        source = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ["Cargo.toml", "Cargo.lock", *[
                p.relative_to(source).as_posix() for p in source.glob("crates/*/Cargo.toml")
            ]]:
                (root / name).parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source / name, root / name)
            set_version.apply(root, "9.8.7-rc.1")
            self.assertEqual(check_version.check(root, "9.8.7-rc.1"), [])


if __name__ == "__main__":
    unittest.main()
