#!/usr/bin/env python3
"""ArgoCD / Flux acceptance test against a read-only path projection URL."""
from contextlib import contextmanager
import base64
import datetime
import json
import re
import subprocess
import time

from run import Test, TOKEN, argument_parser, command


class ProjectionTest(Test):
    def __init__(self, args):
        super().__init__(args)
        self.repo_url = 'http://kasu.gitops-test.svc.cluster.local:50080/demo/-/path/deploy.git'
        self.fixture_prefix = self.fixture_prefix / 'deploy'

    @contextmanager
    def forwarded_repository(self):
        log = self.work / 'port-forward.log'
        with log.open('wb') as output:
            process = subprocess.Popen([
                str(self.args.kubectl), '--kubeconfig', str(self.kubeconfig),
                'port-forward', '-n', 'gitops-test', 'service/kasu', ':50080',
                '--address=127.0.0.1',
            ], stdout=output, stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    match = re.search(r'Forwarding from 127\.0\.0\.1:(\d+) ->', log.read_text())
                    if match:
                        yield f'http://127.0.0.1:{match[1]}/demo/-/path/deploy.git'
                        return
                    if process.poll() is not None:
                        raise RuntimeError('port-forward exited: ' + log.read_text())
                    time.sleep(0.2)
                raise RuntimeError('port-forward did not become ready')
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)

    def inspect_projection(self, url, stage):
        # Pass credentials in the environment, outside command/packet logs.
        env = self.git_env.copy()
        auth = base64.b64encode(f'kasu:{TOKEN}'.encode()).decode()
        env.update(GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='http.extraHeader',
                   GIT_CONFIG_VALUE_0=f'Authorization: Basic {auth}')
        checkout = self.work / 'projected-client'
        if checkout.exists():
            command(['git', '-C', checkout, 'fetch', 'origin'], env=env)
        else:
            command(['git', 'clone', '--no-checkout', url, checkout], env=env)
        head = self.git(checkout, 'rev-parse', 'refs/remotes/origin/main')
        source = self.work / 'source'
        source_head = self.git(source, 'rev-parse', 'main')
        expected = self.git(source, 'ls-tree', '-r', 'main:deploy')
        actual = self.git(checkout, 'ls-tree', '-r', head)
        if actual != expected or head == source_head:
            raise RuntimeError('projection must expose exactly deploy/ with its own commit OID')
        command(['git', '-C', checkout, 'fsck', '--strict'], env=self.git_env)
        proof = {'stage': stage, 'source': source_head, 'projected': head,
                 'tree': self.git(checkout, 'rev-parse', head + '^{tree}'),
                 'files': actual.splitlines(), 'gitFsckStrict': True,
                 'sourceSubtreeMatches': True}
        (self.evidence / f'projection-{stage}.json').write_text(json.dumps(proof, indent=2) + '\n')
        return proof

    def update_and_wait_refresh(self, archive):
        before = (self.get('application', 'kasu', 'argocd') or {}).get('status', {}).get('reconciledAt')
        self.update(archive)
        requested = (self.get('gitrepository', 'kasu', 'flux-system') or {}).get(
            'metadata', {}).get('annotations', {}).get('reconcile.fluxcd.io/requestedAt')
        deadline = time.monotonic() + self.args.timeout
        while time.monotonic() < deadline:
            app = self.get('application', 'kasu', 'argocd') or {}
            source = self.get('gitrepository', 'kasu', 'flux-system') or {}
            reconciled = app.get('status', {}).get('reconciledAt')
            handled = source.get('status', {}).get('lastHandledReconcileAt')
            if (reconciled and reconciled != before
                    and 'argocd.argoproj.io/refresh' not in app.get('metadata', {}).get('annotations', {})
                    and requested and handled == requested):
                return {'argocdReconciledBefore': before, 'argocdReconciledAfter': reconciled,
                        'fluxRequestedAt': requested, 'fluxHandledAt': handled}
            time.sleep(2)
        raise RuntimeError('controllers did not acknowledge the explicit refresh')

    def run(self):
        result = {'started': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  'success': False, 'projection': 'deploy', 'repositoryUrl': self.repo_url}
        try:
            if self.args.webhooks:
                raise ValueError('projection acceptance uses explicit refresh; omit --webhooks')
            if (self.work / 'source').exists() or self.args.reuse:
                raise ValueError('projection acceptance requires a fresh workdir and cluster')
            self.prepare()
            (self.work / 'source').mkdir()
            outside = self.work / 'source/private.txt'
            outside.write_text('outside projection: one\n')
            _, archive = self.fixture('one')
            self.gateway(archive)
            self.controllers()
            with self.forwarded_repository() as url:
                initial = self.inspect_projection(url, 'one')
                self.wait_sync(initial['projected'], 'one')

                outside.write_text('outside projection: two\n')
                _, archive = self.fixture('one')
                outside_refresh = self.update_and_wait_refresh(archive)
                unchanged = self.inspect_projection(url, 'outside')
                if (unchanged['source'] == initial['source']
                        or unchanged['projected'] != initial['projected']):
                    raise RuntimeError('outside-only update must advance source but preserve projection OID')
                self.wait_sync(unchanged['projected'], 'one', label='outside')

                _, archive = self.fixture('two')
                inside_refresh = self.update_and_wait_refresh(archive)
                updated = self.inspect_projection(url, 'two')
                if updated['projected'] == initial['projected']:
                    raise RuntimeError('inside update must advance projection OID')
                self.wait_sync(updated['projected'], 'two')
                result.update(success=True, initial=initial['projected'], updated=updated['projected'],
                              outside=unchanged['projected'], stages=[initial, unchanged, updated],
                              refreshes={'outside': outside_refresh, 'two': inside_refresh})
        finally:
            self.collect()
            result['finished'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            (self.evidence / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        if not self.args.keep:
            command([self.args.kind, 'delete', 'cluster', '--name', self.args.cluster], timeout=120)


if __name__ == '__main__':
    parser = argument_parser()
    parser.description = __doc__
    parser.set_defaults(cluster='kasu-projection-e2e')
    ProjectionTest(parser.parse_args()).run()
