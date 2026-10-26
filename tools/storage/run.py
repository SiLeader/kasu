#!/usr/bin/env python3
"""Disposable, pinned PostgreSQL/S3 acceptance environment. No cloud account."""
import argparse
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import time

root = pathlib.Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser()
parser.add_argument('--linux', action='store_true', help='run precompiled tests in fixed Rust 1.90 Linux')
parser.add_argument('--keep', action='store_true', help='retain this run\'s containers for inspection')
parser.add_argument('--workdir', type=pathlib.Path)
args = parser.parse_args()
work = args.workdir.resolve() if args.workdir else pathlib.Path(tempfile.mkdtemp(prefix='kasu-storage-'))
work.mkdir(parents=True, exist_ok=True)
project = f'kasu-storage-{os.getpid()}'
compose = ['docker', 'compose', '-p', project, '-f', str(root / 'tools/storage/compose.yml')]
pg_port = os.environ.get('KASU_TEST_PG_PORT', '55432')
s3_port = os.environ.get('KASU_TEST_S3_PORT', '58333')
base_env = {key: value for key, value in os.environ.items() if not key.startswith('AWS_')}
base_env['RUSTC_WRAPPER'] = ''
started = time.monotonic()
(work / 'summary.json').write_text(json.dumps({'schema': 1, 'runId': project, 'allTestsPassed': False, 'status': 'running'}) + '\n')

def run(argv, name, env=None, timeout=None):
    with (work / f'{name}.log').open('w') as log:
        result = subprocess.run(argv, cwd=root, env=env or base_env, stdout=log, stderr=subprocess.STDOUT, timeout=timeout)
    if result.returncode:
        print((work / f'{name}.log').read_text()[-10000:], file=sys.stderr)
        raise RuntimeError(f'{name} failed ({result.returncode})')

try:
    print(f'Starting pinned storage services; evidence: {work}', flush=True)
    run(compose + ['up', '-d', '--wait', '--wait-timeout', '60'], 'compose-up')
    url = f'http://127.0.0.1:{s3_port}/kasu-test'
    for attempt in range(60):
        result = subprocess.run(['curl', '--silent', '--show-error', '--fail', '--max-time', '2', '--aws-sigv4', 'aws:amz:us-east-1:s3', '--user', 'kasu-test:kasu-local-test-secret', '-X', 'PUT', url], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if result.returncode == 0:
            break
        time.sleep(.25)
    else:
        raise RuntimeError('S3 never became ready')
    result = subprocess.run(['curl', '--silent', '--max-time', '3', '-o', os.devnull, '-w', '%{http_code}', url], capture_output=True, text=True, check=True)
    if result.stdout != '403':
        raise RuntimeError(f'anonymous S3 request was not denied: {result.stdout}')
    # The default collection grows seven volumes at once. Leave sufficient room
    # for it and use single-volume growth for the test bucket.
    run(compose + ['exec', '-T', 's3', 'sh', '-c', 'printf "fs.configure -locationPrefix=/buckets/ -volumeGrowthCount=1 -apply\\n" | weed shell -master=127.0.0.1:9333 -filer=127.0.0.1:8888'], 's3-growth', timeout=45)
    tls_script = '''set -eu
openssl req -x509 -newkey rsa:2048 -nodes -keyout /tmp/kasu-test-ca.key -out /tmp/kasu-test-ca.crt -days 2 -subj /CN=kasu-test-ca
openssl req -newkey rsa:2048 -nodes -keyout /tmp/kasu-test-server.key -out /tmp/kasu-test-server.csr -subj /CN=localhost
printf '%s\\n' 'subjectAltName=DNS:localhost,DNS:postgres,IP:127.0.0.1' 'basicConstraints=CA:FALSE' 'keyUsage=digitalSignature,keyEncipherment' 'extendedKeyUsage=serverAuth' >/tmp/kasu-test-server.ext
openssl x509 -req -in /tmp/kasu-test-server.csr -CA /tmp/kasu-test-ca.crt -CAkey /tmp/kasu-test-ca.key -CAcreateserial -out /tmp/kasu-test-server.crt -days 2 -extfile /tmp/kasu-test-server.ext
chown postgres:postgres /tmp/kasu-test-server.key /tmp/kasu-test-server.crt
chmod 600 /tmp/kasu-test-server.key
'''
    run(compose + ['exec', '-T', 'postgres', 'sh', '-c', tls_script], 'tls-certificates')
    for name, sql in [('tls-on', "ALTER SYSTEM SET ssl TO 'on'"), ('tls-cert', "ALTER SYSTEM SET ssl_cert_file TO '/tmp/kasu-test-server.crt'"), ('tls-key', "ALTER SYSTEM SET ssl_key_file TO '/tmp/kasu-test-server.key'"), ('tls-reload', 'SELECT pg_reload_conf()')]:
        run(compose + ['exec', '-T', '-u', 'postgres', 'postgres', 'psql', '-v', 'ON_ERROR_STOP=1', '-U', 'kasu', '-d', 'kasu', '-c', sql], name)
    ca = work / 'test-ca.crt'
    run(compose + ['cp', 'postgres:/tmp/kasu-test-ca.crt', str(ca)], 'tls-copy')
    env = dict(base_env, AWS_ACCESS_KEY_ID='kasu-test', AWS_SECRET_ACCESS_KEY='kasu-local-test-secret', AWS_DEFAULT_REGION='us-east-1', KASU_TEST_S3_BUCKET='kasu-test')
    env.pop('AWS_SESSION_TOKEN', None)
    def configure(host, pg_port, s3_host, ca_file):
        return dict(env, KASU_TEST_POSTGRES=f'host={host} port={pg_port} user=kasu password=kasu-local-test dbname=kasu sslmode=disable', KASU_TEST_POSTGRES_TLS=f'host={host} port={pg_port} user=kasu password=kasu-local-test dbname=kasu sslmode=require', KASU_TEST_POSTGRES_CA=str(ca_file), KASU_TEST_S3_ENDPOINT=s3_host)
    if args.linux:
        print('Building and running fixed Rust 1.90 Linux tests', flush=True)
        run(['docker', 'build', '-f', 'tools/storage/Dockerfile', '-t', 'kasu:production-test', '.'], 'linux-build')
        container_env = configure('postgres', '5432', 'http://s3:8333', '/tmp/test-ca.crt')
        argv = ['docker', 'run', '--rm', '--network', f'{project}_default', '--mount', f'type=bind,source={ca},target=/tmp/test-ca.crt,readonly']
        for key in ['AWS_ACCESS_KEY_ID', 'AWS_SECRET_ACCESS_KEY', 'AWS_DEFAULT_REGION', 'KASU_TEST_S3_BUCKET', 'KASU_TEST_POSTGRES', 'KASU_TEST_POSTGRES_TLS', 'KASU_TEST_POSTGRES_CA', 'KASU_TEST_S3_ENDPOINT']:
            argv.extend(['-e', f'{key}={container_env[key]}'])
        run(argv + ['kasu:production-test'], 'tests')
    else:
        print('Running host storage contracts and two-server process tests', flush=True)
        run([sys.executable, 'tools/storage/check.py'], 'tests', configure('localhost', pg_port, f'http://127.0.0.1:{s3_port}', ca))
    summary = {'schema': 1, 'postgres': '18', 's3': 'SeaweedFS 4.45 / 79b872021', 'runtime': 'Rust 1.90 Linux' if args.linux else 'host', 'elapsedSeconds': round(time.monotonic() - started, 3), 'anonymousS3Rejected': True, 'tlsVerifiedAndUntrustedCaRejected': True, 'mixedCasClients': 16, 'landReplayClients': 8, 'conditionalPutClients': 16, 'sharedServerProcesses': 2, 'allTestsPassed': True}
    (work / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, ensure_ascii=False), flush=True)
finally:
    if not args.keep:
        run(compose + ['down', '--volumes', '--remove-orphans'], 'compose-down')
    else:
        print(f'Retained services: docker compose -p {project} -f tools/storage/compose.yml down --volumes', flush=True)
