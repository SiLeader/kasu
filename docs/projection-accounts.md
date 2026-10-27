# Read Accounts for Path Projections

English | [日本語](projection-accounts_ja.md)

`KASU_PROJECTION_ACCOUNTS_FILE` configures, per PAT, which directories can be read.
These accounts are only for Git [path projections](git-projection.md).

```json
{
  "accounts": [
    {
      "name": "deploy-reader",
      "token_file": "secrets/deploy-reader.pat",
      "directories": ["deploy", "charts/team-a"],
      "directories_hex": []
    }
  ]
}
```

`token_file` is a path relative to the configuration file, or an absolute path. The PAT is read with trailing CR / LF removed.
Do not put secrets in the JSON itself; store them in files readable only by the owner.
`directories_hex` specifies byte strings for non-UTF-8 paths. For example, `ff` allows the directory `0xff`.

```sh
KASU_PROJECTION_ACCOUNTS_FILE=/path/to/accounts.json \
  cargo run -p kasu-server -- /path/to/kasu-data my-repo

git clone --filter=blob:none \
  http://any-user:reader-pat@127.0.0.1:50080/my-repo/-/path/deploy.git
```

The Basic username is arbitrary. Bearer also works. `name` is an identifier in the configuration and is not matched against the Basic username.
When combined with the existing `KASU_TOKEN`, that PAT becomes the operator credential for the normal repo / Metadata / Writer APIs.
Without it, the server can run as a read server for projection accounts only.
When this configuration is present, anonymous access is rejected even without `KASU_TOKEN`.

With multiple repositories, instead of `directories`, use
`"projections": [{"repository": "team-a/*", "directories": ["deploy"], "directories_hex": []}]` to specify
pairs of repository name globs and directories. The `directories`-only form applies to projections of the legacy repository.
Non-matching repositories return `NotFound`, the same as nonexistent ones.

## Allowed Scope

| Operation | Projection-only PAT |
|---|---|
| Git projection of an allowed directory itself | Readable |
| Projection of a deeper directory inside an allowed directory | Readable |
| v0 / v1 / v2, shallow / partial clone, follow-up promisor fetch | Available within the same scope |
| Parents or siblings of allowed paths, the normal repo URL | Rejected |
| receive-pack advertisement / push | Rejected |
| Metadata / Writer gRPC and HTTP JSON | Rejected |
| Objects gRPC | Rejected. Cannot be reused as the hop credential for a shared cache |

Matching uses path boundaries, so allowing `deploy` does not allow reading `deploy-other`.
Literal and hex URLs are normalized to the same byte-string path before matching.
Allowing multiple directories never grants permission to fetch the whole normal repository.
Want reachability and the OID index are still checked per projection.

Authentication runs before Git URL interpretation, ref derivation, and pack generation; path authorization runs before ref derivation.
Unauthenticated requests and wrong PATs get 401; authenticated but out-of-scope operations get 403.
A projection-only PAT passed to a native API is rejected before any ID membership or content is examined.
When a dedicated Objects hop token is enabled, Objects requires that hop token.

## Configuration and Constraints

- 1–256 accounts; names must be unique, 1–128 UTF-8 bytes.
- Each account has 1–128 directories, each path at most 4096 bytes.
- PATs are at most 4096 bytes. Duplicates across accounts, with the operator PAT, or with the Objects hop token are rejected.
- JSON is at most 1 MiB. Unknown fields, invalid paths / hex, an empty account list, and missing secret files fail startup.
- Changing or revoking configuration / secret files requires a server restart. Distribute the same configuration to multiple servers.

The original author / committer / message and ref names remain in projections. There is no feature to strip strings from messages by path.
[Per-account write ACLs](repository-accounts.md) are available through the separate repository account configuration.
[Native directory reads](directory-read-acl.md) are available through repository accounts' read_directories.
OIDC, dynamic revocation, and denial audit logs remain to be done.
The existing write `allowed_paths` also applies to the operator PAT and checks the final tree diff.

## Verification

The following tests pass on macOS arm64 and Linux arm64 with pinned Rust 1.90.

- Partial clone / follow-up blob fetches with real Git v0 / v1 / v2, an arbitrary Basic username, and a dedicated PAT.
- Accepting allowed descendants and rejecting the normal repo / sibling directories / other hex paths / push.
- Rejection when the same PAT is passed to Metadata / Writer / Objects over HTTP JSON and real gRPC.
- The real `kasu-server` binary reading a relative token file, rejecting anonymous access without an operator PAT, and failing startup on bad configuration.
- Duplicate PATs, collisions with the Objects hop token in either configuration order, unknown fields, invalid paths / hex, and errors that do not contain secret values.

The [verification record](../crates/kasu-git/tests/fixtures/clients/projection-auth-summary.json) stores source hashes and the scope of the run.

```sh
cargo test -p kasu-server --lib --test projection_accounts --locked
cargo test -p kasu-server --test git_http scoped_projection --locked
docker build -f tools/git-compat/Dockerfile -t kasu:projection-auth-test .
```
