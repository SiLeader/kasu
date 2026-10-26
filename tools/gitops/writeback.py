#!/usr/bin/env python3
"""Exercise real Argo CD Image Updater and Flux Image Automation Git writes."""
import argparse
import contextlib
import hashlib
import json
import re
import subprocess
import tarfile
import time
import urllib.parse
import urllib.request

from run import Test, command, REPO_URL, TOKEN, VERSIONS

REGISTRY = 'kasu-registry.gitops-test.svc.cluster.local:5000'
IMAGE = REGISTRY + '/demo'


class Writeback(Test):
    @contextlib.contextmanager
    def forward(self, service, port):
        log = self.work / f'forward-{service}.log'
        with log.open('wb') as output:
            process = subprocess.Popen([
                self.args.kubectl, '--kubeconfig', str(self.kubeconfig),
                'port-forward', '-n', 'gitops-test', f'service/{service}', f':{port}',
                '--address=127.0.0.1'], stdout=output, stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    match = re.search(r'Forwarding from 127\.0\.0\.1:(\d+)', log.read_text())
                    if match:
                        yield int(match[1])
                        return
                    if process.poll() is not None:
                        raise RuntimeError(log.read_text())
                    time.sleep(.2)
                raise RuntimeError(f'port-forward did not start: {service}')
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()

    def client_git(self, *args):
        return self.git(self.client, '-c', f'http.extraHeader=Authorization: Bearer {TOKEN}', *args)

    def metadata(self, method, body):
        request = urllib.request.Request(f'http://127.0.0.1:{self.git_port}/v1/{method}',
                                         data=json.dumps({'repo': list(b'demo'), **body}).encode(),
                                         headers={'Content-Type': 'application/json', 'Authorization': 'Bearer ' + TOKEN})
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)

    def wait(self, description, predicate, timeout=180):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            result = predicate()
            if result:
                return result
            time.sleep(3)
        raise RuntimeError('timed out: ' + description)

    def registry(self):
        self.apply([
            {'apiVersion': 'apps/v1', 'kind': 'Deployment',
             'metadata': {'name': 'kasu-registry', 'namespace': 'gitops-test'},
             'spec': {'replicas': 1, 'selector': {'matchLabels': {'app': 'kasu-registry'}},
                      'template': {'metadata': {'labels': {'app': 'kasu-registry'}},
                                   'spec': {'containers': [{'name': 'registry', 'image': VERSIONS['test_registry_image'],
                                                            'ports': [{'containerPort': 5000}],
                                                            'readinessProbe': {'httpGet': {'path': '/v2/', 'port': 5000}}}]}}}},
            {'apiVersion': 'v1', 'kind': 'Service',
             'metadata': {'name': 'kasu-registry', 'namespace': 'gitops-test'},
             'spec': {'selector': {'app': 'kasu-registry'}, 'ports': [{'port': 5000, 'targetPort': 5000}]}},
        ])
        self.rollout('deployment/kasu-registry', 'gitops-test')
        command(['docker', 'pull', VERSIONS['test_workload_image']], timeout=120)
        archive = self.work / 'registry-image.tar'
        command(['docker', 'image', 'save', '--output', archive, VERSIONS['test_workload_image']], timeout=120)
        with tarfile.open(archive) as image:
            saved = json.load(image.extractfile('manifest.json'))[0]
            config = image.extractfile(saved['Config']).read()
            layers = [image.extractfile(path).read() for path in saved['Layers']]
        self.registry_blobs = [config, *layers]
        descriptor = lambda body, media: {'mediaType': media, 'digest': 'sha256:' + hashlib.sha256(body).hexdigest(), 'size': len(body)}
        self.registry_manifest = json.dumps({
            'schemaVersion': 2, 'mediaType': 'application/vnd.oci.image.manifest.v1+json',
            'config': descriptor(config, 'application/vnd.oci.image.config.v1+json'),
            'layers': [descriptor(layer, 'application/vnd.oci.image.layer.v1.tar' + ('+gzip' if layer[:2] == b'\x1f\x8b' else '')) for layer in layers],
        }).encode()

    def publish(self, port, tag):
        # The Docker daemon may run inside a VM, where its localhost differs
        # from the host's kubectl forward. Upload the saved image via OCI HTTP.
        base = f'http://127.0.0.1:{port}'
        for body in self.registry_blobs:
            with urllib.request.urlopen(urllib.request.Request(base + '/v2/demo/blobs/uploads/', data=b'', method='POST'), timeout=30) as response:
                location = urllib.parse.urljoin(base, response.headers['Location'])
            location += ('&' if '?' in location else '?') + 'digest=sha256:' + hashlib.sha256(body).hexdigest()
            request = urllib.request.Request(location, data=body, method='PUT', headers={'Content-Type': 'application/octet-stream'})
            with urllib.request.urlopen(request, timeout=30) as response:
                if response.status != 201:
                    raise RuntimeError('registry did not accept image blob')
        request = urllib.request.Request(base + '/v2/demo/manifests/' + tag, data=self.registry_manifest, method='PUT',
                                         headers={'Content-Type': 'application/vnd.oci.image.manifest.v1+json'})
        with urllib.request.urlopen(request, timeout=30) as response:
            if response.status != 201:
                raise RuntimeError('registry did not accept image manifest')

    def seed(self, port):
        self.client = self.work / 'writeback-client'
        if self.client.exists():
            raise RuntimeError('writeback-client already exists; use a fresh successful sync workdir')
        command(['git', '-c', f'http.extraHeader=Authorization: Bearer {TOKEN}',
                 'clone', '--branch', 'main', f'http://127.0.0.1:{port}/demo.git', self.client], env=self.git_env)
        for controller in ['argo', 'flux']:
            target = self.client / f'{controller}-images'
            target.mkdir()
            workload = (
                'apiVersion: apps/v1\nkind: Deployment\nmetadata:\n'
                f'  name: kasu-{controller}-images\nspec:\n  replicas: 0\n'
                f'  selector:\n    matchLabels:\n      app: kasu-{controller}-images\n'
                f'  template:\n    metadata:\n      labels:\n        app: kasu-{controller}-images\n'
                '    spec:\n      containers:\n      - name: demo\n'
                f'        image: {IMAGE}:1.0.0'
                + (' # {"$imagepolicy": "flux-system:kasu-images"}' if controller == 'flux' else '') + '\n')
            (target / 'deployment.yaml').write_text(workload)
            kustomization = 'apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n- deployment.yaml\n'
            if controller == 'argo':
                kustomization += f'images:\n- name: {IMAGE}\n  newTag: 1.0.0\n'
            (target / 'kustomization.yaml').write_text(kustomization)
        self.client_git('add', '.')
        self.client_git('commit', '-m', 'Seed real image automation fixtures')
        self.client_git('push', 'origin', 'main')
        return self.client_git('rev-parse', 'HEAD')

    def updater(self):
        spec = VERSIONS['image_updater']
        manifest = self.vendor / f'image-updater-{spec["version"]}.yaml'
        if not manifest.exists():
            command(['curl', '--fail', '--location', '--retry', '2', '--max-time', '120',
                     spec['url'], '--output', manifest], timeout=180)
        if hashlib.sha256(manifest.read_bytes()).hexdigest() != spec['sha256']:
            raise RuntimeError('Image Updater manifest checksum mismatch')
        self.kubectl('apply', '--server-side', '-n', 'argocd', '-f', manifest, timeout=120)
        self.patch('configmap/argocd-image-updater-config', 'argocd', {'data': {
            'interval': '5s', 'git.user': 'kasu-argocd-image-bot', 'git.email': 'argo@kasu.invalid',
            'registries.conf': f'registries:\n- name: kasu-test\n  prefix: {REGISTRY}\n  api_url: http://{REGISTRY}\n  insecure: true\n'}})
        self.kubectl('rollout', 'restart', 'deployment/argocd-image-updater-controller', '-n', 'argocd')
        self.rollout('deployment/argocd-image-updater-controller', 'argocd')

    def resources(self):
        self.apply([
            {'apiVersion': 'argoproj.io/v1alpha1', 'kind': 'Application',
             'metadata': {'name': 'kasu-images', 'namespace': 'argocd',
                          'annotations': {'argocd.argoproj.io/manifest-generate-paths': '.'}},
             'spec': {'project': 'default', 'source': {'repoURL': REPO_URL, 'targetRevision': 'main', 'path': 'argo-images'},
                      'destination': {'server': 'https://kubernetes.default.svc', 'namespace': 'argo-target'},
                      'syncPolicy': {'automated': {'prune': True, 'selfHeal': True}}}},
            {'apiVersion': 'kustomize.toolkit.fluxcd.io/v1', 'kind': 'Kustomization',
             'metadata': {'name': 'kasu-images', 'namespace': 'flux-system'},
             'spec': {'interval': '1h', 'path': './flux-images', 'prune': True, 'targetNamespace': 'flux-target',
                      'sourceRef': {'kind': 'GitRepository', 'name': 'kasu'}}},
            {'apiVersion': 'image.toolkit.fluxcd.io/v1', 'kind': 'ImageRepository',
             'metadata': {'name': 'kasu-images', 'namespace': 'flux-system'},
             'spec': {'image': IMAGE, 'interval': '5s', 'insecure': True}},
            {'apiVersion': 'image.toolkit.fluxcd.io/v1', 'kind': 'ImagePolicy',
             'metadata': {'name': 'kasu-images', 'namespace': 'flux-system'},
             'spec': {'imageRepositoryRef': {'name': 'kasu-images'}, 'policy': {'semver': {'range': '>=1.0.0 <2.0.0'}}}},
        ])
        self.wait('Flux initial image policy', lambda:
                  (self.get('imagepolicy', 'kasu-images', 'flux-system') or {}).get('status', {}).get('latestRef', {}).get('tag') == '1.0.0')
        self.wait('Argo initial image deployment', lambda:
                  (self.get('deployment', 'kasu-argo-images', 'argo-target') or {}).get('spec', {}).get('template', {}).get('spec', {}).get('containers'))
        self.apply([
            {'apiVersion': 'argocd-image-updater.argoproj.io/v1alpha1', 'kind': 'ImageUpdater',
             'metadata': {'name': 'kasu-images', 'namespace': 'argocd'},
             'spec': {'writeBackConfig': {'method': 'git',
                                          'gitConfig': {'branch': 'main', 'writeBackTarget': 'kustomization'}},
                      'applicationRefs': [{'namePattern': 'kasu-images', 'images': [
                          {'alias': 'demo', 'imageName': IMAGE,
                           'commonUpdateSettings': {'updateStrategy': 'semver', 'forceUpdate': True}}]}]}},
            {'apiVersion': 'image.toolkit.fluxcd.io/v1', 'kind': 'ImageUpdateAutomation',
             'metadata': {'name': 'kasu-images', 'namespace': 'flux-system'},
             'spec': {'interval': '5s', 'sourceRef': {'kind': 'GitRepository', 'name': 'kasu'},
                      'git': {'checkout': {'ref': {'branch': 'main'}}, 'push': {'branch': 'main'},
                              'commit': {'author': {'name': 'kasu-flux-image-bot', 'email': 'flux@kasu.invalid'},
                                         'messageTemplate': 'Update Flux test image'}},
                      'update': {'path': './flux-images', 'strategy': 'Setters'}}},
        ])

    def verify(self, baseline):
        self.client_git('fetch', 'origin', 'main')
        argo = self.client_git('show', 'origin/main:argo-images/kustomization.yaml')
        flux = self.client_git('show', 'origin/main:flux-images/deployment.yaml')
        if '1.0.1' not in argo or f'{IMAGE}:1.0.1' not in flux:
            return None
        head = self.client_git('rev-parse', 'origin/main')
        for controller, namespace in [('argo', 'argo-target'), ('flux', 'flux-target')]:
            deployed = self.get('deployment', f'kasu-{controller}-images', namespace) or {}
            containers = deployed.get('spec', {}).get('template', {}).get('spec', {}).get('containers', [])
            if not containers or containers[0]['image'] != IMAGE + ':1.0.1':
                return None
        app = self.get('application', 'kasu-images', 'argocd') or {}
        if app.get('status', {}).get('sync', {}).get('revision') != head:
            return None
        commits = self.client_git('log', '--format=%H %an <%ae>', f'{baseline}..origin/main').splitlines()
        if not any('kasu-argocd-image-bot <argo@kasu.invalid>' in c for c in commits):
            raise RuntimeError('Argo image updater commit is missing')
        if not any('kasu-flux-image-bot <flux@kasu.invalid>' in c for c in commits):
            raise RuntimeError('Flux image automation commit is missing')
        self.client_git('fsck', '--strict')
        native = self.metadata('find-git-oid', {'oid': list(bytes.fromhex(head))})['id']
        for path, content in [('argo-images/kustomization.yaml', argo), ('flux-images/deployment.yaml', flux)]:
            actual = self.metadata('read-file', {'id': native, 'path': list(path.encode())})
            if bytes(actual['content']).decode().strip() != content:
                raise RuntimeError('native file differs from Git write-back: ' + path)
        return {'baseline': baseline, 'head': head, 'commits': commits,
                'argocdKustomization': argo, 'fluxDeployment': flux,
                'nativeChangeset': bytes(native).hex(), 'nativeFilesMatchGit': True,
                'gitFsckStrict': True, 'deployedImage': IMAGE + ':1.0.1'}

    def run(self):
        result = {'success': False, 'versions': VERSIONS}
        try:
            context = self.kubectl('config', 'current-context').stdout.decode().strip()
            if context != f'kind-{self.args.cluster}':
                raise RuntimeError('kubeconfig is not the dedicated test cluster')
            previous = json.loads((self.evidence / 'result.json').read_text())
            if not previous.get('success') or not (self.evidence / 'webhook-proof.json').exists():
                raise RuntimeError('requires successful --webhooks run in this workdir')
            self.registry()
            self.updater()
            with self.forward('kasu-registry', 5000) as registry_port, self.forward('kasu', 50080) as git_port:
                self.git_port = git_port
                self.publish(registry_port, '1.0.0')
                baseline = self.seed(git_port)
                self.resources()
                started = time.monotonic()
                self.publish(registry_port, '1.0.1')
                proof = self.wait('both controllers write and deploy image tag', lambda: self.verify(baseline), timeout=300)
                self.wait_hook_ack(5)
                proof['acknowledgedNativeSequence'] = 5
                result.update(success=True, elapsedSeconds=round(time.monotonic() - started, 3), **proof)
        finally:
            self.collect()
            for name, resource, namespace in [
                ('image-updater', 'deployment/argocd-image-updater-controller', 'argocd'),
                ('image-automation', 'deployment/image-automation-controller', 'flux-system'),
                ('image-reflector', 'deployment/image-reflector-controller', 'flux-system')]:
                response = self.kubectl('logs', resource, '-n', namespace, check=False)
                (self.evidence / f'{name}.txt').write_bytes(response.stdout + response.stderr)
            version = self.kubectl('exec', 'deployment/argocd-image-updater-controller', '-n', 'argocd',
                                   '--', 'git', '--version', check=False)
            (self.evidence / 'image-updater-git-version.txt').write_bytes(version.stdout + version.stderr)
            (self.evidence / 'writeback-result.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workdir', required=True)
    parser.add_argument('--kubectl', default='kubectl')
    parser.add_argument('--cluster', default='kasu-gitops-e2e')
    parser.add_argument('--timeout', type=int, default=600)
    Writeback(parser.parse_args()).run()
