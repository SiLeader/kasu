#!/usr/bin/env python3
"""Dedicated kind acceptance test. Every kubectl call uses the private kubeconfig.

Use --reuse only for the cluster and workdir created for this test. Evidence is
kept separately from kubeconfig and only test-owned resources are changed.
"""
from __future__ import annotations
import argparse
import base64
import datetime
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import time

ROOT = Path(__file__).resolve().parents[2]
VERSIONS = json.loads((Path(__file__).parent / 'versions.json').read_text())
TOKEN = 'kasu-gitops-test-token'
HOOK_TOKEN = 'kasu-gitops-webhook-secret'
REPO_URL = 'http://kasu.gitops-test.svc.cluster.local:50080/demo.git'


def command(args, *, input=None, env=None, timeout=600, check=True):
    print('+ ' + ' '.join(map(str, args)), flush=True)
    result = subprocess.run(list(map(str, args)), input=input, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
    if check and result.returncode:
        raise RuntimeError(f'{args[0]} failed ({result.returncode}):\n{result.stdout.decode(errors="replace")}\n{result.stderr.decode(errors="replace")}')
    return result


class Test:
    def __init__(self, args):
        self.args = args
        self.repo_url = REPO_URL
        self.fixture_prefix = Path()
        self.work = Path(args.workdir).resolve()
        self.work.mkdir(parents=True, exist_ok=True)
        self.evidence = self.work / 'evidence'
        self.evidence.mkdir(exist_ok=True)
        self.kubeconfig = self.work / 'kubeconfig'
        self.vendor = self.work / 'vendor'
        self.vendor.mkdir(exist_ok=True)
        self.git_env = os.environ.copy()
        self.git_env.update(GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null',
                            GIT_AUTHOR_NAME='kasu GitOps E2E', GIT_AUTHOR_EMAIL='e2e@kasu.invalid',
                            GIT_COMMITTER_NAME='kasu GitOps E2E', GIT_COMMITTER_EMAIL='e2e@kasu.invalid',
                            GIT_AUTHOR_DATE='2026-10-23T00:00:00Z', GIT_COMMITTER_DATE='2026-10-23T00:00:00Z')

    def kubectl(self, *args, input=None, check=True, timeout=60):
        return command([self.args.kubectl, '--kubeconfig', self.kubeconfig, *args],
                       input=input, check=check, timeout=timeout)

    def get(self, kind, name=None, namespace=None):
        args = ['get', kind]
        if name:
            args.append(name)
        if namespace:
            args += ['-n', namespace]
        args += ['-o', 'json']
        result = self.kubectl(*args, check=False)
        return json.loads(result.stdout) if result.returncode == 0 else None

    def apply(self, objects):
        self.kubectl('apply', '--server-side', '-f', '-', input=json.dumps(
            {'apiVersion': 'v1', 'kind': 'List', 'items': objects}).encode())

    def patch(self, resource, namespace, value):
        path = self.work / ('patch-' + resource.replace('/', '-') + '.json')
        path.write_text(json.dumps(value))
        self.kubectl('patch', resource, '-n', namespace, '--type=strategic', '--patch-file', path)

    def git(self, path, *args):
        return command(['git', '-C', path, '-c', 'commit.gpgsign=false', '-c', 'core.hooksPath=/dev/null',
                        *args], env=self.git_env).stdout.decode().strip()

    def prepare(self):
        for tool in [self.args.kind, self.args.kubectl, 'docker', 'git']:
            if not shutil.which(tool):
                raise RuntimeError(f'tool missing: {tool}')
        version = command([self.args.kind, 'version']).stdout.decode()
        if VERSIONS['kind'] not in version and not self.args.unpinned_kind:
            raise RuntimeError('kind version differs from versions.json')
        (self.evidence / 'kind-version.txt').write_text(version)
        for component in ['argocd', 'flux']:
            spec = VERSIONS[component]
            path = self.vendor / f'{component}-{spec["version"]}.yaml'
            if not path.exists():
                command(['curl', '--fail', '--location', '--retry', '2', '--max-time', '120',
                         spec['url'], '--output', path], timeout=180)
            if hashlib.sha256(path.read_bytes()).hexdigest() != spec['sha256']:
                raise RuntimeError(f'{component} manifest checksum mismatch')
        if not self.args.skip_build:
            command(['docker', 'build', '-f', ROOT / 'tools/gitops/Dockerfile', '-t', self.args.image, ROOT], timeout=1800)
        if not self.args.reuse:
            clusters = command([self.args.kind, 'get', 'clusters']).stdout.decode().splitlines()
            if self.args.cluster in clusters:
                raise RuntimeError('cluster already exists; use --reuse only for this test cluster')
            # --unpinned-kind uses the installed kind and its default node image;
            # the actual versions are recorded in the evidence.
            image = [] if self.args.unpinned_kind else ['--image', VERSIONS['node_image']]
            command([self.args.kind, 'create', 'cluster', '--name', self.args.cluster,
                     *image, '--kubeconfig', self.kubeconfig,
                     '--wait', '120s'], timeout=600)
        elif not self.kubeconfig.exists():
            raise RuntimeError('--reuse requires the test kubeconfig')
        context = self.kubectl('config', 'current-context').stdout.decode().strip()
        if context != f'kind-{self.args.cluster}':
            raise RuntimeError('kubeconfig is not the dedicated test cluster')
        command([self.args.kind, 'load', 'docker-image', self.args.image,
                 '--name', self.args.cluster], timeout=300)
        server = json.loads(self.kubectl('version', '-o', 'json').stdout)
        (self.evidence / 'kubernetes.json').write_text(json.dumps(server, indent=2) + '\n')
        (self.evidence / 'versions.json').write_text(json.dumps(VERSIONS, indent=2) + '\n')

    def fixture(self, marker):
        source = self.work / 'source'
        source.mkdir(exist_ok=True)
        if not (source / '.git').exists():
            command(['git', 'init', '--template=', '--initial-branch=main', source], env=self.git_env)
        fixture_root = source / self.fixture_prefix
        for controller in ['argo', 'flux']:
            target = fixture_root / controller
            target.mkdir(parents=True, exist_ok=True)
            resource = {'apiVersion': 'v1', 'kind': 'ConfigMap',
                        'metadata': {'name': f'kasu-{controller}'},
                        'data': {'marker': marker, 'source': 'native-kasu-store'}}
            (target / 'configmap.yaml').write_text(json.dumps(resource, indent=2) + '\n')
        (fixture_root / 'flux/kustomization.yaml').write_text('apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n- configmap.yaml\n')
        self.git(source, 'add', '.')
        if command(['git', '-C', source, 'diff', '--cached', '--quiet'], env=self.git_env, check=False).returncode:
            self.git(source, 'commit', '--quiet', '-m', f'GitOps fixture {marker}')
        bare = self.work / 'repo.git'
        if not bare.exists():
            command(['git', '-c', 'protocol.file.allow=always', 'clone', '--bare', '--no-local', source, bare], env=self.git_env)
        else:
            self.git(bare, 'fetch', str(source), 'refs/heads/main:refs/heads/main')
        head = self.git(bare, 'rev-parse', 'refs/heads/main')
        raw = io.BytesIO()
        with gzip.GzipFile(fileobj=raw, mode='wb', mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode='w') as archive:
                archive.add(bare, arcname='repo.git')
        return head, raw.getvalue()

    def gateway(self, archive):
        self.apply([{'apiVersion': 'v1', 'kind': 'Namespace', 'metadata': {'name': ns}}
                    for ns in ['gitops-test', 'argocd', 'argo-target', 'flux-target']])
        self.apply([{'apiVersion': 'v1', 'kind': 'ConfigMap',
                     'metadata': {'name': 'kasu-fixture', 'namespace': 'gitops-test'},
                     'binaryData': {'repo.tar.gz': base64.b64encode(archive).decode()}}])
        pod = {'securityContext': {'runAsUser': 10001, 'runAsGroup': 10001, 'fsGroup': 10001},
               'initContainers': [{'name': 'ingest', 'image': self.args.image, 'imagePullPolicy': 'Never',
                                   'command': ['/bin/sh', '-ec'],
                                   'args': ['tar -xzf /fixture/repo.tar.gz -C /tmp; kasu-ingest /tmp/repo.git /data demo'],
                                   'volumeMounts': [{'name': 'data', 'mountPath': '/data'}, {'name': 'fixture', 'mountPath': '/fixture', 'readOnly': True}]}],
               'containers': [{'name': 'kasu', 'image': self.args.image, 'imagePullPolicy': 'Never',
                               'args': ['/data', 'demo', '0.0.0.0:50051', '0.0.0.0:50080'],
                               'env': [{'name': 'KASU_TOKEN', 'value': TOKEN}, {'name': 'RUST_LOG', 'value': 'kasu_server=trace'}],
                               'ports': [{'containerPort': 50080, 'name': 'http'}],
                               'readinessProbe': {'tcpSocket': {'port': 50080}, 'periodSeconds': 2},
                               'resources': {'requests': {'cpu': '100m', 'memory': '128Mi'}, 'limits': {'memory': '1Gi'}},
                               'volumeMounts': [{'name': 'data', 'mountPath': '/data'}]}],
               'volumes': [{'name': 'data', 'emptyDir': {}}, {'name': 'fixture', 'configMap': {'name': 'kasu-fixture'}}]}
        self.apply([{'apiVersion': 'apps/v1', 'kind': 'Deployment', 'metadata': {'name': 'kasu', 'namespace': 'gitops-test'},
                     'spec': {'replicas': 1, 'selector': {'matchLabels': {'app': 'kasu'}},
                              'template': {'metadata': {'labels': {'app': 'kasu'}, 'annotations': {'kasu.invalid/fixture-hash': hashlib.sha256(archive).hexdigest()}}, 'spec': pod}}},
                    {'apiVersion': 'v1', 'kind': 'Service', 'metadata': {'name': 'kasu', 'namespace': 'gitops-test'},
                     'spec': {'selector': {'app': 'kasu'}, 'ports': [{'name': 'http', 'port': 50080, 'targetPort': 50080}]}}])
        self.rollout('deployment/kasu', 'gitops-test')

    def rollout(self, resource, namespace):
        deadline = time.monotonic() + self.args.timeout
        while time.monotonic() < deadline:
            result = self.kubectl('rollout', 'status', resource, '-n', namespace, '--timeout=20s', check=False, timeout=30)
            if result.returncode == 0:
                print(result.stdout.decode(), flush=True)
                return
            print(f'waiting for {namespace}/{resource}', flush=True)
        raise RuntimeError(f'rollout timeout: {namespace}/{resource}')

    def controllers(self):
        for name, namespace in [('argocd', 'argocd'), ('flux', 'flux-system')]:
            self.kubectl('apply', '--server-side', '-n', namespace, '-f',
                         self.vendor / f'{name}-{VERSIONS[name]["version"]}.yaml', timeout=120)
        self.kubectl('set', 'env', 'deployment/argocd-repo-server', '-n', 'argocd',
                     'GIT_TRACE_PACKET=/tmp/kasu-git-packets.trace')
        if self.args.webhooks:
            self.patch('configmap/argocd-cm', 'argocd',
                       {'data': {'timeout.reconciliation': '1h', 'timeout.reconciliation.jitter': '0'}})
        for resource, namespace in [('deployment/argocd-repo-server', 'argocd'),
                                    ('statefulset/argocd-application-controller', 'argocd'),
                                    ('deployment/source-controller', 'flux-system'),
                                    ('deployment/kustomize-controller', 'flux-system')]:
            self.rollout(resource, namespace)
        self.apply([
            {'apiVersion': 'v1', 'kind': 'Secret', 'metadata': {'name': 'kasu-repository', 'namespace': 'argocd',
              'labels': {'argocd.argoproj.io/secret-type': 'repository'}},
             'stringData': {'type': 'git', 'url': self.repo_url, 'username': 'kasu', 'password': TOKEN}},
            {'apiVersion': 'v1', 'kind': 'Secret', 'metadata': {'name': 'kasu-auth', 'namespace': 'flux-system'},
             'stringData': {'username': 'kasu', 'password': TOKEN}},
            {'apiVersion': 'argoproj.io/v1alpha1', 'kind': 'Application',
             'metadata': {'name': 'kasu', 'namespace': 'argocd', 'annotations': {'argocd.argoproj.io/manifest-generate-paths': '.'}},
             'spec': {'project': 'default', 'source': {'repoURL': self.repo_url, 'targetRevision': 'main', 'path': 'argo'},
                      'destination': {'server': 'https://kubernetes.default.svc', 'namespace': 'argo-target'},
                      'syncPolicy': {'automated': {'prune': True, 'selfHeal': True}}}},
            {'apiVersion': 'source.toolkit.fluxcd.io/v1', 'kind': 'GitRepository',
             'metadata': {'name': 'kasu', 'namespace': 'flux-system'},
             'spec': {'interval': '1h' if self.args.webhooks else '5s', 'url': self.repo_url, 'ref': {'branch': 'main'}, 'secretRef': {'name': 'kasu-auth'}}},
            {'apiVersion': 'kustomize.toolkit.fluxcd.io/v1', 'kind': 'Kustomization',
             'metadata': {'name': 'kasu', 'namespace': 'flux-system'},
             'spec': {'interval': '1h' if self.args.webhooks else '5s', 'path': './flux', 'prune': True, 'targetNamespace': 'flux-target',
                      'sourceRef': {'kind': 'GitRepository', 'name': 'kasu'}}},
        ])
        self.kubectl('annotate', 'application/kasu', '-n', 'argocd', 'argocd.argoproj.io/refresh=hard', '--overwrite')
        self.kubectl('annotate', 'gitrepository/kasu', '-n', 'flux-system', f'reconcile.fluxcd.io/requestedAt={time.time_ns()}', '--overwrite')

    def wait_sync(self, head, marker, timeout=None, *, label=None):
        deadline = time.monotonic() + (timeout if timeout is not None else self.args.timeout)
        while time.monotonic() < deadline:
            app = self.get('application', 'kasu', 'argocd') or {}
            source = self.get('gitrepository', 'kasu', 'flux-system') or {}
            kust = self.get('kustomization', 'kasu', 'flux-system') or {}
            argo_map = self.get('configmap', 'kasu-argo', 'argo-target') or {}
            flux_map = self.get('configmap', 'kasu-flux', 'flux-target') or {}
            revision = source.get('status', {}).get('artifact', {}).get('revision', '')
            status = app.get('status', {})
            ready = lambda obj: any(c.get('type') == 'Ready' and c.get('status') == 'True' for c in obj.get('status', {}).get('conditions', []))
            if (status.get('sync', {}).get('status') == 'Synced' and status.get('sync', {}).get('revision') == head
                    and status.get('health', {}).get('status') == 'Healthy'
                    and revision.endswith(':' + head) and ready(source) and ready(kust)
                    and kust.get('status', {}).get('lastAppliedRevision') == revision
                    and argo_map.get('data', {}).get('marker') == marker and flux_map.get('data', {}).get('marker') == marker):
                snapshot = {'head': head, 'marker': marker, 'argocd': app, 'fluxSource': source,
                            'fluxKustomization': kust, 'argoConfigMap': argo_map, 'fluxConfigMap': flux_map}
                (self.evidence / f'sync-{label or marker}.json').write_text(json.dumps(snapshot, indent=2) + '\n')
                print(f'BOTH SYNCED marker={marker} head={head}', flush=True)
                return
            print(f'waiting sync marker={marker} argo={status.get("sync")} flux={revision}', flush=True)
            time.sleep(5)
        raise RuntimeError(f'GitOps sync timeout for {marker}/{head}')

    def update(self, archive):
        self.kubectl('exec', '-i', 'deployment/kasu', '-n', 'gitops-test', '-c', 'kasu', '--',
                     '/bin/sh', '-ec', 'mkdir -p /tmp/update; tar -xzf - -C /tmp/update; kasu-ingest /tmp/update/repo.git /data demo',
                     input=archive, timeout=60)
        if not self.args.webhooks:
            self.kubectl('annotate', 'application/kasu', '-n', 'argocd', 'argocd.argoproj.io/refresh=hard', '--overwrite')
            self.kubectl('annotate', 'gitrepository/kasu', '-n', 'flux-system',
                         f'reconcile.fluxcd.io/requestedAt={time.time_ns()}', '--overwrite')

    def logs(self, resource, namespace):
        return self.kubectl('logs', resource, '-n', namespace).stdout.decode(errors='replace')

    def wait_hook_ack(self, sequence):
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            logs = self.logs('deployment/kasu', 'gitops-test')
            if all(any('webhook acknowledged' in line and f'endpoint={endpoint}' in line
                       and 'stream=Native' in line and f'sequence={sequence}' in line
                       for line in logs.splitlines()) for endpoint in ['argocd', 'flux']):
                return
            time.sleep(2)
        raise RuntimeError(f'webhook acknowledgements missing for native sequence {sequence}')

    def configure_webhooks(self):
        self.patch('secret/argocd-secret', 'argocd',
                   {'stringData': {'webhook.github.secret': HOOK_TOKEN}})
        self.apply([
            {'apiVersion': 'v1', 'kind': 'Secret', 'metadata': {'name': 'kasu-hook-auth', 'namespace': 'flux-system'},
             'stringData': {'token': HOOK_TOKEN}},
            {'apiVersion': 'notification.toolkit.fluxcd.io/v1', 'kind': 'Receiver',
             'metadata': {'name': 'kasu', 'namespace': 'flux-system'},
             'spec': {'type': 'github', 'events': ['push'], 'secretRef': {'name': 'kasu-hook-auth'},
                      'resources': [{'apiVersion': 'source.toolkit.fluxcd.io/v1', 'kind': 'GitRepository', 'name': 'kasu'}]}},
        ])
        self.kubectl('set', 'env', 'deployment/argocd-server', '-n', 'argocd', 'ARGOCD_SERVER_INSECURE=true')
        self.rollout('deployment/argocd-server', 'argocd')
        self.rollout('deployment/notification-controller', 'flux-system')
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            receiver = self.get('receiver', 'kasu', 'flux-system') or {}
            path = receiver.get('status', {}).get('webhookPath')
            if path:
                break
            time.sleep(2)
        else:
            raise RuntimeError('Flux Receiver webhook path is missing')
        config = {'repository_url': self.repo_url, 'default_branch': 'main', 'endpoints': [
            {'id': 'argocd', 'url': 'http://argocd-server.argocd.svc.cluster.local/api/webhook', 'secret_file': '/webhooks/auth/token'},
            {'id': 'flux', 'url': 'http://webhook-receiver.flux-system.svc.cluster.local' + path, 'secret_file': '/webhooks/auth/token'}]}
        self.apply([
            {'apiVersion': 'v1', 'kind': 'Secret', 'metadata': {'name': 'kasu-hook-auth', 'namespace': 'gitops-test'},
             'stringData': {'token': HOOK_TOKEN}},
            {'apiVersion': 'v1', 'kind': 'ConfigMap', 'metadata': {'name': 'kasu-webhooks', 'namespace': 'gitops-test'},
             'data': {'config.json': json.dumps(config)}},
        ])
        self.patch('deployment/kasu', 'gitops-test', {'spec': {'template': {'spec': {
            'containers': [{'name': 'kasu', 'env': [{'name': 'KASU_WEBHOOKS_FILE', 'value': '/webhooks/config/config.json'}],
                            'volumeMounts': [{'name': 'webhook-config', 'mountPath': '/webhooks/config', 'readOnly': True},
                                             {'name': 'webhook-auth', 'mountPath': '/webhooks/auth', 'readOnly': True}]}],
            'volumes': [{'name': 'webhook-config', 'configMap': {'name': 'kasu-webhooks'}},
                        {'name': 'webhook-auth', 'secret': {'secretName': 'kasu-hook-auth'}}]}}}})
        self.rollout('deployment/kasu', 'gitops-test')
        self.wait_hook_ack(1)

    def collect(self):
        commands = {
            'pods.json': ['get', 'pods', '-A', '-o', 'json'],
            'events.txt': ['get', 'events', '-A', '--sort-by=.lastTimestamp'],
            'kasu-packets.txt': ['logs', 'deployment/kasu', '-n', 'gitops-test', '-c', 'kasu'],
            'argocd-repo.txt': ['logs', 'deployment/argocd-repo-server', '-n', 'argocd', '-c', 'argocd-repo-server'],
            'argocd-packets.txt': ['exec', 'deployment/argocd-repo-server', '-n', 'argocd', '-c', 'argocd-repo-server', '--', 'cat', '/tmp/kasu-git-packets.trace'],
            'argocd-git-version.txt': ['exec', 'deployment/argocd-repo-server', '-n', 'argocd', '-c', 'argocd-repo-server', '--', 'git', '--version'],
            'flux-source.txt': ['logs', 'deployment/source-controller', '-n', 'flux-system'],
            'flux-kustomize.txt': ['logs', 'deployment/kustomize-controller', '-n', 'flux-system'],
            'argocd-server.txt': ['logs', 'deployment/argocd-server', '-n', 'argocd'],
            'flux-notification.txt': ['logs', 'deployment/notification-controller', '-n', 'flux-system'],
        }
        for name, args in commands.items():
            try:
                result = self.kubectl(*args, check=False)
                (self.evidence / name).write_bytes(result.stdout + result.stderr)
            except subprocess.TimeoutExpired:
                (self.evidence / name).write_text('observation timed out\n')

    def run(self):
        started = datetime.datetime.now(datetime.timezone.utc).isoformat()
        result = {'started': started, 'success': False}
        try:
            self.prepare()
            head, archive = self.fixture('one')
            self.gateway(archive)
            self.controllers()
            self.wait_sync(head, 'one')
            if self.args.webhooks:
                self.configure_webhooks()
                baseline = self.logs('deployment/argocd-server', 'argocd').count('refreshing app from webhook')
                baseline_flux = (self.get('gitrepository', 'kasu', 'flux-system') or {}).get('metadata', {}).get('annotations', {}).get('reconcile.fluxcd.io/requestedAt')
            updated, archive = self.fixture('two')
            if updated == head:
                raise RuntimeError('update fixture must change the Git commit')
            started_update = time.monotonic()
            self.update(archive)
            self.wait_sync(updated, 'two', timeout=60 if self.args.webhooks else None)
            if self.args.webhooks:
                self.wait_hook_ack(2)
                refreshed = self.logs('deployment/argocd-server', 'argocd').count('refreshing app from webhook')
                requested = (self.get('gitrepository', 'kasu', 'flux-system') or {}).get('metadata', {}).get('annotations', {}).get('reconcile.fluxcd.io/requestedAt')
                if refreshed <= baseline or requested == baseline_flux:
                    raise RuntimeError('webhook-triggered ArgoCD refresh / Flux reconcile evidence is missing')
                proof = {'pollInterval': '1h', 'manualRefreshAfterUpdate': False,
                         'updateElapsedSeconds': round(time.monotonic() - started_update, 3),
                         'argocdWebhookRefreshesBefore': baseline, 'argocdWebhookRefreshesAfter': refreshed,
                         'fluxRequestedAtBefore': baseline_flux, 'fluxRequestedAtAfter': requested,
                         'acknowledgedNativeSequence': 2, 'initial': head, 'updated': updated}
                (self.evidence / 'webhook-proof.json').write_text(json.dumps(proof, indent=2) + '\n')
            result.update(success=True, initial=head, updated=updated)
        finally:
            self.collect()
            result['finished'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            (self.evidence / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        if not self.args.keep:
            command([self.args.kind, 'delete', 'cluster', '--name', self.args.cluster], timeout=120)


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workdir', required=True)
    parser.add_argument('--kind', default='kind')
    parser.add_argument('--kubectl', default='kubectl')
    parser.add_argument('--cluster', default='kasu-gitops-e2e')
    parser.add_argument('--image', default='kasu:gitops-e2e')
    parser.add_argument('--reuse', action='store_true')
    parser.add_argument('--skip-build', action='store_true')
    parser.add_argument('--keep', action='store_true')
    parser.add_argument('--webhooks', action='store_true', help='verify signed webhook-only refresh with 1h polling')
    parser.add_argument('--timeout', type=int, default=600)
    parser.add_argument('--unpinned-kind', action='store_true',
                        help='use the installed kind version and its default node image (recorded in evidence)')
    return parser


def main():
    Test(argument_parser().parse_args()).run()

if __name__ == '__main__':
    main()
