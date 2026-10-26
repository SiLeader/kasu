#!/usr/bin/env python3
"""Prepare the original fixed GitLab byte-path history without a working tree."""
import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
URL = 'https://gitlab.com/gitlab-org/gitlab-test.git'
HEAD = '99e4853698007ed7e68826730ef5313d24841991'
BRANCH = 'refs/heads/invalid-utf8-diff-paths'
FIXTURES = ROOT / 'crates/kasu-ingest/tests/fixtures'
REFS = (FIXTURES / 'upstream-byte-path-refs.txt').read_bytes()
OBJECT_LIST = (FIXTURES / 'upstream-byte-path-objects.txt').read_bytes()
OBJECTS = dict(line.split() for line in OBJECT_LIST.decode().splitlines())


def prepare(output, source=None):
    output = output.resolve()
    if output.exists():
        raise ValueError('output must not already exist')
    output.parent.mkdir(parents=True, exist_ok=True)
    env = {'PATH': os.environ['PATH'], 'GIT_CONFIG_GLOBAL': os.devnull,
           'GIT_CONFIG_NOSYSTEM': '1', 'GIT_NO_REPLACE_OBJECTS': '1',
           'GIT_NO_LAZY_FETCH': '1', 'GIT_TERMINAL_PROMPT': '0'}

    def git(repo, *args, data=None):
        result = subprocess.run(['git', '-c', 'core.hooksPath=/dev/null', '-c', 'core.fsmonitor=false',
                                 '-C', str(repo), *map(str, args)], env=env, input=data,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if result.returncode:
            raise RuntimeError(f'git {args[0]} failed: {result.stderr.decode(errors="replace")}')
        return result.stdout

    # Prepare on the destination filesystem; only a fully checked graph is published.
    with tempfile.TemporaryDirectory(prefix='kasu-byte-paths-', dir=output.parent) as tmp:
        staging = Path(tmp) / 'prepared.git'
        staging.mkdir()
        git(staging, 'init', '--bare', '--object-format=sha1', '--template=',
            '--initial-branch=invalid-utf8-diff-paths')
        if source is None:
            refspecs = [f'{oid}:{ref}' for oid, ref in
                        (line.split() for line in REFS.decode().splitlines())]
            git(staging, 'fetch', '--no-tags', '--no-recurse-submodules', '--no-write-fetch-head', URL, *refspecs)
        else:
            # Read exact raw objects only. The new repository never inherits source
            # config, hooks, alternates, remotes or a working tree.
            raw = git(source, 'cat-file', '--batch', data=('\n'.join(OBJECTS) + '\n').encode())
            position = 0
            for oid, kind in OBJECTS.items():
                end = raw.index(b'\n', position)
                header = raw[position:end].decode().split()
                if len(header) != 3 or header[:2] != [oid, kind]:
                    raise ValueError('cached source is missing a pinned object or has the wrong type')
                size = int(header[2])
                body = raw[end + 1:end + 1 + size]
                position = end + size + 2
                if git(staging, 'hash-object', '-w', '-t', kind, '--stdin', data=body).strip().decode() != oid:
                    raise ValueError('cached raw object does not retain its original OID')
            if position != len(raw):
                raise ValueError('unexpected cached object stream')
            for line in REFS.decode().splitlines():
                oid, ref = line.split()
                git(staging, 'update-ref', ref, oid)
        assert git(staging, 'show-ref') == REFS
        assert git(staging, 'symbolic-ref', 'HEAD').strip().decode() == BRANCH
        assert not (staging / 'shallow').exists()
        git(staging, 'fsck', '--strict')
        actual = git(staging, 'cat-file', '--batch-all-objects', '--batch-check=%(objectname) %(objecttype)')
        assert dict(line.split() for line in actual.decode().splitlines()) == OBJECTS
        assert dict(Counter(OBJECTS.values())) == {'commit': 50, 'tree': 76, 'blob': 56, 'tag': 1}
        # Exclude neither deleted-path snapshots nor the older parents behind merges.
        reachable = git(staging, 'rev-list', '--objects', '--all', '--no-object-names').decode().splitlines()
        assert set(reachable) == set(OBJECTS)
        assert int(git(staging, 'rev-list', '--min-parents=2', '--count', '--all')) == 14
        manifest = {
            'source': URL, 'refs': REFS.decode(), 'head': HEAD, 'symbolic_head': BRANCH,
            'scope': 'complete history and all objects reachable from the fixed branch and v1.0.0 tag; other upstream refs excluded',
            'counts': {'commit': 50, 'tree': 76, 'blob': 56, 'tag': 1},
            'object_list_sha256': hashlib.sha256(OBJECT_LIST).hexdigest(), 'merge_commits': 14,
            'non_utf8_path_hex': '68656c6c6f80776f726c64',
            'path_snapshots': [
                {'commit': 'd0888d297eadcd7a345427915c309413b1231e65', 'blob': '3742e48c1108ced3bf45ac633b34b65ac3f2af04'},
                {'commit': '19950f03c765f7ac8723a73a0599764095f52fc0', 'blob': 'ea20973903d3800ac659d503596f9105f0a7423f'},
                {'commit': HEAD, 'blob': None}],
            'strict_fsck': True, 'shallow': False, 'submodule_contents_fetched': False,
            'git': git(staging, '--version').decode().strip(),
        }
        path = bytes.fromhex(manifest['non_utf8_path_hex'])
        for snapshot in manifest['path_snapshots']:
            tree = git(staging, 'ls-tree', '-rz', '--full-tree', snapshot['commit']).split(b'\0')
            matches = [row for row in tree if row.endswith(b'\t' + path)]
            expected = snapshot['blob']
            assert matches == ([] if expected is None else [b'100644 blob ' + expected.encode() + b'\t' + path])
        assert git(staging, 'rev-parse', 'refs/tags/v1.0.0^{}').strip() == b'6f6d7e7ed97bb5f0054f2b1df789b39ca89b6ff9'
        (staging / 'kasu-upstream.json').write_text(json.dumps(manifest, indent=2) + '\n')
        staging.rename(output)
        print(json.dumps(manifest, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--source', type=Path, help='optional cached repository containing all pinned raw objects')
    args = parser.parse_args()
    prepare(args.output, args.source)
