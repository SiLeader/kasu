# Multiple Repositories

English | [日本語](multi-repository_ja.md)

One kasu-server (a cluster sharing the same MetaStore / BlobStore) hosts multiple repositories.
The design is [06. Multiple repositories](plan/06-multi-repository.md).
Repositories do not share permissions, refs, Git OID indexes, configuration, or webhooks.
Immutable objects (Blob / Tree / Changeset / Derived Data) remain content-addressed and shared in storage,
and each repository can handle only the objects in its own membership index.

## Names and Keys

| Term | Contents |
|---|---|
| Repository name | 1–4 `/`-separated segments, each `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`, at most 256 bytes in total. A `.git` suffix and the first segments `-`, `admin`, `api`, `metrics`, and `v1` are not allowed. Uniqueness is judged in ASCII lowercase; display uses the spelling at registration |
| `RepoKey` | Immutable internal identifier. New repositories get `r1\0` + 16 random bytes, never reused. refs, ref_log, Land results, webhook cursors, membership indexes, Git OID indexes, and read-scope namespaces are all stored under this key |
| legacy key | The single repository from before the registry is registered with its name bytes as the key. Existing keys are not rewritten |

After a rename, the old name remains as an alias with an expiry, and old URLs and RPCs with the old name resolve to the renamed repository.
While the alias is alive, the old name cannot be used to create a new repository. The expiry is `KASU_REPOSITORY_ALIAS_TTL_SECS` (default 30 days, `0` for no alias).

The registry is stored in `repository_records` / `repository_names` / `repository_log` in SQLite and PostgreSQL.
Name uniqueness and record updates happen in the same transaction, and every change is appended to `repository_log`.
The server caches the registry with a short TTL (`KASU_REGISTRY_CACHE_MS`, default 1000 ms)
and follows `repository_log` every 250 ms to invalidate changes immediately.
Ref CAS, Land commits, and Git push publication recheck the repository state within the same transaction.
Even with a stale cache, writes to a repository that is not Active never commit.

## Startup and Migration

```sh
# Register an existing single-repository environment (idempotent). The name bytes become the legacy key.
kasu-server migrate-registry /srv/kasu demo

# Serve every registered repository.
KASU_TOKEN_FILE=/etc/kasu/operator.pat kasu-server /srv/kasu [grpc-address] [http-address]

# Legacy form. Treats demo as the legacy repository and also serves other registered repositories.
KASU_TOKEN_FILE=/etc/kasu/operator.pat kasu-server /srv/kasu demo
```

With the legacy form and an empty registry, the name in the arguments is automatically registered as the legacy repository (a compatibility mode for one release).
If the registry is not empty and the name is unregistered, startup aborts.
Repository names never contain `:`, so it is never ambiguous whether the second argument is an address or a name.
The legacy repository is the target of old-format accounts without `grants`, projection accounts with only `directories`,
and the endpoints in `KASU_WEBHOOKS_FILE`.
Existing Git OIDs, Derived Data keys, and webhook cursors are unchanged after migration.
A legacy key that has been deleted cannot be re-registered with `migrate-registry`.

`kasu-git-ssh <kasu-directory> <credential-file>` resolves the repository from the client's command (`git-upload-pack 'team/app.git'`).
The legacy `kasu-git-ssh <kasu-directory> <repository-name> <credential-file>` serves only the given repository.

## Per-repository Configuration

`KASU_REPOSITORIES_FILE` declares repositories and their configuration. It is synced to the registry at startup, on SIGHUP,
and every `KASU_REPOSITORIES_POLL_MS` (default 30 seconds).
Unregistered repositories are created with new keys; registered repositories missing from the declaration only produce a warning and are not deleted.

```json
{
  "repositories": [
    {
      "name": "team-a/infra",
      "default_branch": "main",
      "write_policy": { "allowed_ref_patterns": ["refs/heads/main", "refs/heads/automation/*"] },
      "webhooks": [
        { "id": "argocd", "url": "https://argocd.example.com/api/webhook", "secret_file": "secrets/argocd" }
      ],
      "limits": { "active": 8, "pending": 64 }
    },
    { "name": "team-b/app" }
  ]
}
```

| Setting | Handling |
|---|---|
| `write_policy` | Server-wide (`KASU_WRITE_POLICY_FILE`) ∧ repository ∧ account grant. None widens the others |
| `webhooks` | Endpoints notified of this repository's ref changes. Relative `secret_file` paths are based on the file's location |
| `limits` | This repository's concurrency and queue size. Capped by the server-wide `per_repository_*` |
| `default_branch` | Git HEAD symref and the webhook `default_branch` |
| `review` | Protected refs and approval rules; see [reviews](review.md). Omitted, nothing is protected |

With `KASU_REPOSITORIES_SOURCE=file` (the default), the file is authoritative, and changes to settings / default branch through the Admin API are rejected.
With `registry`, the file is used only to create unregistered repositories, and settings are managed through the Admin API.
Either way, concurrent changes are detected with a compare-and-swap on `config_version`.

## Authentication and Authorization

Accounts hold per-repository permissions in `grants`. The old-format `write_policy` / `read_directories` are
translated into a single grant on the legacy repository. Both cannot be written on the same account.

```json
{
  "accounts": [
    {
      "name": "deploy-bot",
      "token_file": "secrets/deploy-bot.pat",
      "grants": [
        { "repositories": ["team-a/*"],
          "write_policy": { "allowed_ref_patterns": ["refs/heads/automation/*"], "allowed_path_prefixes": ["deploy"] } },
        { "repositories": ["shared/charts"], "read_directories": ["charts/team-a"] }
      ]
    }
  ]
}
```

- `repositories` are case-insensitive globs. `*` matches within one segment, and a `**` segment matches one or more segments.
- When multiple grants match, the single most specific one is used (no wildcards > more literal segments > more literal characters > fewer `**`, ties go to the earlier grant). Permissions are never unioned.
- Grants are matched against the resolved repository's current name and past primary names, not the name used in the request. Renaming does not drop permissions, and alias strings are never used for matching.
- Repositories without a grant get `NotFound` (404 for Git HTTP), indistinguishable from nonexistent repositories. The operator PAT can use every repository and the Admin API.
- Projection-only accounts specify `(repository, path)` with `"projections": [{"repository": "team-a/*", "directories": ["deploy"]}]`. The old-format `directories` are projections of the legacy repository.
- The cache → server hop credential can fetch only repositories in `KASU_OBJECT_REPOSITORIES` (comma-separated globs, default `**`).

## State and Hiding Existence

| State | Behavior |
|---|---|
| `ACTIVE` | Every operation is available |
| `READ_ONLY` | Reads and webhooks continue. Writer RPCs return `FailedPrecondition` (HTTP 412), and receive-pack returns `ng ... repository is read-only` per ref |
| `ARCHIVED` | In addition to `READ_ONLY`, webhook delivery stops |
| `DELETING` | The name and aliases are released immediately, and every RPC returns `NotFound` |

Objects that exist only in other repositories produce the same error as nonexistent objects.
UploadBlobs' "skip existing" is decided by membership in the target repository, so even a blob that exists in another repository must have its body sent.
After verifying the hash of the received body, only membership is added, and the stored frame is reused.
CreateChangeset parents and blobs, Merge / Rebase inputs, and receive-pack thin pack delta bases and connectivity
are all resolved only through membership in the target repository.

## Admin API

`kasu.v1.Admin` is provided over gRPC and HTTP JSON (`/v1/admin/<method>`).

| RPC | HTTP | Description |
|---|---|---|
| `CreateRepository` | `/v1/admin/create-repository` | Creates with an unborn HEAD. Resending with the same `idempotency_key` and contents gives `replayed=true` |
| `GetRepository` | `/v1/admin/get-repository` | Only repositories visible to the caller. `config_json` is returned only to operators and accounts with `admin` on the repository |
| `ListRepositories` | `/v1/admin/list-repositories` | Paging with `prefix` and `cursor` (default 100, max 1000) |
| `UpdateRepository` | `/v1/admin/update-repository` | Changes `config_json`, `state`, and `default_branch` with a CAS on `expected_config_version` |
| `RenameRepository` | `/v1/admin/rename-repository` | `RepoKey` is unchanged. `keep_alias` keeps the old name as an alias |
| `DeleteRepository` | `/v1/admin/delete-repository` | Transitions to `DELETING`. Data deletion is asynchronous |
| `ForkRepository` | `/v1/admin/fork-repository` | Adds membership of the history, trees, blobs, and raw Git objects reachable from the given ref to a new key (bodies are not copied). Resending with the same idempotency key resumes after interruption. Commits shared with the source have the same Git OIDs |
| `GetRepositoryUsage` | `/v1/admin/repository-usage` | Logical size of objects reachable from native refs. Cached per ref state. Shared objects are counted in full for each repository |

Operators may do everything. Accounts may administer through their grants:

| Operation | Account requirement |
|---|---|
| `CreateRepository` | The name matches the account's `create_repositories` |
| `UpdateRepository`, `DeleteRepository` | The grant that applies to the repository has `admin: true` |
| `RenameRepository` | `admin` on the repository and the new name matches `create_repositories` |
| `ForkRepository` | Unrestricted read access to the source and the new name matches `create_repositories` |

Repository configuration never widens server-wide policy. Because `webhooks[].secret_file` names a file on the server, non-operators may only name files below `KASU_WEBHOOK_SECRET_DIR` (an absolute directory; relative names are resolved against it, `..` is rejected); without it they cannot set secret files. Delegated requests ([Web UI](web-ui.md)) administer only when the delegate has `allow_admin`; anonymous delegation never does.
Conflicts return `AlreadyExists` / `Aborted` (HTTP 409).

## Deletion and Storage Reclamation

Deletion has two phases. `DeleteRepository` transitions to `DELETING`, stopping all access and releasing the name.
A background purge (`KASU_PURGE_INTERVAL_MS`, default 60 seconds) deletes refs, Git refs, Land results, webhook state,
membership indexes, Git OID indexes, and publication / reachability indexes. The ref_log is kept for auditing for
`KASU_DELETED_REF_LOG_RETENTION_SECS` (default 90 days) before deletion.
`RepoKey`s are never reused, so recreating with the same name never shows old data.

Physical reclamation of shared immutable objects (blob / tree / changeset bodies and Derived Data) is out of scope.
Indexes that cannot be enumerated by key (directory read-scope membership, the read root cache, per-projection Derived Data)
also remain, but no repository references them. When needed, an offline mark-and-sweep rooted at every repository's membership index
will be added as a separate tool.

## Multi-tenant Operation

- **Fairness**: Admission limits concurrency per repository (`ServerLimits.per_repository_active`, default half of `active`) and queue size (`per_repository_pending`, default 1/4 of `pending`). Requests waiting on a busy repository give back the server-wide execution slot while waiting, so they do not starve other repositories. This is applied as an intersection with IP / PAT limits.
- **Metrics**: `kasu_server_repository_requests_total` / `_rejected_total` / `_active` use the `repository` label only for repositories in `KASU_METRICS_REPOSITORIES` (comma-separated names, up to 256); everything else is aggregated into `other`.
- **Tracing**: The server span's `kasu.repository` attribute records the `RepoKey` (`r1-<hex>` or `legacy-<name>`).

## Webhook

`KASU_WEBHOOKS_FILE` provides delivery settings (polling interval, timeout, retries, body limit) and `public_url`,
and its `endpoints`, if any, become endpoints of the legacy repository. Per-repository endpoints are specified in
`KASU_REPOSITORIES_FILE` or through Admin API settings.
A supervisor starts a dispatcher per Active / ReadOnly repository and recreates it on rename, configuration, or state changes.
Cursors are stored per repository key, so recreation does not redeliver.
The payload's `repository.name` / `full_name` / `clone_url` are generated from the current name
(`team-a/infra` gives `full_name="team-a/infra"`, `name="infra"`; single-segment names remain `kasu/<name>` as before).
`clone_url` is `KASU_PUBLIC_URL` (or `public_url`) + `/<name>.git`.

## Clients

- `kasu clone <server> <repo> [destination]` checks out the default branch (`main` if it cannot be obtained). The working copy records the server URL and repository name.
- `kasu repo list [--prefix P]` / `kasu repo create <name> [--default-branch B]` / `kasu repo show <name>` use the Admin API (`--server` / `KASU_SERVER`).
- `kasu-ingest --create <git-repository> <kasu-directory> <name>` creates an unregistered repository and then imports. Specifying an unregistered name without `--create` is an error (only with an empty registry is it registered as legacy). Imports into different repositories can run concurrently.
- `kasu-cache` / `kasu-daemon` handle each request's `repo`. `--repo` is an optional allow list that can be repeated; other names get `NotFound`. Frames are shared by ID, and only `(repo, id)` pairs whose membership in that repository the upstream has confirmed once are hits. Membership is stored in the disk catalog and snapshots, and if the repository identifier returned by the server (`x-kasu-repository`) changes for a name, membership for that name is discarded. `kasu-daemon snapshot --repo A --repo B` selects target repositories. Catalogs and snapshots from the single-repository era are loaded as membership of that repository.

## Verification

| Kind | Tests |
|---|---|
| Registry | `kasu-store/tests/registry.rs` (shared contract for memory, SQLite, and PostgreSQL; in-Tx state recheck; range deletion) |
| Isolation (property) | `kasu-server/tests/isolation.rs` (Metadata / Objects / Git upload-pack with random repositories, shared blobs, and forks) |
| Existence oracles | `kasu-server/tests/multi_repository.rs` (UploadBlobs, parents, thin pack delta bases) |
| Lifecycle | `kasu-server/tests/admin.rs`, `kasu-sim/tests/repository_lifecycle.rs` (Land concurrent with deletion) |
| Migration | `migrating_a_single_repository_keeps_every_key_and_git_oid` |
| Authorization | `grants_choose_the_most_specific_match_and_hide_other_repositories`, unit tests in `accounts.rs` |
| Fairness | Unit tests in `admission.rs`, `kasu-sim/tests/repository_fairness.rs` |
| cache / CLI / ingest | `kasu-cache/tests/repositories.rs`, `kasu-cli/tests/repositories.rs`, `kasu-ingest/tests/roundtrip.rs` |
| Process | `kasu-server/tests/multi_repository_process.rs` (real server binary, declaration file, signed webhooks, `git clone`) |
| E2E | `tools/gitops/multi_repository.py` ([GitOps on kind](gitops-e2e.md#multiple-repositories)) |
