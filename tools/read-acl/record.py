#!/usr/bin/env python3
"""Record completed native read-scope acceptance runs and their source hashes."""
import argparse
import hashlib
import json
from pathlib import Path
import re

parser = argparse.ArgumentParser()
for name in ['macos-server', 'macos-cli', 'linux-server', 'linux-cli', 'linux-ssh']:
    parser.add_argument('--' + name, type=Path, required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
out = root / 'crates/kasu-git/tests/fixtures/clients/read-acl'
out.mkdir(parents=True, exist_ok=True)
counts = {}
for name, path in vars(args).items():
    text = path.read_text()
    assert 'FAILED' not in text and 'test result: ok.' in text
    results = re.findall(r'test result: ok\. (\d+) passed; 0 failed;', text)
    counts[name] = sum(map(int, results))
    text = re.sub(r'^.*(?:Compiling |Finished |Blocking waiting for file lock).*\n', '', text, flags=re.M)
    text = re.sub(r'^\s*Running (.*?) \(.*?\)\s*$', r'Running \1', text, flags=re.M)
    (out / (name.replace('_', '-') + '.log')).write_text(text)
assert counts == {'macos_server': 62, 'macos_cli': 12, 'linux_server': 5, 'linux_cli': 12, 'linux_ssh': 1}, counts
sources = ['crates/kasu-server/src/read_scope.rs', 'crates/kasu-server/src/backend.rs',
           'crates/kasu-server/src/accounts.rs', 'crates/kasu-server/src/auth.rs',
           'crates/kasu-server/src/objects.rs', 'crates/kasu-server/src/transport.rs',
           'crates/kasu-server/src/write.rs', 'crates/kasu-server/src/land.rs',
           'crates/kasu-server/tests/accounts.rs', 'crates/kasu-cli/tests/e2e.rs',
           'crates/kasu-proto/proto/kasu.proto', 'crates/kasu-proto/build.rs',
           'tools/ssh/Dockerfile', '.github/workflows/ssh.yml', 'tools/read-acl/record.py']
summary = {
    'success': True, 'captured_date': '2026-10-24',
    'source_sha256': {p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in sources},
    'test_counts': counts,
    'platforms': ['macOS arm64 Git 2.54.0', 'Linux arm64 Rust 1.90.0 Git 2.39.5 OpenSSH 9.2p1'],
    'verified': ['canonical filtered roots',
                 'outside-only updates preserve filtered root ID',
                 'metadata changes filtered, source IDs retained, explicit read_filtered flag',
                 'HTTP and gRPC path denial and non-UTF8 path allowance',
                 'root and private subtree ID denial despite full-repo membership',
                 'scope-only blob metadata and whole-batch authorization before streaming',
                 'hop protection retained for full-reader PAT; direct scoped account Objects access',
                 'private blob reference copying and empty existence-probe upload denied',
                 'allowed upload and draft; outside-readable-scope draft denied',
                 'hidden rewrite conflict denied without exposing entries',
                 'full Git denied and allowed projection advertisement accepted',
                 'permission narrowing selects a separate grant namespace',
                 'two real server processes, filtered root/grants and Land receipt after PAT rotation/restart',
                 'real CLI checkout without sparse flags, commit/Land and hidden content preservation',
                 'CLI never requests the hidden blob; private show returns authorization error',
                 'existing Metadata/Objects/Writer/runtime/Git and CLI regression',
                 'real OpenSSH and go-git regression'],
    'not_verified': ['security audit', 'scope delegation to shared cache/daemon',
                     'persistent read-grant cache GC and capacity', 'production deployment',
                     'GitHub workflow execution', 'real FUSE mount with scoped account']
}
(out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
