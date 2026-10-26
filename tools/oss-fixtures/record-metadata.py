#!/usr/bin/env python3
"""Validate and retain fixed-OSS Metadata acceptance logs and their source hashes."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
from zoneinfo import ZoneInfo

parser = argparse.ArgumentParser()
for name in ['macos-tests', 'linux-tests', 'linux-build']:
    parser.add_argument('--' + name, type=Path, required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
out = root / 'docs/metadata-oss'
out.mkdir(parents=True, exist_ok=True)
required = [
    'crates/kasu-server/tests/oss_metadata.rs',
    'crates/kasu-ingest/tests/fixtures/oss-refs.txt',
    'crates/kasu-ops/src/metadata.rs', 'crates/kasu-ops/src/lib.rs',
    'crates/kasu-derived/src/history.rs',
]
counts = 'fixed OSS metadata: commits=21, paths=9 (including missing), log=189, blame=77, merge-base=441'
for platform, path in [('macos', args.macos_tests), ('linux', args.linux_tests)]:
    log = path.read_text()
    assert counts in log
    assert 'test pinned_oss_metadata_matches_git_for_all_commits_paths_and_tip_pairs ... ok' in log
    assert len(re.findall(r'test result: ok\. 1 passed; 0 failed; 0 ignored;', log)) == 1
    assert 'FAILED' not in log and 'error:' not in log
    log = re.sub(r'^.*(?:Compiling |Finished |Blocking waiting for file lock).*\n', '', log, flags=re.M)
    log = re.sub(r'^\s*Running (.*?) \(.*?\)\s*$', r'Running \1', log, flags=re.M)
    (out / (platform + '.log')).write_text(log)
build = args.linux_build.read_text()
assert 'rustc 1.90.0' in build and 'git version ' in build and 'Linux ' in build
hashes = dict((path, digest) for digest, path in re.findall(r'([0-9a-f]{64})  (crates/[^\s]+)', build))
assert set(hashes) == set(required)
for path, digest in hashes.items():
    assert hashlib.sha256((root / path).read_bytes()).hexdigest() == digest, path
selected = [line for line in build.splitlines()
            if re.search(r'rustc |git version |Linux |[0-9a-f]{64}  crates/', line) and not 'RUN ' in line]
(out / 'linux-build.log').write_text('\n'.join(selected) + '\n')
paths = set(required + ['Cargo.toml', 'Cargo.lock', '.github/workflows/oss-metadata.yml',
                       'tools/oss-fixtures/metadata.Dockerfile', 'tools/oss-fixtures/metadata-run.sh',
                       'tools/oss-fixtures/record-metadata.py', 'crates/kasu-proto/proto/kasu.proto'])
for crate in ['core', 'store', 'derived', 'ops', 'git', 'ingest', 'server', 'proto']:
    directory = root / ('crates/kasu-' + crate)
    paths.add(str((directory / 'Cargo.toml').relative_to(root)))
    paths.update(str(p.relative_to(root)) for p in (directory / 'src').rglob('*.rs'))
    if (directory / 'build.rs').exists():
        paths.add(str((directory / 'build.rs').relative_to(root)))
captured_at = datetime.now(timezone.utc)
summary = {
    'success': True, 'captured_date': captured_at.astimezone(ZoneInfo('Asia/Tokyo')).date().isoformat(),
    'captured_at_utc': captured_at.isoformat(), 'date_timezone': 'Asia/Tokyo',
    'source': 'https://github.com/libgit2/TestGitRepository.git',
    'pinned_refs': (root / required[1]).read_text().splitlines(),
    'commits': 21, 'objects': 70, 'existing_leaf_paths': 8,
    'queries_per_platform': {'first_parent_path_log': 189, 'blame': 77, 'merge_base_all': 441},
    'verified': ['all commits and every historical leaf path, plus a missing path',
                 'two-item log pages, cursor exhaustion and no repeated cursor',
                 'Git blame origin OID, original line number and origin path for every regular file snapshot',
                 'all ordered commit pairs including identical tips and disconnected roots',
                 'native Metadata backend API after import and ref publication',
                 'macOS and pinned Rust 1.90 Linux; Linux network disabled and read-only input mount'],
    'not_verified': ['arbitrary OSS repositories and all Git rename/copy heuristics',
                     'non-UTF8 paths in an upstream input',
                     'GitHub workflow execution'],
    'source_sha256': {p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in sorted(paths)},
}
(out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
