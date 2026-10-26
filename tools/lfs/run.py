#!/usr/bin/env python3
"""Evaluate external Git LFS with real Kasu processes and a test-only LFS peer.

The in-memory peer implements only Basic batch/upload/download for this fixture;
it is neither a production server nor a Kasu endpoint.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def evaluate(server_binary, ingest_binary, output):
    output.mkdir(parents=True, exist_ok=True)
    events = []
    objects = {}
    lock = threading.Lock()
    offline = threading.Event()

    class Peer(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def reply(self, status, body, kind='application/vnd.git-lfs+json'):
            body = json.dumps(body).encode() if isinstance(body, dict) else body
            self.send_response(status)
            self.send_header('Content-Type', kind)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            if offline.is_set():
                return self.reply(503, {'message': 'fixture offline'})
            if self.path != '/lfs/objects/batch':
                return self.reply(404, {'message': 'unknown fixture route'})
            request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            op = request['operation']
            assert op in ['upload', 'download']
            result = []
            with lock:
                for obj in request['objects']:
                    oid, size = obj['oid'], obj['size']
                    assert len(oid) == 64 and all(c in '0123456789abcdef' for c in oid)
                    entry = {'oid': oid, 'size': size, 'authenticated': True}
                    events.append({'method': 'batch', 'operation': op, 'oid': oid, 'size': size})
                    if op == 'download' and oid not in objects:
                        entry['error'] = {'code': 404, 'message': 'missing fixture object'}
                    elif op == 'download' or oid not in objects:
                        entry['actions'] = {op: {'href': f'{lfs_url}/objects/{oid}'}}
                    result.append(entry)
            self.reply(200, {'transfer': 'basic', 'objects': result})

        def do_PUT(self):
            oid = self.path.rsplit('/', 1)[-1]
            body = self.rfile.read(int(self.headers['Content-Length']))
            assert hashlib.sha256(body).hexdigest() == oid
            with lock:
                objects[oid] = body
                events.append({'method': 'upload', 'oid': oid, 'bytes': len(body)})
            self.reply(200, b'')

        def do_GET(self):
            oid = self.path.rsplit('/', 1)[-1]
            with lock:
                body = objects.get(oid)
                if body is not None:
                    events.append({'method': 'download', 'oid': oid, 'bytes': len(body)})
            if body is None:
                return self.reply(404, {'message': 'missing fixture object'})
            self.reply(200, body, 'application/octet-stream')

    peer = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Peer)
    lfs_url = f'http://127.0.0.1:{peer.server_port}/lfs'
    thread = threading.Thread(target=peer.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix='kasu-lfs-') as tmp:
            root = Path(tmp)
            env = {'PATH': os.environ['PATH'], 'HOME': str(root), 'LC_ALL': 'C',
                   'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': str(root / 'gitconfig'),
                   'GIT_TERMINAL_PROMPT': '0', 'GIT_AUTHOR_NAME': 'LFS Fixture',
                   'GIT_AUTHOR_EMAIL': 'lfs@example.invalid', 'GIT_COMMITTER_NAME': 'LFS Fixture',
                   'GIT_COMMITTER_EMAIL': 'lfs@example.invalid',
                   'GIT_AUTHOR_DATE': '2025-01-01T00:00:00Z',
                   'GIT_COMMITTER_DATE': '2025-01-01T00:00:00Z'}
            commands = []

            def run(argv, cwd=None, extra=None, ok=True):
                result = subprocess.run([str(a) for a in argv], cwd=cwd or root,
                                        env=env | (extra or {}), capture_output=True, timeout=60)
                commands.append({'argv': [str(a).replace(str(root), '<work>').replace(lfs_url, '<lfs>')
                                          for a in argv], 'exit': result.returncode})
                if ok and result.returncode:
                    raise RuntimeError(f'{argv}: {result.stderr.decode(errors="replace")}')
                return result

            def git(cwd, *args, **kw):
                return run(['git', *args], cwd, **kw)

            git(root, 'lfs', 'install', '--skip-repo')
            versions = {'git': git(root, '--version').stdout.decode().strip(),
                        'git_lfs': git(root, 'lfs', 'version').stdout.decode().strip()}
            source = root / 'source'
            git(root, 'init', '-b', 'main', source)
            git(source, 'lfs', 'track', '*.bin')
            (source / '.lfsconfig').write_text(f'[lfs]\n\turl = {lfs_url}\n')
            payload = hashlib.shake_256(b'kasu external lfs evaluation v1').digest(2 * 1024 * 1024)
            (source / 'asset.bin').write_bytes(payload)
            git(source, 'add', '.')
            git(source, 'commit', '-m', 'initial LFS pointer')
            initial_pointer = git(source, 'show', 'HEAD:asset.bin').stdout
            oid = hashlib.sha256(payload).hexdigest()
            expected = f'version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {len(payload)}\n'.encode()
            assert initial_pointer == expected
            (output / 'initial.pointer').write_bytes(initial_pointer)
            run([ingest_binary, source, root / 'store', 'repo'])
            grpc, http_port = port(), port()
            endpoint = f'http://127.0.0.1:{http_port}'
            remote = f'{endpoint}/repo.git'
            with (output / 'server.log').open('wb') as log:
                process = subprocess.Popen([server_binary, str(root / 'store'), 'repo',
                                            f'127.0.0.1:{grpc}', f'127.0.0.1:{http_port}'],
                                           env=env, stdout=log, stderr=log)
                try:
                    for _ in range(200):
                        if process.poll() is not None:
                            raise RuntimeError('Kasu server exited before readiness')
                        try:
                            urllib.request.urlopen(endpoint + '/metrics', timeout=.2).close()
                            break
                        except (OSError, urllib.error.URLError):
                            time.sleep(.05)
                    else:
                        raise RuntimeError('Kasu server readiness deadline')
                    git(source, 'remote', 'add', 'origin', remote)
                    # Importing a pointer does not move its external payload.
                    missing = root / 'missing'
                    failed = git(root, 'clone', remote, missing, ok=False)
                    assert failed.returncode != 0
                    assert b'Smudge error' in failed.stderr or b'smudge filter' in failed.stderr
                    git(missing, 'fsck', '--strict')
                    assert git(missing, 'show', 'HEAD:asset.bin').stdout == initial_pointer
                    (output / 'missing-object.log').write_bytes(failed.stderr)
                    # Standard LFS upload populates the separate service; a fresh clone hydrates.
                    git(source, 'lfs', 'push', '--all', 'origin')
                    assert objects[oid] == payload
                    cases = []
                    for name, flags in [('full', []), ('depth1', ['--depth=1']),
                                        ('partial', ['--filter=blob:none'])]:
                        target = root / name
                        git(root, 'clone', *flags, remote, target)
                        assert (target / 'asset.bin').read_bytes() == payload
                        assert git(target, 'show', 'HEAD:asset.bin').stdout == initial_pointer
                        git(target, 'lfs', 'fsck')
                        git(target, 'fsck', '--strict')
                        cases.append({'mode': name, 'hydrated_bytes': len(payload), 'strict_fsck': True})
                    # pre-push uploads the replacement first, then publishes its pointer to Kasu.
                    target = root / 'full'
                    changed = payload + b'\nfrom LFS push\n'
                    (target / 'asset.bin').write_bytes(changed)
                    git(target, 'add', 'asset.bin')
                    git(target, 'commit', '-m', 'LFS writeback')
                    new_pointer = git(target, 'show', 'HEAD:asset.bin').stdout
                    new_oid = hashlib.sha256(changed).hexdigest()
                    git(target, 'push', 'origin', 'main')
                    assert objects[new_oid] == changed
                    head = git(target, 'rev-parse', 'HEAD').stdout.decode().strip()
                    shallow = root / 'depth1'
                    git(shallow, 'pull', '--ff-only')
                    assert (shallow / 'asset.bin').read_bytes() == changed
                    git(shallow, 'lfs', 'fsck')
                    git(shallow, 'fsck', '--strict')
                    (output / 'pushed.pointer').write_bytes(new_pointer)

                    def api(route, body):
                        req = urllib.request.Request(endpoint + route, json.dumps(body).encode(),
                                                     {'Content-Type': 'application/json'})
                        with urllib.request.urlopen(req, timeout=5) as response:
                            return json.load(response)

                    ref = api('/v1/resolve-ref', {'repo': list(b'repo'), 'name': list(b'refs/heads/main')})
                    content = api('/v1/read-file', {'repo': list(b'repo'), 'id': ref['target'],
                                                  'path': list(b'asset.bin')})
                    assert bytes(content['content']) == new_pointer
                    # Kasu validates Git connectivity, not existence in the external LFS store.
                    absent_oid = hashlib.sha256(b'absent external fixture payload').hexdigest()
                    absent_pointer = f'version https://git-lfs.github.com/spec/v1\noid sha256:{absent_oid}\nsize 31\n'.encode()
                    git(target, 'checkout', '-b', 'missing-lfs')
                    (target / 'asset.bin').write_bytes(absent_pointer)
                    git(target, 'add', 'asset.bin')
                    git(target, 'commit', '-m', 'pointer without LFS upload')
                    git(target, 'push', '--no-verify', 'origin', 'missing-lfs')
                    assert absent_oid not in objects
                    pointer_only = root / 'pointer-only'
                    git(root, 'clone', '--branch', 'missing-lfs', remote, pointer_only,
                        extra={'GIT_LFS_SKIP_SMUDGE': '1'})
                    assert (pointer_only / 'asset.bin').read_bytes() == absent_pointer
                    git(pointer_only, 'fsck', '--strict')
                    # LFS outages affect smudge, not the validity of the Git pointer history.
                    offline.set()
                    skip = root / 'skip'
                    git(root, 'clone', remote, skip, extra={'GIT_LFS_SKIP_SMUDGE': '1'})
                    assert (skip / 'asset.bin').read_bytes() == new_pointer
                    git(skip, 'fsck', '--strict')
                    failed = git(skip, '-c', 'lfs.transfer.maxretries=0', 'lfs', 'pull', ok=False)
                    assert failed.returncode != 0
                    (output / 'outage.log').write_bytes(failed.stderr)
                    req = urllib.request.Request(remote + '/info/lfs/objects/batch',
                                                 b'{"operation":"download","objects":[]}',
                                                 {'Content-Type': 'application/vnd.git-lfs+json'})
                    try:
                        urllib.request.urlopen(req, timeout=5)
                        raise AssertionError('Kasu unexpectedly exposes LFS batch')
                    except urllib.error.HTTPError as error:
                        assert error.code == 404
                    uploads = [e for e in events if e['method'] == 'upload']
                    assert len(uploads) == 2
                    summary = {'success': True, 'versions': versions, 'clone_cases': cases,
                               'payload_bytes': len(payload), 'pointer_bytes': len(initial_pointer),
                               'payload_sha256': oid, 'pushed_payload_sha256': new_oid,
                               'pushed_git_oid': head, 'native_returns_pointer': True,
                               'missing_payload_checkout_fails': True, 'git_fsck_without_lfs': True,
                               'lfs_outage_skip_smudge_clone': True, 'lfs_outage_pull_fails': True,
                               'missing_payload_push_accepted': True,
                               'kasu_lfs_batch_status': 404, 'external_upload_count': len(uploads),
                               'security_validation': 'not performed; loopback fixture without auth'}
                    (output / 'result.json').write_text(json.dumps(summary, indent=2) + '\n')
                    print(json.dumps(summary, indent=2))
                finally:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            (output / 'requests.json').write_text(json.dumps(events, indent=2) + '\n')
            text = json.dumps(commands, indent=2).replace(endpoint, '<kasu>')
            (output / 'commands.json').write_text(text + '\n')
    finally:
        peer.shutdown()
        peer.server_close()
        thread.join(timeout=5)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--server', default='/out/kasu-server')
    parser.add_argument('--ingest', default='/out/kasu-ingest')
    parser.add_argument('--output', type=Path, default=Path('/evidence'))
    args = parser.parse_args()
    evaluate(args.server, args.ingest, args.output)
