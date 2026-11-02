import tempfile
import unittest
from pathlib import Path

import check_version

ROOT = """[workspace]
members = ["crates/*"]

[workspace.package]
version = "{version}"

[workspace.dependencies]
app-core = {{ version = "{dependency}", path = "crates/app-core" }}
serde = "1"
"""
CORE = """[package]
name = "app-core"
version.workspace = true
"""
APP = """[package]
name = "app"
version = "{version}"

[dependencies]
app-core.workspace = true
"""
LOCK = """version = 4

[[package]]
name = "app"
version = "{app}"

[[package]]
name = "app-core"
version = "{core}"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"""


def write(root, version="1.2.3", dependency="1.2.3", app="1.2.3", lock_app="1.2.3", lock_core="1.2.3"):
    (root / "crates/app-core").mkdir(parents=True)
    (root / "crates/app").mkdir(parents=True)
    (root / "Cargo.toml").write_text(ROOT.format(version=version, dependency=dependency))
    (root / "crates/app-core/Cargo.toml").write_text(CORE)
    (root / "crates/app/Cargo.toml").write_text(APP.format(version=app))
    (root / "Cargo.lock").write_text(LOCK.format(app=lock_app, core=lock_core))


class CheckVersionTest(unittest.TestCase):
    def errors(self, release="1.2.3", **kwargs):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write(root, **kwargs)
            return check_version.check(root, release)

    def test_consistent_workspace_passes(self):
        self.assertEqual(self.errors(), [])

    def test_prerelease_version_is_accepted(self):
        rc = "1.2.3-rc.1"
        errors = self.errors(rc, version=rc, dependency=rc, app=rc, lock_app=rc, lock_core=rc)
        self.assertEqual(errors, [])

    def test_leading_v_is_rejected(self):
        self.assertIn("not a semantic version", self.errors("v1.2.3")[0])

    def test_workspace_version_mismatch(self):
        errors = self.errors(version="1.2.2")
        self.assertTrue(any("[workspace.package]" in e for e in errors))
        self.assertTrue(any("package 'app-core' is '1.2.2'" in e for e in errors))

    def test_member_with_its_own_version_mismatch(self):
        errors = self.errors(app="1.0.0")
        self.assertEqual(len(errors), 1)
        self.assertIn("crates/app/Cargo.toml", errors[0])

    def test_path_dependency_requirement_mismatch(self):
        errors = self.errors(dependency="1.2.2")
        self.assertEqual(len(errors), 1)
        self.assertIn("path dependency 'app-core'", errors[0])

    def test_stale_lockfile_is_reported(self):
        errors = self.errors(lock_core="1.2.2")
        self.assertEqual(errors, ["Cargo.lock: package 'app-core' is '1.2.2'"])

    def test_repository_is_consistent_with_its_own_version(self):
        root = Path(__file__).resolve().parents[2]
        version = check_version.load(root / "Cargo.toml")["workspace"]["package"]["version"]
        self.assertEqual(check_version.check(root, version), [])


if __name__ == "__main__":
    unittest.main()
