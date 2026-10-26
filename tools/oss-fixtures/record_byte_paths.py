#!/usr/bin/env python3
"""Validate and retain the fixed upstream non-UTF8-path roundtrip evidence."""
import argparse
from datetime import datetime
import hashlib
import json
from pathlib import Path
import re
from zoneinfo import ZoneInfo
from prepare_byte_paths import ROOT, URL, HEAD, BRANCH, REFS, OBJECT_LIST, OBJECTS

parser = argparse.ArgumentParser()
for name in ['prepared', 'online-prepared', 'macos-tests', 'linux-tests', 'linux-build']:
    parser.add_argument('--' + name, type=Path, required=True)
parser.add_argument('--image-id', required=True)
args = parser.parse_args()
out = ROOT / 'docs/git-import-byte-paths'
out.mkdir(parents=True, exist_ok=True)
prepared = json.loads(args.prepared.read_text())
assert prepared == json.loads(args.online_prepared.read_text())
assert (prepared['source'], prepared['head'], prepared['symbolic_head'], prepared['refs']) == (URL, HEAD, BRANCH, REFS.decode())
assert prepared['object_list_sha256'] == hashlib.sha256(OBJECT_LIST).hexdigest()
assert len(OBJECTS) == 183 and prepared['counts'] == {'commit': 50, 'tree': 76, 'blob': 56, 'tag': 1}
assert prepared['strict_fsck'] and not prepared['shallow'] and not prepared['submodule_contents_fetched']
assert prepared['merge_commits'] == 14 and prepared['non_utf8_path_hex'] == '68656c6c6f80776f726c64'
assert len(prepared['path_snapshots']) == 3 and prepared['path_snapshots'][-1] == {'commit': HEAD, 'blob': None}
(out / 'preparation.json').write_text(json.dumps(prepared, indent=2) + '\n')
for platform, path, count in [('macos', args.macos_tests, 11), ('linux', args.linux_tests, 10)]:
    log = path.read_text()
    assert f'test result: ok. {count} passed; 0 failed; 0 ignored;' in log
    assert 'test pinned_gitlab_byte_paths_tags_and_merges_roundtrip ... ok' in log
    assert 'test pinned_upstream_submodule_fixture_roundtrips_all_reachable_objects ... ok' in log
    assert 'FAILED' not in log and 'error:' not in log
    log = re.sub(r'^.*(?:Compiling |Finished |Blocking waiting for file lock).*\n', '', log, flags=re.M)
    log = re.sub(r'^\s*Running (.*?) \(.*?\)\s*$', r'Running \1', log, flags=re.M)
    (out / (platform + '.log')).write_text(log)
build = args.linux_build.read_text()
assert 'rustc 1.90.0' in build and 'Linux aarch64' in build and 'git version 2.39.5' in build
assert 'exporting manifest list ' + args.image_id in build
hashes = dict((path, digest) for digest, path in re.findall(r'([0-9a-f]{64})  (crates/[^\s]+)', build))
assert set(hashes) == {
    'crates/kasu-ingest/tests/roundtrip.rs', 'crates/kasu-ingest/src/lib.rs',
    'crates/kasu-ingest/tests/fixtures/upstream-submodule-refs.txt',
    'crates/kasu-ingest/tests/fixtures/upstream-byte-path-refs.txt',
    'crates/kasu-ingest/tests/fixtures/upstream-byte-path-objects.txt',
}
for path, digest in hashes.items():
    assert hashlib.sha256((ROOT / path).read_bytes()).hexdigest() == digest, path
selected = [line for line in build.splitlines() if re.search(r'rustc |git version |Linux aarch64|[0-9a-f]{64}  crates/|exporting manifest list ', line) and 'RUN ' not in line]
(out / 'linux-build.log').write_text('\n'.join(selected) + '\n')
paths = set(hashes) | {'Cargo.lock', 'tools/oss-fixtures/prepare_byte_paths.py',
    'tools/oss-fixtures/record_byte_paths.py', 'tools/oss-fixtures/run.sh',
    'tools/oss-fixtures/Dockerfile', '.github/workflows/oss-roundtrip.yml'}
for crate in ['core', 'store', 'derived', 'git', 'ingest']:
    directory = ROOT / ('crates/kasu-' + crate)
    paths.add(str((directory / 'Cargo.toml').relative_to(ROOT)))
    paths.update(str(p.relative_to(ROOT)) for p in (directory / 'src').rglob('*.rs'))
summary = {
    'success': True, 'captured_at': datetime.now(ZoneInfo('Asia/Tokyo')).isoformat(),
    'source': URL, 'refs': REFS.decode().splitlines(), 'scope': prepared['scope'],
    'commits': 50, 'objects': 183, 'counts': prepared['counts'], 'merges': 14,
    'non_utf8_path_hex': prepared['non_utf8_path_hex'], 'path_snapshots': prepared['path_snapshots'],
    'gitlinks_at_head': {
        'gitlab-grack': '645f6c4c82fd3f5e06f67134450a570b795e55a6',
        'gitlab-shell': '79bceae69cb5750d6567b223597999bfa91cb3b9',
        'six': '409f37c4f05865e4fb208c771485f211a22c4c2d'},
    'linux_image_manifest': args.image_id,
    'verified': ['public network fetch and cached raw-object preparation produce identical manifests',
        'fixed branch and annotated tag, complete parent history with no shallow boundary',
        'all 183 original raw object bytes and Git OIDs match after Kasu import',
        'native byte-path Set, content update and Delete preserve the exact bytes',
        'all 50 native parent lists match Git; 14 merge commits',
        'all 3 native gitlinks at HEAD preserve target OIDs without importing target commits',
        'strict source fsck, exported index-pack, exported and bare-clone fsck',
        'all refs, peeled tags, symbolic HEAD and clone object graph match',
        'macOS 11 roundtrip tests and pinned Rust 1.90 Linux 10 roundtrip tests',
        'Linux run uses no network and read-only source mounts'],
    'not_verified': ['every other ref in the upstream GitLab test corpus',
        'worktree checkout of non-UTF8 filenames on APFS',
        'submodule contents and Git LFS payloads', 'large production workload',
        'GitHub workflow execution'],
    'source_sha256': {p: hashlib.sha256((ROOT / p).read_bytes()).hexdigest() for p in sorted(paths)},
}
(out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
