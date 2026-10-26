#!/usr/bin/env python3
"""Retain the reproduced write-failure regression and matching acceptance runs."""
import argparse
from datetime import datetime
import hashlib
import json
from pathlib import Path
import re
from zoneinfo import ZoneInfo

parser = argparse.ArgumentParser()
for name in ['before', 'macos', 'linux', 'clippy']:
    parser.add_argument('--' + name, type=Path, required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
out = root / 'docs/simulations/cache-write-failures'
out.mkdir(parents=True, exist_ok=True)
name = 'cache_write_failures_stop_refetches_preserve_warm_hits_and_recover'
before = args.before.read_text()
assert name + ' ... FAILED' in before
assert 'left: Internal' in before and 'right: Unavailable' in before
macos, linux = args.macos.read_text(), args.linux.read_text()
for log in [macos, linux]:
    assert 'test ' + name + ' ... ok' in log
    assert 'test result: ok. 6 passed; 0 failed; 0 ignored;' in log
    assert 'test result: FAILED' not in log and 'error:' not in log
assert 'rustc 1.90.0' in linux and 'Linux aarch64' in linux
hashes = dict((path, digest) for digest, path in re.findall(r'([0-9a-f]{64})  (crates/[^\s]+)', linux))
required = {'crates/kasu-cache/src/lib.rs', 'crates/kasu-cache/tests/layers.rs'}
assert required <= hashes.keys()
for path, digest in hashes.items():
    assert hashlib.sha256((root / path).read_bytes()).hexdigest() == digest, path
clippy = args.clippy.read_text()
assert 'Finished `dev` profile' in clippy and 'error:' not in clippy and 'warning:' not in clippy

def normalize(log):
    log = re.sub(r'^#\d+ [\d.]+ ', '', log, flags=re.M)
    log = re.sub(r'^.*(?:Compiling |Checking |Finished |Blocking waiting for file lock).*\n', '', log, flags=re.M)
    log = re.sub(r'^\s*Running (.*?) \(.*?\)\s*$', r'Running \1', log, flags=re.M)
    return log

(out / 'before.log').write_text(normalize(before))
(out / 'macos.log').write_text(normalize(macos))
selected = [line for line in linux.splitlines()
            if re.search(r'rustc |git version |Linux aarch64|[0-9a-f]{64}  crates/|test .* \.\.\. |test result:', line)
            and 'RUN ' not in line]
(out / 'linux.log').write_text(normalize('\n'.join(selected) + '\n'))
(out / 'clippy.log').write_text('cargo clippy -p kasu-cache --all-targets --locked -- -D warnings: passed\n')
paths = set(hashes) | {'Cargo.lock', 'crates/kasu-cache/Cargo.toml', 'tools/cache/Dockerfile',
                       'tools/cache/record-failures.py'}
summary = {
    'captured_at': datetime.now(ZoneInfo('Asia/Tokyo')).isoformat(), 'success': True,
    'regression_reproduced_before_fix': True,
    'macos_tests_passed': 6,
    'linux_tests_passed': sum(map(int, re.findall(r'test result: ok\. (\d+) passed;', linux))),
    'linux_tests_ignored': sum(map(int, re.findall(r'test result: ok\. \d+ passed; 0 failed; (\d+) ignored;', linux))),
    'verified': ['3 failed persistence operations open the fill circuit',
                 '30 queued distinct cold misses make no further authority requests',
                 'warm frames remain available while persistence is unavailable',
                 'one failed cooldown probe reopens the circuit despite a successful upstream read',
                 'successful persistence after recovery closes the circuit; the next read is a hit',
                 'macOS cache layer suite; pinned Rust 1.90 Linux cache, daemon and CLI suites',
                 'cache all-targets clippy with warnings denied'],
    'not_verified': ['actual filesystem ENOSPC or power loss; this regression injects Store::put errors',
                     'large multi-node production workload', 'GitHub CI execution'],
    'source_sha256': {p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in sorted(paths)},
}
assert summary['linux_tests_passed'] == 43 and summary['linux_tests_ignored'] == 2
(out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
