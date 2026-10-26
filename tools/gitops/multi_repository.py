#!/usr/bin/env python3
"""Two repositories on one kasu server, synchronized separately by ArgoCD and Flux.

Each repository is created with `kasu-ingest --create`, declared in
KASU_REPOSITORIES_FILE with its own webhook endpoints, and consumed by its own
ArgoCD Application and Flux GitRepository. Polling is one hour: after each
repository is updated, only webhooks can make the controllers notice, so a
timely sync proves the push payload named the right repository. The other
repository must stay at its previous revision.

Uses the same dedicated-cluster rules as run.py (private kubeconfig, --reuse
only for this test's cluster and workdir).
"""
from __future__ import annotations
import base64
import datetime
import gzip
import hashlib
import io
import json
from pathlib import Path
import sys
import tarfile
import time

sys.path.insert(0, str(Path(__file__).parent))
import run  # noqa: E402

SERVICE = 'http://kasu.gitops-test.svc.cluster.local:50080'
REPOSITORIES = {'a': 'team-a/apps', 'b': 'team-b/apps'}


class MultiRepositoryTest(run.Test):
    def url(self, slug):
        return f'{SERVICE}/{REPOSITORIES[slug]}.git'

    def fixtures(self, markers):
        """Commit `markers[slug]` to each repository and archive both bare repositories."""
        heads = {}
        raw = io.BytesIO()
        with gzip.GzipFile(fileobj=raw, mode='wb', mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode='w') as archive:
                for slug, marker in markers.items():
                    source = self.work / f'source-{slug}'
                    source.mkdir(exist_ok=True)
                    if not (source / '.git').exists():
                        run.command(['git', 'init', '--template=', '--initial-branch=main', source], env=self.git_env)
                    for controller in ['argo', 'flux']:
                        target = source / controller
                        target.mkdir(parents=True, exist_ok=True)
                        resource = {'apiVersion': 'v1', 'kind': 'ConfigMap',
                                    'metadata': {'name': f'kasu-{controller}'},
                                    'data': {'marker': marker, 'repository': REPOSITORIES[slug]}}
                        (target / 'configmap.yaml').write_text(json.dumps(resource, indent=2) + '\n')
                    (source / 'flux/kustomization.yaml').write_text('apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n- configmap.yaml\n')
                    self.git(source, 'add', '.')
                    if run.command(['git', '-C', source, 'diff', '--cached', '--quiet'], env=self.git_env, check=False).returncode:
                        self.git(source, 'commit', '--quiet', '-m', f'{REPOSITORIES[slug]} {marker}')
                    bare = self.work / f'{slug}.git'
                    if not bare.exists():
                        run.command(['git', '-c', 'protocol.file.allow=always', 'clone', '--bare', '--no-local', source, bare], env=self.git_env)
                    else:
                        self.git(bare, 'fetch', str(source), 'refs/heads/main:refs/heads/main')
                    heads[slug] = self.git(bare, 'rev-parse', 'refs/heads/main')
                    archive.add(bare, arcname=f'{slug}.git')
        return heads, raw.getvalue()

    def declarations(self, webhooks):
        repositories = []
        for slug, name in REPOSITORIES.items():
            entry = {'name': name, 'default_branch': 'main'}
            if webhooks:
                entry['webhooks'] = [
                    {'id': 'argocd', 'url': 'http://argocd-server.argocd.svc.cluster.local/api/webhook',
                     'secret_file': '/webhooks/auth/token'},
                    {'id': 'flux', 'url': 'http://webhook-receiver.flux-system.svc.cluster.local' + webhooks[slug],
                     'secret_file': '/webhooks/auth/token'}]
            repositories.append(entry)
        return json.dumps({'repositories': repositories}, indent=2)

    def gateway(self, archive):
        self.apply([{'apiVersion': 'v1', 'kind': 'Namespace', 'metadata': {'name': ns}}
                    for ns in ['gitops-test', 'argocd', 'argo-a', 'argo-b', 'flux-a', 'flux-b']])
        self.apply([
            {'apiVersion': 'v1', 'kind': 'ConfigMap', 'metadata': {'name': 'kasu-fixture', 'namespace': 'gitops-test'},
             'binaryData': {'repos.tar.gz': base64.b64encode(archive).decode()}},
            {'apiVersion': 'v1', 'kind': 'ConfigMap', 'metadata': {'name': 'kasu-repositories', 'namespace': 'gitops-test'},
             'data': {'repositories.json': self.declarations(None)}},
            {'apiVersion': 'v1', 'kind': 'Secret', 'metadata': {'name': 'kasu-hook-auth', 'namespace': 'gitops-test'},
             'stringData': {'token': run.HOOK_TOKEN}},
        ])
        ingest = 'tar -xzf /fixture/repos.tar.gz -C /tmp; ' + '; '.join(
            f'kasu-ingest --create /tmp/{slug}.git /data {name}' for slug, name in REPOSITORIES.items())
        pod = {'securityContext': {'runAsUser': 10001, 'runAsGroup': 10001, 'fsGroup': 10001},
               'initContainers': [{'name': 'ingest', 'image': self.args.image, 'imagePullPolicy': 'Never',
                                   'command': ['/bin/sh', '-ec'], 'args': [ingest],
                                   'volumeMounts': [{'name': 'data', 'mountPath': '/data'}, {'name': 'fixture', 'mountPath': '/fixture', 'readOnly': True}]}],
               # No repository argument: every registered repository is served.
               'containers': [{'name': 'kasu', 'image': self.args.image, 'imagePullPolicy': 'Never',
                               'args': ['/data', '0.0.0.0:50051', '0.0.0.0:50080'],
                               'env': [{'name': 'KASU_TOKEN', 'value': run.TOKEN},
                                       {'name': 'KASU_REPOSITORIES_FILE', 'value': '/repositories/repositories.json'},
                                       {'name': 'KASU_PUBLIC_URL', 'value': SERVICE},
                                       {'name': 'RUST_LOG', 'value': 'kasu_server=info'}],
                               'ports': [{'containerPort': 50080, 'name': 'http'}],
                               'readinessProbe': {'tcpSocket': {'port': 50080}, 'periodSeconds': 2},
                               'resources': {'requests': {'cpu': '100m', 'memory': '128Mi'}, 'limits': {'memory': '1Gi'}},
                               'volumeMounts': [{'name': 'data', 'mountPath': '/data'},
                                                {'name': 'repositories', 'mountPath': '/repositories', 'readOnly': True},
                                                {'name': 'webhook-auth', 'mountPath': '/webhooks/auth', 'readOnly': True}]}],
               'volumes': [{'name': 'data', 'emptyDir': {}}, {'name': 'fixture', 'configMap': {'name': 'kasu-fixture'}},
                           {'name': 'repositories', 'configMap': {'name': 'kasu-repositories'}},
                           {'name': 'webhook-auth', 'secret': {'secretName': 'kasu-hook-auth'}}]}
        self.apply([{'apiVersion': 'apps/v1', 'kind': 'Deployment', 'metadata': {'name': 'kasu', 'namespace': 'gitops-test'},
                     'spec': {'replicas': 1, 'selector': {'matchLabels': {'app': 'kasu'}},
                              'template': {'metadata': {'labels': {'app': 'kasu'}, 'annotations': {'kasu.invalid/fixture-hash': hashlib.sha256(archive).hexdigest()}}, 'spec': pod}}},
                    {'apiVersion': 'v1', 'kind': 'Service', 'metadata': {'name': 'kasu', 'namespace': 'gitops-test'},
                     'spec': {'selector': {'app': 'kasu'}, 'ports': [{'name': 'http', 'port': 50080, 'targetPort': 50080}]}}])
        self.rollout('deployment/kasu', 'gitops-test')

    def controllers(self):
        for name, namespace in [('argocd', 'argocd'), ('flux', 'flux-system')]:
            self.kubectl('apply', '--server-side', '-n', namespace, '-f',
                         self.vendor / f'{name}-{run.VERSIONS[name]["version"]}.yaml', timeout=120)
        self.patch('configmap/argocd-cm', 'argocd',
                   {'data': {'timeout.reconciliation': '1h', 'timeout.reconciliation.jitter': '0'}})
        for resource, namespace in [('deployment/argocd-repo-server', 'argocd'),
                                    ('statefulset/argocd-application-controller', 'argocd'),
                                    ('deployment/source-controller', 'flux-system'),
                                    ('deployment/kustomize-controller', 'flux-system')]:
            self.rollout(resource, namespace)
        objects = [{'apiVersion': 'v1', 'kind': 'Secret', 'metadata': {'name': 'kasu-auth', 'namespace': 'flux-system'},
                    'stringData': {'username': 'kasu', 'password': run.TOKEN}}]
        for slug in REPOSITORIES:
            objects += [
                {'apiVersion': 'v1', 'kind': 'Secret', 'metadata': {'name': f'kasu-repository-{slug}', 'namespace': 'argocd',
                  'labels': {'argocd.argoproj.io/secret-type': 'repository'}},
                 'stringData': {'type': 'git', 'url': self.url(slug), 'username': 'kasu', 'password': run.TOKEN}},
                {'apiVersion': 'argoproj.io/v1alpha1', 'kind': 'Application',
                 'metadata': {'name': f'kasu-{slug}', 'namespace': 'argocd', 'annotations': {'argocd.argoproj.io/manifest-generate-paths': '.'}},
                 'spec': {'project': 'default', 'source': {'repoURL': self.url(slug), 'targetRevision': 'main', 'path': 'argo'},
                          'destination': {'server': 'https://kubernetes.default.svc', 'namespace': f'argo-{slug}'},
                          'syncPolicy': {'automated': {'prune': True, 'selfHeal': True}}}},
                {'apiVersion': 'source.toolkit.fluxcd.io/v1', 'kind': 'GitRepository',
                 'metadata': {'name': f'kasu-{slug}', 'namespace': 'flux-system'},
                 'spec': {'interval': '1h', 'url': self.url(slug), 'ref': {'branch': 'main'}, 'secretRef': {'name': 'kasu-auth'}}},
                {'apiVersion': 'kustomize.toolkit.fluxcd.io/v1', 'kind': 'Kustomization',
                 'metadata': {'name': f'kasu-{slug}', 'namespace': 'flux-system'},
                 'spec': {'interval': '1h', 'path': './flux', 'prune': True, 'targetNamespace': f'flux-{slug}',
                          'sourceRef': {'kind': 'GitRepository', 'name': f'kasu-{slug}'}}},
            ]
        self.apply(objects)
        # The initial sync is requested explicitly; later ones only by webhook.
        for slug in REPOSITORIES:
            self.kubectl('annotate', f'application/kasu-{slug}', '-n', 'argocd', 'argocd.argoproj.io/refresh=hard', '--overwrite')
            self.kubectl('annotate', f'gitrepository/kasu-{slug}', '-n', 'flux-system',
                         f'reconcile.fluxcd.io/requestedAt={time.time_ns()}', '--overwrite')

    def synced(self, slug, head, marker):
        app = self.get('application', f'kasu-{slug}', 'argocd') or {}
        source = self.get('gitrepository', f'kasu-{slug}', 'flux-system') or {}
        kust = self.get('kustomization', f'kasu-{slug}', 'flux-system') or {}
        argo_map = self.get('configmap', 'kasu-argo', f'argo-{slug}') or {}
        flux_map = self.get('configmap', 'kasu-flux', f'flux-{slug}') or {}
        revision = source.get('status', {}).get('artifact', {}).get('revision', '')
        status = app.get('status', {})
        ready = lambda obj: any(c.get('type') == 'Ready' and c.get('status') == 'True' for c in obj.get('status', {}).get('conditions', []))
        done = (status.get('sync', {}).get('status') == 'Synced' and status.get('sync', {}).get('revision') == head
                and status.get('health', {}).get('status') == 'Healthy'
                and revision.endswith(':' + head) and ready(source) and ready(kust)
                and kust.get('status', {}).get('lastAppliedRevision') == revision
                and argo_map.get('data', {}).get('marker') == marker and flux_map.get('data', {}).get('marker') == marker
                and argo_map.get('data', {}).get('repository') == REPOSITORIES[slug])
        return done, {'argocd': app, 'fluxSource': source, 'fluxKustomization': kust,
                      'argoConfigMap': argo_map, 'fluxConfigMap': flux_map}

    def wait_all(self, heads, markers, label, timeout):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            states = {slug: self.synced(slug, heads[slug], markers[slug]) for slug in REPOSITORIES}
            if all(done for done, _ in states.values()):
                snapshot = {slug: {'head': heads[slug], 'marker': markers[slug], **state} for slug, (_, state) in states.items()}
                (self.evidence / f'multi-sync-{label}.json').write_text(json.dumps(snapshot, indent=2) + '\n')
                print(f'ALL REPOSITORIES SYNCED {label} {heads}', flush=True)
                return
            print(f'waiting {label}: ' + ', '.join(f'{slug}={done}' for slug, (done, _) in states.items()), flush=True)
            time.sleep(5)
        raise RuntimeError(f'multi-repository sync timeout for {label}')

    def acks(self):
        """Webhook acknowledgements per (repository, endpoint)."""
        counts = {}
        for line in self.logs('deployment/kasu', 'gitops-test').splitlines():
            if 'webhook acknowledged' not in line or 'stream=Native' not in line:
                continue
            for name in REPOSITORIES.values():
                for endpoint in ['argocd', 'flux']:
                    if f'repository={name}' in line and f'endpoint={endpoint}' in line:
                        counts[f'{name}/{endpoint}'] = counts.get(f'{name}/{endpoint}', 0) + 1
        return counts

    def configure_webhooks(self):
        self.patch('secret/argocd-secret', 'argocd', {'stringData': {'webhook.github.secret': run.HOOK_TOKEN}})
        objects = [{'apiVersion': 'v1', 'kind': 'Secret', 'metadata': {'name': 'kasu-hook-auth', 'namespace': 'flux-system'},
                    'stringData': {'token': run.HOOK_TOKEN}}]
        for slug in REPOSITORIES:
            objects.append({'apiVersion': 'notification.toolkit.fluxcd.io/v1', 'kind': 'Receiver',
                            'metadata': {'name': f'kasu-{slug}', 'namespace': 'flux-system'},
                            'spec': {'type': 'github', 'events': ['push'], 'secretRef': {'name': 'kasu-hook-auth'},
                                     'resources': [{'apiVersion': 'source.toolkit.fluxcd.io/v1', 'kind': 'GitRepository', 'name': f'kasu-{slug}'}]}})
        self.apply(objects)
        self.kubectl('set', 'env', 'deployment/argocd-server', '-n', 'argocd', 'ARGOCD_SERVER_INSECURE=true')
        self.rollout('deployment/argocd-server', 'argocd')
        self.rollout('deployment/notification-controller', 'flux-system')
        paths = {}
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline and len(paths) < len(REPOSITORIES):
            for slug in REPOSITORIES:
                receiver = self.get('receiver', f'kasu-{slug}', 'flux-system') or {}
                if receiver.get('status', {}).get('webhookPath'):
                    paths[slug] = receiver['status']['webhookPath']
            time.sleep(2)
        if len(paths) < len(REPOSITORIES):
            raise RuntimeError('Flux Receiver webhook paths are missing')
        self.apply([{'apiVersion': 'v1', 'kind': 'ConfigMap', 'metadata': {'name': 'kasu-repositories', 'namespace': 'gitops-test'},
                     'data': {'repositories.json': self.declarations(paths)}}])
        # A restart applies the declaration at once; the data volume is
        # re-ingested from the same fixtures, so heads are unchanged.
        self.kubectl('rollout', 'restart', 'deployment/kasu', '-n', 'gitops-test')
        self.rollout('deployment/kasu', 'gitops-test')
        # Each repository first delivers its initial publication; wait until
        # those settle so later counts belong to the updates alone.
        keys = [f'{name}/{endpoint}' for name in REPOSITORIES.values() for endpoint in ['argocd', 'flux']]
        previous = self.wait_acks({}, keys)
        while True:
            time.sleep(10)
            current = self.acks()
            if current == previous:
                return
            previous = current

    def update(self, slug, archive):
        name = REPOSITORIES[slug]
        self.kubectl('exec', '-i', 'deployment/kasu', '-n', 'gitops-test', '-c', 'kasu', '--',
                     '/bin/sh', '-ec', f'mkdir -p /tmp/update; tar -xzf - -C /tmp/update; kasu-ingest /tmp/update/{slug}.git /data {name}',
                     input=archive, timeout=60)

    def wait_acks(self, before, keys, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            now = self.acks()
            if all(now.get(key, 0) > before.get(key, 0) for key in keys):
                return now
            time.sleep(2)
        raise RuntimeError(f'webhook acknowledgements missing for {keys}: {self.acks()}')

    def run(self):
        result = {'started': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'success': False}
        try:
            self.prepare()
            markers = {'a': 'a-one', 'b': 'b-one'}
            heads, archive = self.fixtures(markers)
            self.gateway(archive)
            self.controllers()
            self.wait_all(heads, markers, 'initial', self.args.timeout)
            self.configure_webhooks()
            steps = []
            for slug in REPOSITORIES:
                name = REPOSITORIES[slug]
                before = self.acks()
                markers = {**markers, slug: f'{slug}-two'}
                updated, archive = self.fixtures(markers)
                if updated[slug] == heads[slug] or any(updated[o] != heads[o] for o in REPOSITORIES if o != slug):
                    raise RuntimeError('only the updated repository may change')
                started = time.monotonic()
                self.update(slug, archive)
                # One-hour polling: only a webhook naming this repository can
                # move its consumers; the other repository keeps its revision.
                self.wait_all(updated, markers, f'after-{slug}', 90)
                acks = self.wait_acks(before, [f'{name}/argocd', f'{name}/flux'])
                others = [f'{other}/{endpoint}' for other in REPOSITORIES.values() if other != name for endpoint in ['argocd', 'flux']]
                if any(acks.get(key, 0) != before.get(key, 0) for key in others):
                    raise RuntimeError(f'updating {name} delivered webhooks for another repository: {acks}')
                steps.append({'repository': name, 'head': updated[slug], 'elapsedSeconds': round(time.monotonic() - started, 3),
                              'acknowledgements': acks})
                heads = updated
            proof = {'pollInterval': '1h', 'manualRefreshAfterUpdate': False, 'repositories': REPOSITORIES, 'steps': steps}
            (self.evidence / 'multi-repository-proof.json').write_text(json.dumps(proof, indent=2) + '\n')
            result.update(success=True, heads=heads)
        finally:
            self.collect()
            result['finished'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            (self.evidence / 'multi-repository-result.json').write_text(json.dumps(result, indent=2) + '\n')
        if not self.args.keep:
            run.command([self.args.kind, 'delete', 'cluster', '--name', self.args.cluster], timeout=120)


def main():
    parser = run.argument_parser()
    parser.set_defaults(cluster='kasu-gitops-multi')
    MultiRepositoryTest(parser.parse_args()).run()


if __name__ == '__main__':
    main()
