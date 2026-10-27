# GitOps Acceptance Tests on kind

English | [日本語](gitops-e2e_ja.md)

`tools/gitops/run.py` starts kasu, ArgoCD, and Flux in a dedicated kind cluster.
Versions and the SHA-256 of the official installation manifests are pinned in
[`versions.json`](../tools/gitops/versions.json).

Requires Docker, Git, Python 3, kind v0.33.0, and kubectl v1.37.0.
Allocate enough memory and disk to Docker, including for the controllers.
The first run builds Rust and pulls container images.

```sh
python3 tools/gitops/run.py --workdir /tmp/kasu-gitops-e2e
```

It stops if a cluster with the same name already exists. The kubeconfig is created inside the workdir
and passed explicitly to every kubectl call. On success the dedicated cluster is deleted;
on failure or with `--keep`, it is kept for investigation.
Only to reuse an environment this test created, pass the same workdir and `--reuse`.
`--skip-build` uses a prebuilt `kasu:gitops-e2e` image.

`--webhooks` extends ArgoCD / Flux periodic polling to 1 hour and sends signed
GitHub push webhooks to ArgoCD `/api/webhook` and the Flux Receiver.
For the ref update after the initial sync, no manual refresh is used; it checks that the new SHA / ConfigMap are reflected within 60 seconds,
that both endpoints return 2xx, that ArgoCD logs a webhook refresh, and that Flux's reconcile request is updated.
The results are stored in `evidence/webhook-proof.json`.

## Image Updater / Image Automation Write-back

Keep the dedicated cluster after the webhook verification with `--keep`, and run with the same workdir.

```sh
python3 tools/gitops/run.py --workdir /tmp/kasu-gitops-e2e --keep --webhooks
python3 tools/gitops/writeback.py --workdir /tmp/kasu-gitops-e2e
```

This uses pinned Argo CD Image Updater v1.4.0 and Flux image controllers.
A digest-pinned OCI registry is started in the cluster, and a pinned BusyBox image is registered
with tag `1.0.0`. After the initial manifests are stored in kasu with a Git push,
`1.0.1` is added. Image Updater changes a Kustomization and Flux changes a Deployment with setter comments,
and each actually commits / pushes.
Image Updater reuses ArgoCD repository credentials
(the [pinned version's explicit Secret authentication path](https://github.com/argoproj-labs/argocd-image-updater/blob/v1.4.0/pkg/argocd/gitcreds.go#L142) does not accept HTTP URLs).

The only commit the client creates is the initial fixture. From the subsequent Git history it checks both bots' commits,
updated files, `git fsck --strict`, matching native contents via the Metadata API, confirmed webhook delivery, the new image in both Deployments,
and ArgoCD's synced SHA, storing them in `evidence/writeback-result.json`.
Deployments use `replicas: 0`; what is verified is Git write-back and reflection in Kubernetes manifests.
Periodic polling is 1 hour, and no manual refresh is done after the tag is added.
Images are registered from the host over OCI HTTP so this also works when Docker runs inside a VM.

A workdir that has already created `writeback-client` is rejected on rerun.
After investigating a failed environment, rerun all steps with a dedicated cluster and a new workdir.

Verification proceeds in this order.

1. Import a small Git fixture into kasu's native store.
   The Git repository lives in the init container's temporary area and is not passed to the serving process.
2. Configure an ArgoCD Application and Flux GitRepository / Kustomization with a test token over HTTP Basic.
3. Check both controllers' status, the commit SHA, and the contents of the created ConfigMap.
4. Update the fixture and reimport it into the store shared with the running kasu, advancing the ref.
5. Check under the same conditions that both controllers reflected the new SHA and new ConfigMap.

`<workdir>/evidence/` stores the Kubernetes resources at each sync, versions, real image IDs,
events, controller logs, and Git pkt-lines.
`success: true` in `result.json` means both stages succeeded.
This environment uses test-only fixtures and tokens. The kubeconfig and Secrets are not included in the artifacts.
Pack bodies and HTTP authentication headers are not included in traffic logs.

[`gitops.yml`](../.github/workflows/gitops.yml), for nightly and manual runs,
runs the same runner on Linux amd64 and uploads evidence even on failure.
Results of runs on GitHub need separate confirmation.

## Path Projection URLs

```sh
python3 tools/gitops/projection.py --workdir /tmp/kasu-projection-gitops-e2e
python3 tools/gitops/record.py --evidence /tmp/kasu-projection-gitops-e2e/evidence \
  --output crates/kasu-git/tests/fixtures/clients/path-projection-kind
```

This creates a dedicated cluster `kasu-projection-e2e` and sets both controllers' repository URL to
`http://kasu.gitops-test.svc.cluster.local:50080/demo/-/path/deploy.git`.
The source repository's `deploy/argo` and `deploy/flux` become `argo` and `flux` within the projection.
Another file is placed outside the projection.

Three stages are checked: initial sync, an update only outside the projection, and a ConfigMap update inside the projection.
At each stage a real Git client clones / fetches, checks the mode, OID, and path of every file against
the source repository's `deploy/`, and runs `git fsck --strict`.
It confirms that both tools sync to a projected SHA different from the source SHA, that an outside update changes only the source SHA,
and that an inside update reflects the new projected SHA and ConfigMap value in both tools.
Acceptance of explicit refreshes is also confirmed through ArgoCD's `reconciledAt` and Flux's `lastHandledReconcileAt`.

`evidence/projection-{one,outside,two}.json` stores the Git check results,
`sync-{one,outside,two}.json` the state of both controllers and ConfigMaps,
and `result.json` the source / projected SHAs and refresh acceptance records.
This test uses explicit refreshes; webhooks are verified in the normal-URL scenario.
Nightly / manual CI runs normal-URL write-back and path projections as separate matrix jobs.

On 2026-10-24 (JST), these three stages passed on Docker / kind on macOS arm64.
The source SHA advanced `8b2e4029…` → `56f1f25e…` → `7638c1d2…`, and the projected SHA went
`e744c005…` → `e744c005…` → `1f2f1220…`.
Refresh acceptance for both updates, both controllers' sync and ConfigMaps, and subtree equality / strict fsck at all three stages are stored in the
[results and real image IDs](../crates/kasu-git/tests/fixtures/clients/path-projection-kind/gitops-kind-summary.json),
[ArgoCD traffic](../crates/kasu-git/tests/fixtures/clients/path-projection-kind/argocd-3.5.6-kind.trace), and
[server-side traffic](../crates/kasu-git/tests/fixtures/clients/path-projection-kind/gitops-kind-server.trace).
Results of this additional scenario on GitHub are unconfirmed.

## Multiple Repositories

`tools/gitops/multi_repository.py` serves two repositories (`team-a/apps`, `team-b/apps`) from one kasu-server
and syncs each with a separate ArgoCD Application and Flux GitRepository / Kustomization.

```sh
python3 tools/gitops/multi_repository.py --workdir /tmp/kasu-gitops-multi
```

Repositories are created by `kasu-ingest --create` in an init container, and the server starts without a repository argument.
`KASU_REPOSITORIES_FILE` declares per-repository webhooks (ArgoCD `/api/webhook` and a Flux Receiver per repository),
and the payload's `clone_url` is built from `KASU_PUBLIC_URL`. ArgoCD and Flux poll every hour.
Only one repository is updated, and it checks that within 90 seconds both controllers for that repository sync to the new SHA and ConfigMap,
that the other stays at its previous revision, and that kasu's logs show confirmed deliveries increasing only for the updated repository's two endpoints.
ArgoCD chooses Applications by the payload's repository URL,
so a successful sync shows that the payload named the correct repository. This is done for both repositories in turn
and recorded in `evidence/multi-repository-proof.json`. Other rules (dedicated cluster, `--reuse`, `--keep`) are the same as `run.py`.

On 2026-10-26 this ran successfully on Docker on Linux x86_64. Because the pinned kind v0.33.0 was not available,
`--unpinned-kind` was used with the installed kind v0.29.0 and the default node image (Kubernetes v1.33.1)
(ArgoCD v3.5.6 / Flux v2.9.6 are pinned). `team-a/apps` and then `team-b/apps` were updated; about 5.7 seconds after each update,
only that repository's ArgoCD / Flux synced to the new SHA, and confirmed deliveries increased only for the updated repository's two endpoints.
This is stored in the [evidence and environment](verification/gitops-multi-repository/).
`--unpinned-kind` removes the kind version pin and records the actual version in `evidence/kind-version.txt`.

## Measurements on 2026-10-23

A dedicated kind v0.33.0 / Kubernetes v1.37.0 cluster was created on Docker on macOS arm64,
and both stages of syncing with ArgoCD v3.5.6 and Flux v2.9.6 passed.
Re-verification with signed webhooks also showed both syncing within 5.602 seconds of the update without manual refresh.
The polling interval was 1 hour, ArgoCD's webhook refresh count went from 1 to 2,
and the Flux Receiver's reconcile request update and confirmed delivery of native log sequence 2 were also observed.
The subsequent write-back E2E in a clean environment also passed. 22.486 seconds after the tag was added,
both Image Updater / Flux Automation commits and pushes, the Deployment image update,
matching native contents, strict fsck, and confirmed webhook delivery of sequence 5 were confirmed.
The [write-back results](../crates/kasu-git/tests/fixtures/clients/gitops-kind-writeback.json) store
the real commit SHAs and native Changeset IDs. Git in the Image Updater container is 2.54.0.
Git in the ArgoCD container is 2.53.0, and Flux source-controller / kustomize-controller are v1.9.6.
Flux's [pinned source go.mod](https://github.com/fluxcd/source-controller/blob/v1.9.6/go.mod)
specifies go-git v5.19.1. The agent on the wire is `go-git/5.x`.

The initial SHA was `60be29eea4594d7b543fcfa4a70935093f70a20c`,
and the updated SHA was `6e1773d9c0ad7af349d31c76b3f09a09ed15721b`.
The [results and real image IDs](../crates/kasu-git/tests/fixtures/clients/gitops-kind-summary.json),
[ArgoCD traffic](../crates/kasu-git/tests/fixtures/clients/argocd-3.5.6-kind.trace), and
[server-side traffic including both controllers](../crates/kasu-git/tests/fixtures/clients/gitops-kind-server.trace) are stored.
Regenerating the fixtures without timestamps:

```sh
python3 tools/gitops/record.py --evidence /tmp/kasu-gitops-e2e/evidence \
  --output crates/kasu-git/tests/fixtures/clients
```
