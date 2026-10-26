#!/usr/bin/env python3
"""Disposable TiKV/PostgreSQL evaluation with real replica and PD failures."""
import argparse
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import time

root = pathlib.Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser()
parser.add_argument('--workdir', type=pathlib.Path)
parser.add_argument('--operations', type=int, default=256)
parser.add_argument('--keep', action='store_true')
args = parser.parse_args()
if not 8 <= args.operations <= 4096:
    parser.error('operations must be 8..4096')
work = args.workdir.resolve() if args.workdir else pathlib.Path(tempfile.mkdtemp(prefix='kasu-kv-eval-'))
work.mkdir(parents=True, exist_ok=True)
project = f'kasu-kv-eval-{os.getpid()}'
namespace = f'run-{time.time_ns():x}'
compose = ['docker', 'compose', '-p', project, '-f', str(root / 'tools/kv-eval/compose.yml')]
busybox = 'busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e'
python = 'python@sha256:c6ead215bfd31f1e433d968853b7a769989117115b728874824e6c0a27cb96fc'
start_time = time.monotonic()
summary = {'schema': 1, 'runId': project, 'namespace': namespace, 'operations': args.operations, 'allChecksPassed': False}
(work / 'summary.json').write_text(json.dumps(summary) + '\n')
started = False

def command(argv, name, env=None, timeout=240):
    with (work / f'{name}.log').open('w') as log:
        result = subprocess.run(argv, cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT, timeout=timeout)
    if result.returncode:
        print((work / f'{name}.log').read_text()[-12000:], file=sys.stderr)
        raise RuntimeError(f'{name} failed ({result.returncode})')

def inspect():
    argv = ['docker', 'run', '--rm', '--network', f'{project}_default', '--label', f'kasu-kv-eval={project}', '--mount', f'type=bind,source={root / "tools/kv-eval/pd_state.py"},target=/pd_state.py,readonly', python, 'python3', '/pd_state.py']
    result = subprocess.run(argv, capture_output=True, text=True, timeout=15)
    return json.loads(result.stdout) if result.returncode == 0 else None

def wait_for(predicate, name, timeout=120):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        data = inspect()
        if data and predicate(data):
            (work / f'{name}.json').write_text(json.dumps(data, indent=2) + '\n')
            return data
        time.sleep(.5)
    raise RuntimeError(f'{name} never became ready')

def phase(backend, stage, input_file=None):
    name = f'{backend}-{stage}'
    argv = ['docker', 'run', '--rm', '--name', f'{project}-{name}', '--label', f'kasu-kv-eval={project}', '--network', f'{project}_default', '--mount', f'type=bind,source={work / "kv-eval"},target=/kv-eval,readonly', '--mount', f'type=bind,source={work},target=/evidence', '-e', 'KASU_EVAL_PD=pd1:2379,pd2:2379,pd3:2379', '-e', 'KASU_EVAL_POSTGRES_DSN=postgres://kasu:kasu-evaluation-only@postgres:5432/kasu?sslmode=disable', '--entrypoint', '/kv-eval', busybox, '--backend', backend, '--phase', stage, '--namespace', namespace, '--operations', str(args.operations), '--output', f'/evidence/{name}.json']
    if input_file:
        argv.extend(['--input', f'/evidence/{input_file}'])
    print(f'Running {name}', flush=True)
    command(argv, name)
    result = json.loads((work / f'{name}.json').read_text())
    if not result.get('allChecksPassed') or result.get('namespace') != namespace or result.get('phase') != stage:
        raise RuntimeError(f'{name} did not prove its checks')
    return result

def store_names(data):
    out = {}
    for entry in data['stores']:
        store = entry['store']
        name = store['address'].split(':')[0]
        if not re.fullmatch(r'tikv[123]', name):
            raise RuntimeError('unexpected store address')
        out[store['id']] = name
    return out

def healthy_regions(data, ids, stopped=frozenset()):
    regions = {r['id']: r for r in data['regions']}
    names = store_names(data)
    for key in ids:
        if key not in regions:
            return False
        region = regions[key]
        leader = (region.get('leader') or {}).get('store_id')
        if len(region.get('peers', [])) != 3 or leader in stopped or not leader:
            return False
        if not data['alive'].get(names.get(leader)):
            return False
        if sum(data['alive'].get(names.get(peer['store_id']),False) for peer in region['peers']) < 2:
            return False
    return True

try:
    print(f'Evaluation evidence: {work}', flush=True)
    memory = int(subprocess.check_output(['docker','info','--format','{{.MemTotal}}'],text=True).strip())
    if memory < 10 * 1024**3:
        raise RuntimeError('three-replica evaluation needs at least 10 GiB Docker memory; 12 GB or more is recommended')
    summary['dockerMemoryBytes'] = memory
    arch = subprocess.check_output(['docker', 'info', '--format', '{{.Architecture}}'], text=True).strip()
    arch = {'aarch64': 'arm64', 'arm64': 'arm64', 'x86_64': 'amd64', 'amd64': 'amd64'}[arch]
    env = dict(os.environ, GOOS='linux', GOARCH=arch, CGO_ENABLED='0')
    command(['go', '-C', 'tools/kv-eval', 'build', '-mod=readonly', '-o', str(work / 'kv-eval'), '.'], 'build', env)
    command(['go', '-C', 'tools/kv-eval', 'vet', './...'], 'vet')
    command(['go', '-C', 'tools/kv-eval', 'run', 'golang.org/x/vuln/cmd/govulncheck@v1.9.0', './...'], 'govulncheck')
    summary['goVersion'] = subprocess.check_output(['go', 'version'], text=True).strip()
    summary['architecture'] = arch
    summary['binarySha256'] = hashlib.sha256((work / 'kv-eval').read_bytes()).hexdigest()
    summary['sourcesSha256'] = {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root / 'tools/kv-eval').iterdir()) if p.is_file()}
    print('Starting three PD and three TiKV replicas plus PostgreSQL', flush=True)
    started = True
    command(compose + ['up', '-d'], 'up')
    ready = wait_for(lambda d: len(d['stores']) == 3 and len(d['regions']) > 0 and all(d['alive'].values()) and all(len(r.get('peers', [])) == 3 for r in d['regions']), 'initial-topology')
    # PostgreSQL health and all three placement-driver names are independently checked.
    command(compose + ['exec', '-T', 'postgres', 'pg_isready', '-U', 'kasu', '-d', 'kasu'], 'pg-ready')
    pg = phase('postgres', 'baseline')
    kv = phase('tikv', 'baseline')
    region_ids = {entry['ID'] for entry in kv['regions'].values()}
    current = wait_for(lambda d: healthy_regions(d, region_ids), 'before-faults')
    command(['docker', 'stats', '--no-stream', '--format', '{{json .}}'], 'resource-sample')
    names = store_names(current)
    failed_id = kv['faultRegion']['LeaderStore']
    failed_node = names[failed_id]
    print(f'Killing data leader {failed_node}', flush=True)
    t = time.monotonic()
    command(compose + ['kill', '-s', 'SIGKILL', failed_node], 'kill-data-leader')
    wait_for(lambda d: healthy_regions(d, region_ids, {failed_id}), 'after-data-leader-kill')
    summary['dataLeaderReplacementMS'] = round((time.monotonic() - t) * 1000, 3)
    phase('tikv', 'one-node-down')
    command(compose + ['start', failed_node], 'start-data-leader')
    wait_for(lambda d: healthy_regions(d, region_ids), 'data-recovered')
    current = inspect()
    pd_leader = current['leader']['name']
    if not re.fullmatch(r'pd[123]', pd_leader):
        raise RuntimeError('unexpected PD leader')
    print(f'Killing PD leader {pd_leader}', flush=True)
    t = time.monotonic()
    command(compose + ['kill', '-s', 'SIGKILL', pd_leader], 'kill-pd-leader')
    wait_for(lambda d: d['leader']['name'] != pd_leader, 'after-pd-leader-kill')
    summary['pdLeaderReplacementMS'] = round((time.monotonic() - t) * 1000, 3)
    phase('tikv', 'pd-down')
    command(compose + ['start', pd_leader], 'start-pd-leader')
    phase('tikv', 'prepare-outage')
    current = inspect()
    region = next(r for r in current['regions'] if r['id'] == kv['faultRegion']['ID'])
    current_leader = region['leader']['store_id']
    second = next(key for key in names if key != current_leader)
    print('Removing the data quorum; every request keeps a finite deadline', flush=True)
    command(compose + ['kill', '-s', 'SIGKILL', names[current_leader], names[second]], 'kill-data-majority')
    phase('tikv', 'outage', 'tikv-prepare-outage.json')
    command(compose + ['start', names[current_leader], names[second]], 'restore-data-majority')
    wait_for(lambda d: healthy_regions(d, region_ids), 'quorum-restored')
    phase('tikv', 'recover', 'tikv-prepare-outage.json')
    print('Restarting every data and control process; rechecking all accepted data', flush=True)
    command(compose + ['restart'], 'restart-all')
    wait_for(lambda d: healthy_regions(d, region_ids), 'after-restart')
    phase('tikv', 'verify', 'tikv-baseline.json')
    phase('postgres', 'verify-baseline', 'postgres-baseline.json')
    summary.update({'allChecksPassed': True, 'elapsedSeconds': round(time.monotonic() - start_time, 3), 'pdProcesses': 3, 'tikvProcesses': 3, 'replicasPerRegion': 3, 'crossRegionAtomicity': True, 'dataLeaderSIGKILL': failed_node, 'pdLeaderSIGKILL': pd_leader, 'majorityLossBoundedWithoutAcknowledgement': True, 'sameKeyRecoveredWithoutDuplicate': True, 'allAcceptedDataSurvivedRestart': True, 'audit': 'govulncheck v1.9.0: no vulnerabilities found'})
    (work / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({key: value for key, value in summary.items() if key != 'sourcesSha256'}), flush=True)
finally:
    if started and not args.keep:
        # docker run jobs are outside Compose; stop only jobs labelled for this run.
        ids = subprocess.check_output(['docker', 'ps', '-aq', '--filter', f'label=kasu-kv-eval={project}'], text=True).split()
        if ids:
            subprocess.run(['docker', 'rm', '-f'] + ids, check=False, stdout=subprocess.DEVNULL)
        command(compose + ['logs', '--no-color', '--tail', '150'], 'services')
        command(compose + ['down', '--volumes', '--remove-orphans'], 'down')
    elif started:
        print(f'Retained project {project}; use docker compose -p {project} -f tools/kv-eval/compose.yml down --volumes', flush=True)
