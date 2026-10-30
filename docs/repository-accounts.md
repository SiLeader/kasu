# Repository Accounts and Write ACLs

English | [日本語](repository-accounts_ja.md)

`KASU_ACCOUNTS_FILE` configures the write scope per PAT.
It applies equally to Git Smart HTTP / SSH and to native Writer gRPC / HTTP JSON.
By default, accounts can read the whole repository.
`read_directories` limits the [native read scope](directory-read-acl.md).
For Git connections with reads limited to directories, use the existing [projection-only accounts](projection-accounts.md).
Scoped accounts can read allowed Git path projections, while reads / pushes on the normal Git URL are rejected.

## Configuration

```json
{
  "accounts": [
    {
      "name": "deploy-bot",
      "token_file": "secrets/deploy-bot.pat",
      "write_policy": {
        "allowed_ref_patterns": ["refs/heads/main", "refs/heads/automation/*"],
        "allowed_path_prefixes": ["deploy", "charts/team-a"],
        "force_ref_patterns": ["refs/heads/automation/*"],
        "allow_create": false,
        "allow_delete": false
      }
    },
    {
      "name": "repository-reader",
      "token_file": "secrets/repository-reader.pat"
    }
  ]
}
```

```sh
KASU_TOKEN_FILE=/etc/kasu/operator.pat \
KASU_ACCOUNTS_FILE=/etc/kasu/accounts.json \
KASU_WRITE_POLICY_FILE=/etc/kasu/write-policy.json \
  kasu-server /srv/kasu demo
```

`token_file` is a path relative to the JSON's location, or an absolute path.
`KASU_TOKEN` / `KASU_TOKEN_FILE` remain the operator credential.
The server can also run with accounts only, omitting the operator PAT. When accounts are configured, anonymous access is rejected.
PATs are passed as the Basic password or as Bearer. The Basic username is not matched against the account name.

Omitting `write_policy` or setting it to `null` makes the account read-only.
An explicit `{}` is the default WritePolicy, i.e. creating / fast-forwarding branches / tags and changing every path are allowed.
Force and deletion are not allowed by default. To limit the write scope, spell out the policy as above.
`allowed_path_prefixes` accepts both UTF-8 strings and byte arrays; for example, `[255]` is the path `0xff`.
An empty string means every path, and an empty array means no paths may be changed.
Matching is at path boundaries, so allowing `deploy` does not allow changing `deployment`.

## Per-repository Grants

When serving multiple repositories, set `grants` on the account.
For each repository name glob in `repositories`, specify `write_policy` and `read_directories`;
only the single most specific match is used. Repositories without a grant return `NotFound`, the same as nonexistent ones.
The form above without `grants` is treated as a single grant on the legacy repository registered by the startup arguments or `migrate-registry`.
See [multiple repositories](multi-repository.md#authentication-and-authorization) for matching rules and examples.

## Relation to the Server-wide Policy

Effective permissions are the range allowed by all of the [server-wide WritePolicy](git-push.md), the [repository policy](multi-repository.md#per-repository-configuration), and the account policy.
Account settings never widen server-wide restrictions.

| Operation | Decision |
|---|---|
| Ref update target | Both policies allow that ref |
| New ref | Both policies allow creation |
| Ref deletion | Both policies allow deletion, and the final tree diff is within the allowed scope |
| Force / tag replacement | Both policies allow force for that ref |
| Changed paths | Each change in the final tree is within both policies' path scope |
| Git atomic push | A batched CAS only if every ref passes |
| Read-only PAT | Metadata / Git fetch allowed; Objects also follows the existing hop authorization. All Writer RPCs and push are rejected |

If the server-wide force setting is empty, force is not possible even if the account has a force setting.
Path checks for new refs compare against an empty tree, so publishing a new branch containing existing files outside the allowed scope is also rejected.
Changes to an existing branch compare that branch's tree before and after the update.
This allows updating only allowed directories while keeping existing out-of-scope files.

Native UploadBlobs / CreateChangeset / Merge / Rebase can create drafts for writable accounts.
Path ACLs check the final tree diff when UpdateRef and Land publish, and Git when receive-pack publishes.
Decisions are not based only on individual Changesets' change lists, so out-of-scope changes through another merge parent are also checked.
Land checks the post-rebase candidate on each attempt and right before committing.
Accounts without a read scope can read the whole repository, so this is not a feature for hiding draft creation per path.

## Land Resends and Ref Logs

Durable idempotent Land results are identified by combining the configured account name with the client's idempotency key.
If different accounts use the same key, each is processed as its own request.
Changing the request contents with the same account and key is an error.
The existing key / fingerprint format for the operator PAT is preserved.

When updating PATs or reordering the configuration, keeping the account name allows refetching the same result.
Changing the account name gives a different resend namespace.
Configuration and secret files are read at process startup. Changes and revocations require deploying the configuration to every server / SSH worker.
SSH workers start per connection, so updates are read from the next connection.
Requests already started keep the policy from their start time.

The configured account name is used as the actor in the ref log of finalized native / Git updates.
The operator's existing `api` / `git-push` actors are preserved.
Durable audit logs of rejected requests, OIDC authentication, and dynamic revocation are not implemented.

## Mapping over SSH

Make the last argument of the forced command in [Git SSH](git-ssh.md) `authorized_keys` the target account's PAT file.
Also include `KASU_ACCOUNTS_FILE=/etc/kasu/accounts.json` in the administrator-configured sshd `SetEnv`.
There is no need to accept account configuration or policy environment variables from clients.

```text
restrict,command="/opt/kasu/kasu-git-ssh /srv/kasu demo /etc/kasu/secrets/deploy-bot.pat" ssh-ed25519 AAAA... deploy-bot
```

## Administration, Profiles and Delegates

Grants may carry `"admin": true` to let the account administer the repositories they match (settings, state, rename, deletion), and accounts may list `create_repositories` (name globs) they may create, rename to or fork to. See [multiple repositories](multi-repository.md#admin-api).
`display_name` and `email` are optional profile fields (at most 256 bytes, no `<`, `>` or newlines) used as the default signature of commits made in the [Web UI](web-ui.md).

The same file declares **delegates**: principals such as kasu-web that cannot read anything themselves but act as an account per request.

```json
"delegates": [
  { "name": "kasu-web", "token_file": "secrets/kasu-web.pat", "act_as": ["*"], "allow_admin": true,
    "allow_unbound": false, "anonymous_account": "public-reader", "trusted_peers": ["10.0.20.0/24"] }
]
```

| Field | Meaning |
|---|---|
| `act_as` | Account name globs the delegate may act as. The operator and projection accounts are never targets |
| `allow_admin` | Whether delegated requests may change repositories through the Admin API |
| `allow_unbound` | Whether requests without `kasu-act-binding` (reverse-proxy authentication) are accepted |
| `anonymous_account` | The account anonymous visitors act as. It need not match `act_as` and is always read-only |
| `trusted_peers` | Source networks (CIDR) the delegate may connect from; empty allows any address and logs a warning |

A delegated request carries `authorization: Bearer <delegate token>`, `kasu-act-as: <account>`, and optionally `kasu-act-binding` (hex of the binding `Identity.WhoAmI` returned when the user signed in with their PAT), `kasu-act-user` (an audit name, at most 256 bytes) and `kasu-client-addr` (the browser's address). The server rejects with `UNAUTHENTICATED` a missing or unknown account, one outside `act_as`, an untrusted peer, and a binding that does not match the account's current PAT; a delegate token without `kasu-act-as` reads nothing. Otherwise the request is authorized, attributed (ref log actor, Land keys, review authorship) and admitted exactly as a direct request by that account, except that the anonymous account is fixed read-only. The ref log records the delegate and user in the `via` column. Bindings are keyed hashes of the PAT under `KASU_BINDING_KEY_FILE` (32 bytes or more; shared by every replica); without it each process uses a random key. Delegate tokens must differ from every other credential.

## Configuration Limits

- 1–256 accounts. Names are 1–128 bytes and must be unique within the configuration and across projection accounts.
- PATs must not duplicate the operator, other accounts, projection accounts, or the Objects hop token.
- JSON is at most 1 MiB. Unknown fields, missing secret files, and invalid byte paths fail startup.
- Each rule array in each policy has at most 128 entries, each rule at most 4096 bytes.
- PAT read limits and trailing newline handling are the same as existing token files.

## Verification

Verified with real gRPC / HTTP JSON on macOS arm64, real HTTP and SSH stdio with Git 2.54.0,
and real OpenSSH / Git 2.39.5 on Linux arm64 with pinned Rust 1.90.
Tests covered per-account paths / refs, the intersection of both policies, read-only, atomic rollback, non-UTF-8,
64 concurrent HTTP requests, native Land final diffs and resends, differing account order across 2 real servers,
and resends / ref logs after PAT updates and restarts.
The tests verify the authorization feature's behavior; a security audit is outside the scope of this work.

```sh
cargo test -p kasu-server --lib accounts --locked
cargo test -p kasu-server --test accounts --test git_ssh --locked
docker build -f tools/ssh/Dockerfile -t kasu:ssh-test .
docker run --rm --network none --entrypoint /out/accounts-test kasu:ssh-test
docker run --rm --network none kasu:ssh-test
```

The [verification results and hashes](../crates/kasu-git/tests/fixtures/clients/accounts/) and
[SSH traffic records](../crates/kasu-git/tests/fixtures/clients/ssh/) are stored.
The same tests have also been added to the [Git SSH CI](../.github/workflows/ssh.yml). Results of runs on GitHub are unconfirmed.
