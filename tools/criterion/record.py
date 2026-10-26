#!/usr/bin/env python3
"""Validate Criterion samples, matching source hashes, strict pack sizes and CI smoke runs."""
import argparse
import csv
from datetime import datetime
import hashlib
import json
import math
from pathlib import Path
import re
import statistics
from zoneinfo import ZoneInfo

parser = argparse.ArgumentParser()
for name in ['macos-output', 'linux-output', 'macos-log', 'linux-log', 'linux-build', 'workspace-tests', 'clippy']:
    parser.add_argument('--' + name, type=Path, required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
out = root / 'docs/benchmarks/criterion'
out.mkdir(parents=True, exist_ok=True)
expected = {f'changeset_create/{n}' for n in [100, 1000, 10000]}
expected |= {f'path_history/{mode}/{n}' for mode in ['cold', 'warm'] for n in [100, 1000]}
expected |= {f'pack_encode/{mode}/{n}' for mode in ['cold', 'warm'] for n in [4096, 262144]}
all_data = {}
pack_sizes = {}
for platform, directory, log_file in [('macos', args.macos_output, args.macos_log), ('linux', args.linux_output, args.linux_log)]:
    log = log_file.read_text()
    assert 'error:' not in log and 'panicked' not in log
    data = {}
    for p in directory.glob('criterion/**/new/benchmark.json'):
        benchmark = json.loads(p.read_text())
        key = benchmark['full_id']
        assert key in expected and key not in data
        estimates = json.loads(p.with_name('estimates.json').read_text())
        samples = json.loads(p.with_name('sample.json').read_text())
        assert len(samples['iters']) == len(samples['times']) == 20
        assert all(math.isfinite(n) and n > 0 for n in samples['iters'] + samples['times'])
        mean = statistics.mean(t / i for t, i in zip(samples['times'], samples['iters']))
        assert math.isclose(mean, estimates['mean']['point_estimate'], rel_tol=1e-10)
        interval = estimates['mean']['confidence_interval']
        assert 0 <= interval['lower_bound'] <= mean <= interval['upper_bound']
        assert interval['confidence_level'] == 0.95
        assert f'Benchmarking {key}: Analyzing' in log
        data[key] = {'benchmark': benchmark, 'estimates': estimates, 'sample': samples,
                     'tukey': json.loads(p.with_name('tukey.json').read_text())}
    assert set(data) == expected
    all_data[platform] = data
    (out / (platform + '.json')).write_text(json.dumps(data, indent=2) + '\n')
    with (directory / 'pack-sizes.csv').open() as handle:
        sizes = list(csv.DictReader(handle))
    assert [int(row['blob_bytes']) for row in sizes] == [4096, 262144]
    assert all(row['index_pack_strict'] == row['fsck_strict'] == 'true' for row in sizes)
    assert all(int(row['commits']) == 101 and int(row['objects']) == 109 for row in sizes)
    assert all(int(row['pack_bytes']) > 0 and int(row['raw_bytes']) > 2 * int(row['blob_bytes']) for row in sizes)
    pack_sizes[platform] = sizes
    text = re.sub(r'^.*(?:Compiling |Finished |Blocking waiting for file lock).*\n', '', log, flags=re.M)
    text = re.sub(r'^\s*Running (.*?) \(.*?\)\s*$', r'Running \1', text, flags=re.M)
    (out / (platform + '.log')).write_text(text)
assert pack_sizes['macos'] == pack_sizes['linux']
(out / 'pack-sizes.csv').write_bytes((args.macos_output / 'pack-sizes.csv').read_bytes())

build = args.linux_build.read_text()
assert 'rustc 1.90.0' in build and 'git version 2.39.5' in build and 'Linux aarch64' in build
required = {'Cargo.lock', 'crates/kasu-ingest/Cargo.toml', 'crates/kasu-ingest/benches/throughput.rs'}
for log in [build, args.macos_log.read_text()]:
    hashes = {path: digest for digest, path in re.findall(r'([0-9a-f]{64})  (Cargo\.lock|crates/[^\s]+)', log)}
    assert set(hashes) == required
    for path, digest in hashes.items():
        assert hashlib.sha256((root / path).read_bytes()).hexdigest() == digest, path
selected = [line for line in build.splitlines() if re.search(r'rustc |git version |Linux aarch64|[0-9a-f]{64}  (?:Cargo.lock|crates/)', line) and 'RUN ' not in line]
(out / 'linux-build.log').write_text('\n'.join(selected) + '\n')
tests = args.workspace_tests.read_text()
assert 'error:' not in tests and 'FAILED' not in tests
assert set(re.findall(r'^Testing (.+)$', tests, re.M)) == expected
assert len(re.findall(r'^Success$', tests, re.M)) == len(expected)
passed = sum(map(int, re.findall(r'test result: ok\. (\d+) passed;', tests)))
ignored = sum(map(int, re.findall(r'test result: ok\. \d+ passed; 0 failed; (\d+) ignored;', tests)))
assert passed >= 212
clippy = args.clippy.read_text()
assert 'Finished `dev` profile' in clippy and 'warning:' not in clippy and 'error:' not in clippy
normalized = re.sub(r'^.*(?:Compiling |Finished |Blocking waiting for file lock).*\n', '', tests, flags=re.M)
normalized = re.sub(r'^\s*Running (.*?) \(.*?\)\s*$', r'Running \1', normalized, flags=re.M)
(out / 'workspace-tests.log').write_text(normalized)
(out / 'clippy.log').write_text('cargo clippy --workspace --all-targets --all-features --locked -- -D warnings: passed\n')
paths = set(required) | {'Cargo.toml', '.dockerignore', '.github/workflows/criterion.yml',
                       'tools/criterion/Dockerfile', 'tools/criterion/run.sh', 'tools/criterion/record.py'}
for directory in (root / 'crates').iterdir():
    if not directory.is_dir():
        continue
    paths.add(str((directory / 'Cargo.toml').relative_to(root)))
    paths.update(str(p.relative_to(root)) for p in directory.rglob('*.rs'))
    paths.update(str(p.relative_to(root)) for p in directory.rglob('*.proto'))
paths.add('.github/workflows/ci.yml')
summary = {
    'success': True, 'captured_at': datetime.now(ZoneInfo('Asia/Tokyo')).isoformat(),
    'criterion_version': '0.7.0', 'cases_per_platform': 11, 'samples_per_case': 20,
    'workspace_tests_passed': passed, 'workspace_tests_ignored': ignored,
    'workspace_benchmark_smoke_cases': len(expected), 'pack_sizes': pack_sizes['macos'],
    'mean_nanoseconds': {platform: {key: item['estimates']['mean']['point_estimate'] for key, item in sorted(data.items())} for platform, data in all_data.items()},
    'verified': ['all 11 Criterion cases on macOS and pinned Rust 1.90 Linux',
                 'raw samples recompute the recorded mean; finite positive samples and 95% intervals',
                 'cold and warm byte-identical packs with strict index-pack and fsck',
                 'identical pack sizes and input cardinalities across both platforms',
                 'all-targets/all-features workspace tests include all benchmark smoke cases',
                 'all-targets/all-features workspace clippy with warnings denied'],
    'not_verified': ['PostgreSQL/S3/network performance', 'large production corpus or latency SLA',
                     'GitHub workflow execution'],
    'source_sha256': {p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in sorted(paths)},
}
(out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps({'cases': 11, 'workspace_passed': passed, 'workspace_ignored': ignored}))
