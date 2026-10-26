#!/usr/bin/env python3
"""Prepare the pinned libgit2 submodule graph without upstream execution config."""
import argparse
import io
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import tarfile
import tempfile

URL = 'https://github.com/libgit2/libgit2.git'
COMMIT = '338e6fb681369ff0537719095e22ce9dc602dbf0'
RESOURCE = 'tests/resources/submodules.git'
REFS = b'97896810b3210244a62a82458b8e0819ecfc6850 refs/heads/master\n'
OBJECTS = {
    '09176a980273d801a3e37cc45c84af1366501ed9': 'commit',
    '092bfb9bdf74dd8cfd22e812151281ee9aa6f01a': 'blob',
    '0f8fb758578188a8fdf04e29bac3ace81b4f3c9c': 'tree',
    '78308c9251cf4eee8b25a76c7d2790c73d797357': 'blob',
    '97896810b3210244a62a82458b8e0819ecfc6850': 'commit',
    'b60fd986699ba4e9e68bea07cf8e793f323ef888': 'tree',
}

def prepare(output, source=None):
    output = output.resolve()
    if output.exists():
        raise ValueError('output must not already exist')
    output.parent.mkdir(parents=True, exist_ok=True)
    env = {'PATH': os.environ['PATH'], 'GIT_CONFIG_GLOBAL': os.devnull,
           'GIT_CONFIG_NOSYSTEM': '1', 'GIT_NO_REPLACE_OBJECTS': '1', 'GIT_TERMINAL_PROMPT': '0'}

    def git(repo, *args):
        result = subprocess.run(['git', '-c', 'core.hooksPath=/dev/null', '-c', 'core.fsmonitor=false',
                                 '-C', str(repo), *map(str, args)], env=env,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if result.returncode:
            raise RuntimeError(f'git {args[0]} failed: {result.stderr.decode(errors="replace")}')
        return result.stdout

    with tempfile.TemporaryDirectory(prefix='kasu-oss-prepare-') as tmp:
        temp = Path(tmp)
        if source is None:
            source = temp / 'upstream.git'
            source.mkdir()
            git(source, 'init', '--bare', '--object-format=sha1', '--template=', '--initial-branch=main')
            git(source, 'fetch', '--no-tags', '--depth=1', '--filter=blob:none', URL, COMMIT)
        assert git(source, 'rev-parse', COMMIT + '^{commit}').strip().decode() == COMMIT
        archive = git(source, 'archive', COMMIT, RESOURCE)
        staging = temp / 'staging.git'
        staging.mkdir()
        git(staging, 'init', '--bare', '--object-format=sha1', '--template=', '--initial-branch=master')
        files = 0
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            for member in tar:
                if not member.isfile() or not member.name.startswith(RESOURCE + '/'):
                    continue
                name = member.name[len(RESOURCE) + 1:]
                rel = PurePosixPath(name)
                if rel.is_absolute() or '..' in rel.parts:
                    raise ValueError('invalid archive member')
                if name == 'objects/info/alternates':
                    continue
                if name not in ['HEAD', 'packed-refs'] and not name.startswith(('objects/', 'refs/')):
                    continue
                destination = staging / rel
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(tar.extractfile(member).read())
                files += 1
        assert git(staging, 'show-ref') == REFS
        # The upstream ODB includes three unreferenced blobs. Transfer the complete
        # reachable graph with Git's own local upload-pack, preserving all refs.
        git(temp, 'clone', '--bare', '--no-local', '--template=', staging, output)
        git(output, 'config', '--remove-section', 'remote.origin')
        git(output, 'fsck', '--strict')
        assert git(output, 'show-ref') == REFS
        lines = git(output, 'cat-file', '--batch-all-objects', '--batch-check=%(objectname) %(objecttype)').decode().splitlines()
        assert dict(line.split() for line in lines) == OBJECTS
        manifest = {'source': URL, 'commit': COMMIT, 'resource': RESOURCE,
                    'scope': 'all objects reachable from the original refs; three dangling blobs omitted',
                    'objects': OBJECTS, 'refs': REFS.decode(), 'source_metadata_files': files,
                    'git': git(output, '--version').decode().strip(), 'strict_fsck': True,
                    'non_utf8_paths': False, 'gitlink_target_fetched': False}
        (output / 'kasu-upstream.json').write_text(json.dumps(manifest, indent=2) + '\n')
        print(json.dumps(manifest, indent=2))

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--source', type=Path, help='optional local libgit2 repository containing the pinned commit')
    args = parser.parse_args()
    prepare(args.output, args.source)
