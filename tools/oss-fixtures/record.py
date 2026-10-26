#!/usr/bin/env python3
"""Record complete fixed-upstream gitlink acceptance evidence."""
import argparse
import hashlib
import json
from pathlib import Path
import re
from prepare import COMMIT, OBJECTS, REFS, RESOURCE, URL

parser = argparse.ArgumentParser()
for arg in ['prepared', 'online-prepared', 'macos-tests', 'linux-tests', 'linux-build']:
    parser.add_argument('--' + arg, type=Path, required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
out = root / 'docs/git-import-upstream-submodule'
out.mkdir(parents=True, exist_ok=True)
prepared = json.loads(args.prepared.read_text())
assert prepared == json.loads(args.online_prepared.read_text())
assert (prepared['source'], prepared['commit'], prepared['resource']) == (URL, COMMIT, RESOURCE)
assert prepared['objects'] == OBJECTS and prepared['refs'] == REFS.decode()
assert prepared['strict_fsck'] and not prepared['gitlink_target_fetched']
(out / 'preparation.json').write_text(json.dumps(prepared, indent=2) + '\n')
for platform, path, expected in [('macos', args.macos_tests, 11), ('linux', args.linux_tests, 10)]:
    text = path.read_text()
    assert 'FAILED' not in text and 'error:' not in text
    assert sum(map(int, re.findall(r'test result: ok\. (\d+) passed; 0 failed;', text))) == expected
    assert 'test pinned_upstream_submodule_fixture_roundtrips_all_reachable_objects ... ok' in text
    text = re.sub(r'^.*(?:Compiling |Finished |Blocking waiting for file lock).*\n', '', text, flags=re.M)
    text = re.sub(r'^\s*Running (.*?) \(.*?\)\s*$', r'Running \1', text, flags=re.M)
    (out / (platform + '.log')).write_text(text)
build = args.linux_build.read_text()
assert 'rustc 1.90.0' in build
for digest, path in re.findall(r'([0-9a-f]{64})  (crates/[^\s]+)', build):
    assert hashlib.sha256((root / path).read_bytes()).hexdigest() == digest, path
selected = [line for line in build.splitlines() if re.search(r'rustc |git version |Linux aarch64|[0-9a-f]{64}  crates/', line)]
(out / 'linux-build.log').write_text('\n'.join(selected) + '\n')
paths = ['Cargo.lock', 'crates/kasu-ingest/tests/roundtrip.rs', 'crates/kasu-ingest/src/lib.rs',
         'crates/kasu-ingest/tests/fixtures/upstream-submodule-refs.txt', 'tools/oss-fixtures/prepare.py',
         'tools/oss-fixtures/run.sh', 'tools/oss-fixtures/Dockerfile', 'tools/oss-fixtures/record.py',
         '.github/workflows/oss-roundtrip.yml']
summary = {'success': True, 'captured_date': '2026-10-26',
           'source': {'url': URL, 'commit': COMMIT, 'resource': RESOURCE, 'objects': OBJECTS},
           'reachable_objects': 6, 'commits': 2, 'gitlinks': 1, 'dangling_blobs_omitted': 3,
           'gitlink_path_hex': '746573747265706f', 'gitlink_target': 'a65fedf39aefe402d3bb6e24df4d4f5fe4547750',
           'verified': ['cached and network preparation match', 'all reachable raw object bytes and OIDs match',
                        'strict source fsck, strict index-pack, strict exported/clone fsck',
                        'refs, peeled tags and symbolic HEAD preserved', 'clone object graph equals source',
                        'native Entry::Gitlink exactly retains target OID', 'gitlink target has no imported changeset',
                        'Linux run uses no network and a read-only input mount',
                        'macOS 11 tests including original TestGitRepository and GitLab; Linux 10 tests with pinned Rust 1.90'],
           'not_verified': ['non-UTF8 paths in this libgit2 fixture; covered separately by the GitLab input', 'submodule content checkout',
                            'GitHub workflow execution'],
           'source_sha256': {p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in paths}}
(out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
