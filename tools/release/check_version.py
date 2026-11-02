#!/usr/bin/env python3
"""Check that every version in the workspace matches the release version.

Usage: check_version.py <version> [repository-root]

Compared against <version>:
  * [workspace.package].version in the root Cargo.toml
  * the version of every workspace member
  * the `version` requirement of every path dependency (the root
    [workspace.dependencies] table and each member's dependency tables)
  * the Cargo.lock entry of every workspace member
"""
import re
import sys
import tomllib
from pathlib import Path

SEMVER = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$"
)
DEPENDENCY_KINDS = ("dependencies", "dev-dependencies", "build-dependencies")


def load(path):
    with open(path, "rb") as handle:
        return tomllib.load(handle)


def dependency_tables(manifest):
    workspace = manifest.get("workspace", {})
    if "dependencies" in workspace:
        yield workspace["dependencies"]
    for kind in DEPENDENCY_KINDS:
        if kind in manifest:
            yield manifest[kind]
    for target in manifest.get("target", {}).values():
        for kind in DEPENDENCY_KINDS:
            if kind in target:
                yield target[kind]


def check(root, version):
    """Return a list of human-readable mismatches (empty when consistent)."""
    if not SEMVER.match(version):
        return [
            f"{version!r} is not a semantic version such as 1.2.3 or 1.2.3-rc.1 "
            "(no leading 'v')"
        ]
    errors = []
    root_manifest = load(root / "Cargo.toml")
    workspace = root_manifest["workspace"]
    workspace_version = workspace.get("package", {}).get("version")
    if workspace_version != version:
        errors.append(
            f"Cargo.toml: [workspace.package] version is {workspace_version!r}"
        )

    manifests = {"Cargo.toml": root_manifest}
    members = {}
    for pattern in workspace.get("members", []):
        for path in sorted(root.glob(f"{pattern}/Cargo.toml")):
            manifests[path.relative_to(root).as_posix()] = load(path)
    for name, manifest in manifests.items():
        package = manifest.get("package")
        if package is None:
            continue
        declared = package.get("version")
        if isinstance(declared, dict) and declared.get("workspace"):
            declared = workspace_version
        members[package["name"]] = declared
        if declared != version:
            errors.append(f"{name}: package {package['name']!r} is {declared!r}")

    for name, manifest in manifests.items():
        for table in dependency_tables(manifest):
            for dependency, spec in table.items():
                if not isinstance(spec, dict) or "path" not in spec:
                    continue
                requirement = spec.get("version")
                if requirement is not None and requirement.lstrip("=^") != version:
                    errors.append(
                        f"{name}: path dependency {dependency!r} requires {requirement!r}"
                    )

    locked = {}
    for package in load(root / "Cargo.lock").get("package", []):
        if "source" not in package and package["name"] in members:
            locked[package["name"]] = package["version"]
    for name in sorted(members):
        if locked.get(name) != version:
            errors.append(f"Cargo.lock: package {name!r} is {locked.get(name)!r}")
    return errors


def main(argv):
    if not 2 <= len(argv) <= 3:
        print(__doc__, file=sys.stderr)
        return 2
    root = Path(argv[2]) if len(argv) == 3 else Path(__file__).resolve().parents[2]
    errors = check(root, argv[1])
    if errors:
        print(f"release version {argv[1]} does not match the repository:")
        for error in errors:
            print(f"  - {error}")
        return 1
    print(f"all workspace versions match {argv[1]}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
