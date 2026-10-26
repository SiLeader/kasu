#!/usr/bin/env python3
"""Validate and record complete FirstParentSkip v2 acceptance captures."""
import argparse
import csv
import hashlib
import io
import json
from pathlib import Path
import re

parser = argparse.ArgumentParser()
for key in ['macos-csv', 'linux-csv', 'macos-log', 'linux-log', 'macos-tests', 'linux-build', 'server-tests']:
    parser.add_argument('--' + key, type=Path, required=True)
parser.add_argument('--derived-tests', type=Path, required=True)
parser.add_argument('--cli-tests', type=Path, required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
out = root / 'docs/benchmarks/first-parent-skip-v2'
out.mkdir(parents=True, exist_ok=True)
summary = {'captured_date': '2026-10-24', 'first_parent_skip_version': 2, 'platforms': {}, 'scope': {
    'changesets': 100000, 'divergent_merges': 10000, 'side_commits': 10000,
    'samples_per_row': 1000, 'timed': 'Operations::create_changeset including eager derived data',
    'untimed': 'source construction, fork/ancestry/merge-base/content assertions and reporting',
    'counts': 'MetaStore/BlobStore API calls, not physical disk I/O',
    'validation': '16 kasu-ops and 5 selected kasu-derived tests per OS; 16 server and 12 CLI tests on macOS',
    'exhaustive_dags': 1024, 'exhaustive_tip_pairs': 25600,
    'cold_merge_base_metadata_read_bound': 32,
    'new_skip_index_get_calls_per_create_max': 7,
    'new_skip_index_record_bytes_max': 82,
    'v1_records': 'preserved and lazy; v2 uses a separate key space',
}, 'limits': ['one run per OS, no throughput or strict constant-time guarantee',
              'small fixed-width tree with two reused blob contents',
              'local SQLite WAL/FULL and filesystem; host and container storage differ',
              'some build/test activity overlapped; no timing gate',
              'SQLite lookup/put costs still depend on storage size; no physical-latency guarantee',
              'legacy histories need a one-time v2 first-parent backfill',
              'GitHub workflow execution unverified']}
for platform in ['macos', 'linux']:
    raw = getattr(args, platform + '_csv').read_text()
    rows = list(csv.DictReader(io.StringIO(raw)))
    assert len(rows) == 100
    for n, row in enumerate(rows, 1):
        assert int(row['history_length']) == n * 1000
        assert int(row['merges']) == n * 100
        assert all(float(row[key]) > 0 for key in row if key != 'merges')
    assert max(int(row['max_meta_get_calls']) for row in rows) <= 27
    log = getattr(args, platform + '_log').read_text()
    match = re.search(r'history_count=100000 divergent_merges=10000 tip_changeset=([0-9a-f]{64}) root_tree=([0-9a-f]{64})', log)
    assert match, log
    (out / (platform + '.csv')).write_text(raw)
    (out / (platform + '.log')).write_text(match.group(0) + '\n')
    blocks = {}
    for label, start, end in [('first_10000', 0, 10), ('middle_10000', 45, 55), ('last_10000', 90, 100)]:
        block = rows[start:end]
        blocks[label] = {key: round(sum(float(r[key]) for r in block)/len(block), 3)
                         for key in rows[0] if key not in ['history_length', 'merges', 'max_meta_get_calls']}
        blocks[label]['max_meta_get_calls'] = max(int(r['max_meta_get_calls']) for r in block)
    summary['platforms'][platform] = {'tip_changeset': match[1], 'root_tree': match[2],
                                      'max_meta_get_calls': max(int(r['max_meta_get_calls']) for r in rows),
                                      'blocks': blocks}
assert summary['platforms']['macos']['tip_changeset'] == summary['platforms']['linux']['tip_changeset']
assert summary['platforms']['macos']['root_tree'] == summary['platforms']['linux']['root_tree']
expected_tests = {'macos_tests': 16, 'linux_build': 21, 'server_tests': 16, 'derived_tests': 5, 'cli_tests': 12}
for name in expected_tests:
    text = getattr(args, name).read_text()
    assert 'FAILED' not in text and 'error:' not in text
    passed = sum(map(int, re.findall(r'test result: ok\. (\d+) passed; 0 failed;', text)))
    assert passed == expected_tests[name], (name, passed)
    if name in ['macos_tests', 'linux_build']:
        for test in ['eager_skip_index_has_fixed_reads_and_bytes_and_queries_skip_long_histories',
                     'v1_records_survive_reopen_and_v2_backfill_without_changing_changesets',
                     'pruned_merge_bases_match_all_five_node_dags_and_tip_pairs']:
            assert re.search(r'test ' + test + r' \.\.\. ok', text), (name, test)
    if name != 'linux_build':
        text = re.sub(r'^.*(?:Compiling |Finished |Blocking waiting for file lock).*\n', '', text, flags=re.M)
        text = re.sub(r'^\s*Running (.*?) \(.*?\)\s*$', r'Running \1', text, flags=re.M)
        (out / (name.replace('_', '-') + '.log')).write_text(text)
    else:
        assert 'rustc 1.90.0' in text
        for digest, path in re.findall(r'([0-9a-f]{64})  (crates/[^\s]+)', text):
            assert hashlib.sha256((root / path).read_bytes()).hexdigest() == digest, path
        selected = [line for line in text.splitlines() if re.search(r'rustc |Linux aarch64|test .* \.\.\. ok|test result:|[0-9a-f]{64}  crates/', line)]
        (out / 'linux-build.log').write_text('\n'.join(selected) + '\n')
paths = ['Cargo.lock', 'crates/kasu-ops/Cargo.toml', 'crates/kasu-ops/src/lib.rs',
         'crates/kasu-ops/tests/operations.rs', 'crates/kasu-ops/benches/divergent_history.rs',
         'crates/kasu-derived/src/types.rs', 'crates/kasu-derived/src/scheduler.rs',
         'crates/kasu-derived/tests/derived.rs', 'crates/kasu-ops/src/metadata.rs',
         'crates/kasu-ops/tests/first_parent.rs', 'tools/history-benchmark/Dockerfile',
         'tools/history-benchmark/record.py', '.github/workflows/history-benchmark.yml']
summary['source_sha256'] = {p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in paths}
baseline = json.loads((root / 'docs/benchmarks/divergent-history/summary.json').read_text())
for platform in ['macos', 'linux']:
    assert summary['platforms'][platform]['tip_changeset'] == baseline['platforms'][platform]['tip_changeset']
    assert summary['platforms'][platform]['root_tree'] == baseline['platforms'][platform]['root_tree']
summary['baseline'] = {'path': '../divergent-history/summary.json', 'same_final_changeset_and_tree': True,
                       'v1_meta_get_max': 43, 'v2_meta_get_max': 27}
(out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
