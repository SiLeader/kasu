#!/usr/bin/env python3
"""Retain reproducible cache fault traces and matching native/Linux source evidence."""
import argparse
from datetime import datetime
import hashlib
import json
from pathlib import Path
import re
from zoneinfo import ZoneInfo

parser = argparse.ArgumentParser()
for name in ['macos', 'linux', 'macos-build', 'linux-build', 'unit', 'layers', 'processes', 'clippy', 'before']:
    parser.add_argument('--' + name, type=Path, required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
out = root / 'docs/simulations/cache-deterministic'
out.mkdir(parents=True, exist_ok=True)
phases = ['coalesced_wave', 'cache_unavailable_cached_and_cold_fail_closed', 'empty_cache_restarted',
          'truncated_frame_rejected', 'upstream_deadline', 'persistence_circuit_blocks_burst',
          'failed_half_open_probe', 'persistence_recovered', 'daemon_disk_reopened']
expected_times = [5, 9, 18, 20, 80, 110, 140, 170, 170]
results = {}
for seed in [0, 1, 42, 2026]:
    files = [directory / f'seed-{seed}-{repeat}.json'
             for directory in [args.macos, args.linux] for repeat in [1, 2]]
    bodies = [p.read_bytes() for p in files]
    assert all(body == bodies[0] for body in bodies)
    data = json.loads(bodies[0])
    assert data['seed'] == seed and data['clients'] == 1000
    assert data['virtual_time_ms'] == 170
    assert data['initial_authority_requests'] == 1
    assert data['initial_node_coalesced'] == [499, 499] and data['initial_cache_coalesced'] == 1
    assert data['outage_authority_requests'] == data['disk_reopen_authority_requests'] == 0
    assert (data['truncated_replies'], data['timed_out_replies'], data['persistence_failures'], data['authority_requests']) == (1, 1, 4, 11)
    assert [e['phase'] for e in data['events']] == phases
    assert [e['virtual_ms'] for e in data['events']] == expected_times
    assert all(e['cache_pending'] == 0 and e['node_pending'] == [0, 0] for e in data['events'])
    assert re.fullmatch('[0-9a-f]{64}', data['result_digest'])
    results[str(seed)] = data['result_digest']
    (out / f'seed-{seed}.json').write_bytes(bodies[0])
assert len(set(results.values())) == 4
required = {'Cargo.lock', 'crates/kasu-cache/src/lib.rs', 'crates/kasu-cache/Cargo.toml',
            'crates/kasu-sim/Cargo.toml', 'crates/kasu-sim/src/cache.rs', 'crates/kasu-sim/src/main.rs'}
for platform, path in [('macos', args.macos_build), ('linux', args.linux_build)]:
    log = path.read_text()
    hashes = {p: digest for digest, p in re.findall(r'([0-9a-f]{64})  (Cargo\.lock|crates/[^\s]+)', log)}
    assert set(hashes) == required
    for p, digest in hashes.items():
        assert hashlib.sha256((root / p).read_bytes()).hexdigest() == digest, p
    if platform == 'linux':
        assert 'rustc 1.90.0' in log and 'Linux aarch64' in log
        for test in ['circuit_cooldown_and_half_open_probe_follow_the_runtime_clock',
                     'seeded_cache_faults_repeat_with_real_layers_and_virtual_deadlines',
                     'cache_write_failures_stop_refetches_preserve_warm_hits_and_recover']:
            assert test + ' ... ok' in log
        assert sum(map(int, re.findall(r'test result: ok\. (\d+) passed; 0 failed;', log))) == 9
    selected = [line for line in log.splitlines()
                if re.search(r'rustc |Darwin arm64|Linux aarch64|[0-9a-f]{64}  (?:Cargo.lock|crates/)|test .* \.\.\. |test result:', line) and 'RUN ' not in line]
    (out / (platform + '-build.log')).write_text('\n'.join(selected) + '\n')
before = args.before.read_text()
assert 'circuit_cooldown_and_half_open_probe_follow_the_runtime_clock ... FAILED' in before
(out / 'clock-before.log').write_text('\n'.join(line for line in before.splitlines()
    if not any(x in line for x in ['Compiling ', 'Finished ', 'Running '])) + '\n')
for name, path, count in [('unit', args.unit, 3), ('layers', args.layers, 6), ('processes', args.processes, 50)]:
    log = path.read_text()
    assert 'error:' not in log and 'FAILED' not in log
    assert sum(map(int, re.findall(r'test result: ok\. (\d+) passed; 0 failed;', log))) == count
    log = re.sub(r'^.*(?:Compiling |Finished |Blocking waiting for file lock).*\n', '', log, flags=re.M)
    log = re.sub(r'^\s*Running (.*?) \(.*?\)\s*$', r'Running \1', log, flags=re.M)
    (out / (name + '.log')).write_text(log)
clippy = args.clippy.read_text()
assert 'Finished `dev` profile' in clippy and 'error:' not in clippy and 'warning:' not in clippy
(out / 'clippy.log').write_text('cargo clippy -p kasu-cache -p kasu-sim --all-targets --locked -- -D warnings: passed\n')
paths = set(required) | {'Cargo.toml', '.dockerignore', '.github/workflows/land-sim.yml',
    'tools/cache-sim/Dockerfile', 'tools/cache-sim/run.sh', 'tools/cache-sim/record.py'}
for directory in (root / 'crates').iterdir():
    if directory.is_dir():
        paths.add(str((directory / 'Cargo.toml').relative_to(root)))
        paths.update(str(p.relative_to(root)) for p in (directory / 'src').rglob('*.rs'))
        paths.update(str(p.relative_to(root)) for p in (directory / 'tests').rglob('*.rs'))
        paths.update(str(p.relative_to(root)) for p in directory.rglob('*.proto'))
summary = {
    'success': True, 'captured_at': datetime.now(ZoneInfo('Asia/Tokyo')).isoformat(),
    'clients_per_run': 1000, 'seeds': [0, 1, 42, 2026], 'repeats_per_platform': 2,
    'macos_linux_traces_identical': True, 'virtual_time_ms': 170, 'result_digests': results,
    'clock_regression_reproduced_before_fix': True,
    'macos_cache_daemon_cli_sim_tests_passed': 50,
    'verified': ['production cache Layer and Objects Client/handler with in-memory HTTP/2',
        'two node DiskStores and one regional memory cache; 1000 requests coalesce to one authority call',
        'cache unavailable: cached and cold reads fail closed without authoritative identity confirmation',
        'empty cache restart, incomplete-frame rejection and recovery',
        'virtual upstream timeout; no partial cache publication',
        'persistence failures, blocked 30-request burst, failed probe and recovery',
        'daemon DiskStore reopen returns retained frames without an authority call',
        'same seed repeated exactly; seeds 0/1 are distinct and request count binds the digest',
        'macOS and pinned Rust 1.90 Linux traces match byte-for-byte',
        'existing real TCP cache layer tests pass on both platforms'],
    'not_verified': ['OS packet loss or actual process kill within the deterministic harness',
        'real ENOSPC/power loss; Store::put failures are injected',
        'all fault orderings, network topology sizes or production latency SLA',
        'GitHub workflow execution'],
    'source_sha256': {p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in sorted(paths)},
}
(out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps({'seeds': 4, 'runs': 16, 'virtual_ms': 170, 'authority_calls_per_full_scenario': 11}))
