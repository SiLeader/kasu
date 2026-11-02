#!/usr/bin/env python3
"""Rewrite every workspace version to the release version.

Usage: set_version.py <version> [repository-root]

Updates what check_version.py compares: [workspace.package].version, the version
of members that declare their own, the `version` of path dependencies in inline
tables, and the Cargo.lock entries of workspace members. Everything else in the
files is left as it is. Run check_version.py afterwards to confirm.
"""
import re
import sys
import tomllib
from pathlib import Path

import check_version

SECTION_VERSION = re.compile(r'^(version\s*=\s*")[^"]*(")', re.MULTILINE)
PATH_DEPENDENCY_VERSION = re.compile(r'(?<![-\w])(version\s*=\s*")([=^]?)[^"]*(")')
INLINE_PATH_DEPENDENCY = re.compile(r"\{[^}]*\bpath\s*=")
LOCK_ENTRY = re.compile(r'(\[\[package\]\]\nname = "([^"]+)"\nversion = ")[^"]*("\n(?!source))')


def set_section_version(text, section, version):
    header = re.search(rf"^\[{re.escape(section)}\][ \t]*$", text, re.MULTILINE)
    if header is None:
        return text
    following = re.search(r"^\[", text[header.end() :], re.MULTILINE)
    stop = header.end() + following.start() if following else len(text)
    body = SECTION_VERSION.sub(
        lambda m: m[1] + version + m[2], text[header.end() : stop], count=1
    )
    return text[: header.end()] + body + text[stop:]


def set_path_dependency_versions(text, version):
    lines = text.split("\n")
    for index, line in enumerate(lines):
        if INLINE_PATH_DEPENDENCY.search(line):
            lines[index] = PATH_DEPENDENCY_VERSION.sub(
                lambda m: m[1] + m[2] + version + m[3], line
            )
    return "\n".join(lines)


def set_lock_versions(text, members, version):
    return LOCK_ENTRY.sub(
        lambda m: m[1] + version + m[3] if m[2] in members else m[0], text
    )


def rewrite(path, transform):
    old = path.read_text()
    new = transform(old)
    if new != old:
        path.write_text(new)
        return True
    return False


def apply(root, version):
    """Rewrite the files under root; return the changed paths relative to root."""
    problem = check_version.invalid_version(version)
    if problem:
        raise ValueError(problem)
    root_manifest = root / "Cargo.toml"
    members = {}
    manifests = [root_manifest]
    for pattern in check_version.load(root_manifest)["workspace"].get("members", []):
        manifests += sorted(root.glob(f"{pattern}/Cargo.toml"))
    changed = []
    for path in manifests:
        package = tomllib.loads(path.read_text()).get("package")
        if package:
            members[package["name"]] = None

        def transform(text, path=path):
            if path == root_manifest:
                text = set_section_version(text, "workspace.package", version)
            text = set_section_version(text, "package", version)
            return set_path_dependency_versions(text, version)

        if rewrite(path, transform):
            changed.append(path)
    lock = root / "Cargo.lock"
    if rewrite(lock, lambda text: set_lock_versions(text, members, version)):
        changed.append(lock)
    return [path.relative_to(root).as_posix() for path in changed]


def main(argv):
    if not 2 <= len(argv) <= 3:
        print(__doc__, file=sys.stderr)
        return 2
    root = Path(argv[2]) if len(argv) == 3 else Path(__file__).resolve().parents[2]
    try:
        changed = apply(root, argv[1])
    except ValueError as error:
        print(error, file=sys.stderr)
        return 2
    for path in changed:
        print(f"updated {path}")
    if not changed:
        print(f"already at {argv[1]}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
