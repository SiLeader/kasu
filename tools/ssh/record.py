#!/usr/bin/env python3
"""Record task-local SSH captures without Git timestamps or temporary paths."""
import argparse
import hashlib
import json
import pathlib
import re

parser = argparse.ArgumentParser()
parser.add_argument('--host', type=pathlib.Path, required=True)
parser.add_argument('--linux', type=pathlib.Path, required=True)
args = parser.parse_args()
root = pathlib.Path(__file__).resolve().parents[2]
out = root / 'crates/kasu-git/tests/fixtures/clients/ssh'
out.mkdir(parents=True, exist_ok=True)
for source, names in [(args.host, ['ssh-stdio.json', 'git-ssh-stdio.trace']),
                      (args.linux, ['ssh-openssh.json', 'git-ssh-openssh.trace',
                                    'go-git-ssh-depth0.trace', 'go-git-ssh-depth1.trace'])]:
    for name in names:
        data = (source / name).read_text()
        if name.endswith('.trace'):
            data = re.sub(r'^\d\d:\d\d:\d\d\.\d+\s+[^\n]*?packet:\s*', 'packet: ', data, flags=re.M)
            data = re.sub(r'/tmp/\.tmp[^\s"\']+', '<temporary-path>', data)
            data = re.sub(r'/var/folders/[^\s"\']+', '<temporary-path>', data)
        (out / name).write_text(data)
report = json.loads((out / 'ssh-openssh.json').read_text())
assert report['success'] and report['real_openssh'] and len(report['go_git']) == 2
assert report['repository_account_writes']
sources = ['crates/kasu-server/src/accounts.rs', 'crates/kasu-server/src/backend.rs',
           'crates/kasu-server/src/auth.rs', 'crates/kasu-server/src/receive.rs',
           'crates/kasu-server/src/write.rs', 'crates/kasu-server/src/write_transport.rs',
           'crates/kasu-server/src/land.rs', 'crates/kasu-server/tests/accounts.rs',
           'crates/kasu-server/Cargo.toml', 'crates/kasu-server/src/main.rs',
           'crates/kasu-server/src/configuration.rs', 'crates/kasu-server/src/ssh_main.rs',
           'crates/kasu-server/src/git_http.rs', 'crates/kasu-server/src/git_ssh.rs',
           'crates/kasu-server/src/git_ssh/pack_stream.rs', 'crates/kasu-server/tests/git_ssh.rs',
           'tools/git-compat/main.go', 'tools/ssh/Dockerfile', '.github/workflows/ssh.yml']
summary = {
    'captured_date': '2026-10-24',
    'source_sha256': {p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in sources},
    'scope': ['macOS stdio shim: Git 2.54.0 (no SSH encryption)',
              'Linux arm64: actual OpenSSH public-key authentication and pinned known_hosts',
              'Git protocols 0/1/2 full/depth1, shallow incremental fetch, deepen/unshallow',
              'Push, empty pack, deletion without pack, atomic branch/tag push',
              'Partial clone hydration, path projection and reader authorization',
              'go-git 5.19.3 full/depth1 clone/commit/push; native contents and OIDs',
              'Strict fsck, Linux/macOS idle session deadline',
              'Repository account forced key: allowed deploy push, forbidden path/ref/delete denial',
              'Transport and account unit tests on macOS and Linux',
              '49 server unit/HTTP/push/runtime/Writer/account regression tests on macOS'],
    'not_verified': ['GitHub workflow execution', 'production SSH deployment', 'security audit',
                     'shared cross-process SSH admission quota'],
}
(out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
