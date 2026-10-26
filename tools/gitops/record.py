#!/usr/bin/env python3
"""Normalize successful test evidence into the versioned compatibility fixtures."""
import argparse
import json
from pathlib import Path
import re


def record(source, output):
    result = json.loads((source / 'result.json').read_text())
    if result.get('success') is not True:
        raise ValueError('only successful evidence can be recorded')
    if result.get('projection'):
        stages = result.get('stages', [])
        refreshes = result.get('refreshes', {})
        if (len(stages) != 3 or [s['stage'] for s in stages] != ['one', 'outside', 'two']
                or stages[0]['source'] == stages[1]['source']
                or stages[0]['projected'] != stages[1]['projected']
                or stages[1]['projected'] == stages[2]['projected']
                or any(s.get('gitFsckStrict') is not True
                       or s.get('sourceSubtreeMatches') is not True for s in stages)
                or set(refreshes) != {'outside', 'two'}):
            raise ValueError('path projection proof is incomplete')
        for refresh in refreshes.values():
            if (not refresh.get('fluxRequestedAt')
                    or refresh['fluxRequestedAt'] != refresh.get('fluxHandledAt')
                    or not refresh.get('argocdReconciledAfter')
                    or refresh['argocdReconciledAfter'] == refresh.get('argocdReconciledBefore')):
                raise ValueError('path projection refresh was not acknowledged')
    output.mkdir(parents=True, exist_ok=True)
    snapshots = []
    for marker in (['one', 'outside', 'two'] if result.get('projection') else ['one', 'two']):
        data = json.loads((source / f'sync-{marker}.json').read_text())
        snapshots.append({
            'head': data['head'], 'marker': data['marker'],
            'argocd': {k: data['argocd']['status'][k] for k in ['health', 'sync']},
            'fluxSource': {k: data['fluxSource']['status'][k] for k in ['artifact', 'conditions']},
            'fluxKustomization': {k: data['fluxKustomization']['status'][k]
                                  for k in ['lastAppliedRevision', 'conditions']},
            'argoConfigMap': data['argoConfigMap']['data'],
            'fluxConfigMap': data['fluxConfigMap']['data'],
        })
    pods = json.loads((source / 'pods.json').read_text())
    images = sorted({(container['image'], container['imageID'])
                     for pod in pods['items']
                     if pod['metadata']['namespace'] in ['argocd', 'flux-system', 'gitops-test']
                     for container in pod.get('status', {}).get('containerStatuses', [])})
    summary = {
        'result': result,
        'versions': json.loads((source / 'versions.json').read_text()),
        'kubernetes': json.loads((source / 'kubernetes.json').read_text()),
        'argocdGit': (source / 'argocd-git-version.txt').read_text().strip(),
        'images': [{'image': image, 'imageID': digest} for image, digest in images],
        'syncs': snapshots,
    }
    webhook = source / 'webhook-proof.json'
    if webhook.exists():
        proof = json.loads(webhook.read_text())
        if (proof.get('manualRefreshAfterUpdate') is not False
                or proof.get('pollInterval') != '1h'
                or proof.get('acknowledgedNativeSequence') != 2
                or proof['argocdWebhookRefreshesAfter'] <= proof['argocdWebhookRefreshesBefore']
                or proof['fluxRequestedAtAfter'] == proof['fluxRequestedAtBefore']):
            raise ValueError('webhook refresh proof is incomplete')
        summary['webhooks'] = proof
    writeback = source / 'writeback-result.json'
    if writeback.exists():
        proof = json.loads(writeback.read_text())
        if (proof.get('success') is not True or proof.get('gitFsckStrict') is not True
                or proof.get('nativeFilesMatchGit') is not True
                or proof.get('acknowledgedNativeSequence') != 5):
            raise ValueError('controller write-back proof did not succeed')
        proof['imageUpdaterGit'] = (source / 'image-updater-git-version.txt').read_text().strip()
        (output / 'gitops-kind-writeback.json').write_text(json.dumps(proof, indent=2) + '\n')
    (output / 'gitops-kind-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    argo = (source / 'argocd-packets.txt').read_text()
    if 'packet:' not in argo:
        raise ValueError('ArgoCD packet capture is missing')
    argo = re.sub(r'^\d{2}:\d{2}:\d{2}\.\d+\s+', '', argo, flags=re.MULTILINE)
    (output / 'argocd-3.5.6-kind.trace').write_text(argo)
    server = (source / 'kasu-packets.txt').read_text()
    server = '\n'.join(re.sub(r'^\S+\s+TRACE\s+', '', line)
                       for line in server.splitlines() if ' TRACE ' in line) + '\n'
    if 'agent=go-git/5.x' not in server or 'protocol=V2' not in server:
        raise ValueError('expected go-git and Git v2 packets are missing')
    (output / 'gitops-kind-server.trace').write_text(server)
    if webhook.exists():
        for component in ['argocd-server', 'flux-notification']:
            (output / f'gitops-kind-{component}.log').write_text(
                (source / f'{component}.txt').read_text())
    if writeback.exists():
        for component in ['image-updater', 'image-automation', 'image-reflector']:
            (output / f'gitops-kind-{component}.log').write_text(
                (source / f'{component}.txt').read_text())


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--evidence', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    record(args.evidence, args.output)
