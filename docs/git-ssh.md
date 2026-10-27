# Git over SSH

English | [日本語](git-ssh_ja.md)

`kasu-git-ssh` is a Git transport launched as an OpenSSH forced command.
Public-key authentication and encryption are delegated to OpenSSH, while Kasu handles Git advertisement / negotiation / packs
and reads and writes to native storage. No external Git process or bare repository is used.

## Startup and Credentials

```sh
cargo build -p kasu-server --bin kasu-git-ssh --release --locked
```

Register a fixed command per public key in the `authorized_keys` of a dedicated OS user `git`.
Replace the paths below with your installation paths. Never build arguments from client input.

```text
restrict,command="/opt/kasu/kasu-git-ssh /srv/kasu demo /etc/kasu/operator.pat" ssh-ed25519 AAAA... operator
restrict,command="/opt/kasu/kasu-git-ssh /srv/kasu demo /etc/kasu/deploy-reader.pat" ssh-ed25519 AAAA... deploy-reader
```

Omitting the repository, as in `kasu-git-ssh /srv/kasu /etc/kasu/operator.pat`, resolves
the repository from the client's command (`git-upload-pack 'team-a/infra.git'` for `git clone git@host:team-a/infra.git`)
and authorizes it with the credential's [grants](multi-repository.md#authentication-and-authorization).
The form that names a repository serves only that repository.

In `sshd_config`, pass only administrator-prepared settings to that user.

```text
Match User git
    AuthenticationMethods publickey
    PasswordAuthentication no
    KbdInteractiveAuthentication no
    PermitTTY no
    DisableForwarding yes
    AcceptEnv GIT_PROTOCOL
    SetEnv KASU_TOKEN_FILE=/etc/kasu/operator.pat KASU_PROJECTION_ACCOUNTS_FILE=/etc/kasu/accounts.json KASU_WRITE_POLICY_FILE=/etc/kasu/write-policy.json
```

`KASU_TOKEN_FILE` is the file version of the existing `KASU_TOKEN` and also works with the regular `kasu-server`.
Specifying both is a startup error. Store PATs in secret files, and match the [projection account](projection-accounts.md)
`token_file` with the last argument of the forced command. The last argument is the connecting user's credential;
the file in the environment variable is the operator credential the backend checks against.
Do not configure an `AcceptEnv` that accepts `KASU_*` from clients.

```sh
git clone ssh://git@example.test/demo.git
git clone git@example.test:demo.git
git clone --depth=1 ssh://git@example.test/demo/-/path/deploy.git
git clone --filter=blob:none ssh://git@example.test/demo.git
git push origin main
```

SSH host keys are checked against the client's `known_hosts`. Normal repos are authorized by the operator PAT or a [repository account](repository-accounts.md),
and projections by the PAT for the allowed directory. Anonymous SSH is rejected.
Only `git-upload-pack` / `git-receive-pack` with a single repository argument is interpreted, and
`SSH_ORIGINAL_COMMAND` is never run through a shell. Apostrophes and non-UTF-8 bytes in paths are preserved,
and the projection `path-hex` notation is interpreted the same as over HTTP.

## Behavior and Constraints

Supports stateful negotiation for protocol v0 / v1 / v2, shallow / deepen / unshallow,
blob fetching for partial clones, normal push, atomic push, and ref deletion.
The end of a pack is detected from zlib boundaries and the object count, so the receiver does not wait for SSH EOF.
Empty packs and deletion-only pushes without a pack are also handled.
Received pack validation, connectivity, ref CAS, write policy, change log, and webhook outbox are shared with HTTP.
Pushes to projection URLs are rejected.

It reads the shared backend configuration and connects to SQLite / filesystem or the existing production storage.
The SSH worker itself does not start a webhook dispatcher. To deliver the durable outbox,
run a regular `kasu-server` on the same storage.

Control pkt-lines are limited to 16 MiB per session in total, and commands to 4096 bytes.
Packs are subject to the existing receive limits (default 128 MiB, 100,000 objects, 64 MiB per object, 512 MiB expanded total,
delta depth 64). Backpressure is propagated through stdout writes.
`KASU_SSH_TIMEOUT_SECS` is the whole-session deadline, 1–3600 seconds, default 300.
A timeout exits with code 124 and other failures with 1; diagnostics go to stderr.

Each connection is a separate worker process. It is not counted against the shared admission budget of the HTTP / gRPC server.
SSH connection and process counts are managed by OpenSSH and OS limits.
Key operations and revocation, real sshd configuration, and shared quotas across nodes are unevaluated.

## Verification

Over stdio with macOS Git 2.54.0, and over real public-key connections with Linux Git 2.39.5 / OpenSSH 9.2p1,
verified full / depth1 clone over protocol 0 / 1 / 2, shallow incremental fetch,
deepen / unshallow, push, empty packs / ref deletion, atomic branch+tag push, partial clone,
projection clones, and account authorization. Each clone's strict fsck and native file contents are also checked.
On Linux, go-git v5.19.3 also passed full / depth1 clone→commit→push over real SSH with pinned known_hosts.

Boundary unit tests verify pkt-line / pack input fed one byte at a time, truncated input, and not consuming subsequent messages.
It was also confirmed that a real process waiting with stdin left open exits at the configured deadline.
The [traffic fixtures and results](../crates/kasu-git/tests/fixtures/clients/ssh/) are stored.

```sh
cargo test -p kasu-server --lib git_ssh --locked
cargo test -p kasu-server --test git_ssh --locked
docker build -f tools/ssh/Dockerfile -t kasu:ssh-test .
docker run --rm --network none kasu:ssh-test
```

Root / empty password in Docker is a setting only inside the test container; sshd accepts public keys only, and the destination is loopback.
The CI [Git SSH workflow](../.github/workflows/ssh.yml) runs the same container.
Results of runs on GitHub are unconfirmed. A security audit of cryptography and key management is outside the scope of this verification.

The specifications referenced are Git's official [pack protocol](https://git-scm.com/docs/gitprotocol-pack) and
[protocol v2](https://git-scm.com/docs/gitprotocol-v2).
EOF-independent receiving, the ordering of shallow-info, and multiple rounds of negotiation follow SSH's stateful connection.
