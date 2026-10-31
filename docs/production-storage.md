# PostgreSQL / S3 and Multiple Servers

English | [日本語](production-storage_ja.md)

The `production` feature of `kasu-store` enables the PostgreSQL MetaStore / RefStore / GitRefStore /
WebhookStore and the S3 BlobStore.
It is available through the same feature on `kasu-server` / `kasu-ingest`.

```sh
cargo build -p kasu-server -p kasu-ingest --features production --bins --locked

export KASU_POSTGRES_DSN_FILE=/run/secrets/kasu-postgres
export KASU_S3_BUCKET=my-kasu-bucket
export KASU_S3_REGION=ap-northeast-1
export KASU_S3_PREFIX=kasu/objects/v1
export KASU_TOKEN=your-user-token

target/debug/kasu-ingest /path/to/repository.git /path/to/local-fallback my-repo
target/debug/kasu-server /path/to/local-fallback my-repo 127.0.0.1:50051 127.0.0.1:50080
```

The DSN file contains PostgreSQL connection settings such as
`host=db.example port=5432 dbname=kasu user=kasu sslmode=require`. If a password is needed, include it in the same file and protect it.
It is at most 64 KiB, UTF-8, with trailing CR / LF removed. Empty connection settings are rejected.
TLS is verified with WebPKI roots and the host name, and a private CA can be added as a PEM file with
`KASU_POSTGRES_CA_FILE`.
TLS `prefer` is upgraded to `require`, and it never falls back automatically to an unencrypted connection.

S3 credentials follow [object_store's AWS configuration](https://docs.rs/object_store/0.14.2/object_store/aws/struct.AmazonS3Builder.html#method.from_env).
Access key / secret / session token from environment variables, web identity, and ECS / EC2 credential providers are available.
Key values are never stored in CLI arguments or the working copy.
Create the bucket beforehand. Grant the server / ingest processes using the same bucket / prefix
the required GetObject / PutObject permissions. The store never deletes objects.
Bucket policies that retain existing objects and credential management are the operator's responsibility.

| Environment variable | Behavior |
|---|---|
| `KASU_POSTGRES_DSN_FILE` | PostgreSQL when set; local SQLite when unset |
| `KASU_POSTGRES_CA_FILE` | Additional CA certificates for PostgreSQL |
| `KASU_S3_BUCKET` | S3 when set; local FS when unset |
| `KASU_S3_PREFIX` | Prefix for compressed objects. Default `kasu/objects/v1` |
| `KASU_S3_REGION` | Explicit AWS region. Uses the AWS configuration when omitted |
| `KASU_S3_ENDPOINT` | URL of an S3-compatible service. Defaults to the AWS endpoint |
| `KASU_S3_PATH_STYLE` | `true` / `1` for path style. Default false |
| `KASU_POSTGRES_ALLOW_PLAINTEXT` | `true` / `1` combined with `sslmode=disable` in the DSN explicitly allows plaintext |
| `KASU_S3_ALLOW_PLAINTEXT` | `true` / `1` explicitly allows an HTTP endpoint |

When an external store is specified, configuration and connection errors fail startup or the request.
It never switches to a local store and writes separate state.
Binaries without the feature also refuse to start rather than ignoring specified PostgreSQL / S3 settings.
When both external stores are specified, the local directory in the positional argument is neither created nor used.
Multiple servers must always share the same PostgreSQL and S3 bucket / prefix.
Externalizing only one of them is possible, but then sharing the other's local data is the operator's responsibility.

## PostgreSQL Storage Contract

At startup, a dedicated `kasu` schema is initialized or upgraded from version 1 to version 2 under an advisory lock. Connections to version 2 only read the version; reconnecting does not run DDL or lock data tables. Stop older server/ingest processes before this upgrade, since they do not accept version 2.
Schemas are never dropped, and there is no automatic migration from existing SQLite. Unsupported schema versions are rejected.
Prepare a dedicated DB role with the permissions needed for initialization and normal storage.

The immutable KV keeps a BLAKE3 key index plus the original key and value.
After `INSERT ... ON CONFLICT DO NOTHING`, equality is checked and overwrites with a different value are rejected.
Long paths and non-UTF-8 keys can be stored without depending on PostgreSQL's B-tree row size limit.
Repo and ref indexes also check fixed-length hashes against the original bytes.

Mutable refs lock the row per repo / type / name and then compare both target and version.
The lock order for multiple refs is fixed, and refs and logs are updated in the same transaction only when every ref matches.
Deletion is a tombstone that keeps the version. Mixed transactions of branches and Git-only refs and
[Git atomic push](git-push.md) are supported.

Log numbers do not use `SERIAL` / `nextval`.
[PostgreSQL sequences](https://www.postgresql.org/docs/18/functions-sequence.html) advance independently of transactions,
so if issue order and commit order invert, cursor delivery could miss logs from a slow transaction.
A per-repo / per-stream row counter is incremented in the same transaction as the log append,
preventing a later number from committing first. This short section is serialized for the same repo / stream.
Unrelated repos do not share counters.

Land shares the repo / idempotency key claim, the result fingerprint, and the response bytes.
The claim, ref CAS, logs, and result storage are in the same transaction and roll back on cancellation.
Requests for the same key from another connection return the original result, and a different fingerprint is rejected.
Webhooks also keep per-endpoint / per-stream cursors, finalized bodies, attempt counts, and next-attempt times in PostgreSQL.
Bodies do not change even if another server resends, and stale workers cannot rewind cursors.
External HTTP delivery is at-least-once.

Each `PostgresStore` reuses one synchronous connection and serializes within the instance.
Transactions are never widened across the whole immutable KV to run SQL.
Defaults are a 5-second connection deadline, 10-second SQL statement deadline, 5-second lock wait deadline, 30-second idle transaction deadline,
and `synchronous_commit=on`.
After a connection closes, the next request reconnects. Failed SQL / commits are never automatically retried internally.
A Land with a lost response can be resent with the original key. For normal CAS, check the latest ref.
Connection pooling and capacity evaluation for large-scale operation are future work.

## S3 Storage Contract

Keys are `<prefix>/<first 2 hex digits>/<remaining 62 hex digits>`, and contents are the stored zstd frames.
Objects are created with `If-None-Match: *` using
[S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html).
If an object exists, its decompressed contents are checked, and even if a differently compressed frame of the same raw bytes arrives, the first frame is kept.
There is no fallback that replaces an unsupported conditional write with a normal overwriting PUT.
S3-compatible services also need conditional create and read-after-write consistency.

Reads and writes verify the 256 MiB decompression limit and BLAKE3.
Compressed bytes are at most 257 MiB, and received streams are also checked against the limit and Content-Length.
Corrupt existing objects are errors and are never repaired or overwritten by a normal put.
Object frames are not recompressed on transfer or retrieval.
S3 GET / PUT and PostgreSQL storage are not a distributed transaction;
immutable objects are stored first and refs are published afterwards. Unreferenced objects from failed requests may remain.

S3 operations have a 15-second overall deadline by default, a 5-second connection timeout, and at most 2 internal retries.
Retries are included in the 15-second overall deadline.
SQL parameters, connection DSNs, signed URLs, and credentials from provider errors are never exposed in responses.
Manage data deletion, retention generations, and backup and restore with separate operational procedures.

## Verification Against Real Services

```sh
python3 tools/storage/run.py --workdir /tmp/kasu-storage-host
python3 tools/storage/run.py --linux --workdir /tmp/kasu-storage-linux
```

Pinned PostgreSQL 18 / SeaweedFS 4.45 are started with Docker Compose on dedicated localhost ports.
To change the default ports 55432 / 58333, set `KASU_TEST_PG_PORT` / `KASU_TEST_S3_PORT`.
Test credentials use only the public test values in `tools/storage/s3.json`; no cloud account is needed.
A short-lived test CA is generated, and rejection of untrusted CAs, TLS with an explicit CA, and rejection of anonymous S3 are also confirmed.
At exit, this run's Compose project and volumes are deleted. `--keep` keeps them for investigation.
Logs, the public CA certificate, and `summary.json` on success are saved in the working directory.

- A single winner for mixed CAS over 16 connections, agreement between refs and both kinds of logs, and rejection of ABA / stale.
- Rollback of refs / logs / counters on SQL failures, and later logs never overtaking uncommitted smaller numbers.
- The same Land key over 8 connections, reconnection and cancellation, webhook body races and stale workers.
- 16 concurrent S3 writers, immutable storage of differently compressed frames, reopening, and rejection of corruption.
- Git atomic push / strict fsck with the real ingest binary and two server processes,
  immediate sharing of Metadata / Objects, a single winner for native CAS, and Land replay on another server and after restart.
- Fetch failures with wrong S3 credentials, without exposing credentials and without local fallback.

This verification targets a local S3-compatible API. It does not cover real AWS IAM / KMS / network failures,
or capacity, latency, and disaster recovery for geographically distributed deployments.
The [macOS / Linux run records](simulations/README.md#shared-storage-acceptance-test) from 2026-10-23 are stored.
The [initial TiKV evaluation](benchmarks/kv-evaluation.md) covered multi-region transactions, replica / PD failures,
and full checks after a complete restart. The default shared storage remains PostgreSQL / S3.
A production Rust adapter for TiKV and evaluation of equivalent HA, large-scale load, and disaster recovery are future work.
