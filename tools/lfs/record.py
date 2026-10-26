#!/usr/bin/env python3
"""Record successful external-LFS evaluation evidence."""
import hashlib
import json
from pathlib import Path
import re
import sys

root = Path(__file__).resolve().parents[2]
source = Path(sys.argv[1])
out = root / 'crates/kasu-git/tests/fixtures/clients/lfs'
result = json.loads((source / 'result.json').read_text())
assert result['success'] and result['native_returns_pointer']
assert result['missing_payload_push_accepted'] and result['kasu_lfs_batch_status'] == 404
assert len(result['clone_cases']) == 3 and all(c['strict_fsck'] for c in result['clone_cases'])
requests = json.loads((source / 'requests.json').read_text())
assert len([r for r in requests if r['method'] == 'upload']) == 2
out.mkdir(parents=True, exist_ok=True)
for name in ['result.json', 'requests.json', 'commands.json', 'initial.pointer', 'pushed.pointer',
             'missing-object.log', 'outage.log']:
    text = (source / name).read_text()
    text = re.sub(r'http://127\.0\.0\.1:\d+/lfs', '<lfs>', text)
    text = re.sub(r'http://127\.0\.0\.1:\d+', '<kasu>', text)
    text = re.sub(r'/tmp/kasu-lfs-[^/\s"\']+', '<work>', text)
    text = re.sub(r'\d{8}T\d{6}\.\d+', '<timestamp>', text)
    (out / name).write_text(text)
paths = ['tools/lfs/run.py', 'tools/lfs/Dockerfile', 'tools/lfs/record.py',
         '.github/workflows/lfs.yml', 'Cargo.lock', 'crates/kasu-server/src/backend.rs',
         'crates/kasu-server/src/receive.rs', 'crates/kasu-server/src/git_http.rs',
         'crates/kasu-ingest/src/main.rs']
summary = {'captured_date': '2026-10-24',
           'source_sha256': {p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in paths},
           'scope': 'Linux arm64 real Git LFS client; separate in-memory Basic LFS peer; real Kasu binaries',
           'not_verified': ['production LFS service', 'TLS and authentication', 'SSH plus LFS',
                            'projection plus LFS', 'large-file capacity and performance', 'GitHub workflow']}
(out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
