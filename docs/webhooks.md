# Webhooks for Ref Updates

English | [日本語](webhooks_ja.md)

Setting `KASU_WEBHOOKS_FILE=/path/to/webhooks.json` delivers POSTs in the GitHub push format.
The configuration is read at startup.

```json
{
  "repository_url": "https://kasu.example/demo.git",
  "default_branch": "main",
  "endpoints": [
    {
      "id": "argocd",
      "url": "https://argocd.example/api/webhook",
      "secret_file": "webhook-secret"
    }
  ]
}
```

`secret_file` is a path relative to the configuration file, or an absolute path.
The file's bytes are used as is as the HMAC-SHA256 key. Newlines are part of the key.
Configure the same key on the receiver. If `secret_file` is omitted, deliveries are not signed.
ArgoCD uses `webhook.github.secret`; Flux uses a Receiver with `type: github` and
its `secretRef`. URLs may be HTTP / HTTPS, and redirects are not followed.

With multiple repositories, write endpoints in the [per-repository configuration](multi-repository.md#webhook).
This file then provides delivery settings and `public_url` (or `KASU_PUBLIC_URL`), the base for `clone_url`,
while `endpoints` and `repository_url` belong to the legacy repository.
The payload's `repository.name` / `full_name` / `clone_url` are generated from the current name, so deliveries after a rename reflect it.
Nothing is delivered for `ARCHIVED` repositories.

## Delivery and Resending

The `ref_log` / `git_ref_log` recorded in the same SQLite transaction as the ref CAS
serve as the outbox. A cursor and pending body are persisted per endpoint ID, repository, and log type,
and the body is finalized before sending. An HTTP 2xx advances the cursor; anything else is retried.
Native branch logs and Git tag logs are separate ordered queues.
Symbolic changes to `HEAD` do not send push events; they only advance the cursor.

Deliveries carry `X-GitHub-Event: push`, a stable UUIDv8 `X-GitHub-Delivery`,
and, when signing is configured, `X-Hub-Signature-256: sha256=...`.
The pending body and delivery ID are reused after lost responses or restarts.
Receivers need deduplication. Connecting multiple workers to the same queue never rewinds the cursor,
but concurrent sends can produce duplicates.

A new endpoint ID sends from the beginning of the log. Keeping the ID carries over the delivery position across restarts.
URL / key changes apply to resends after a restart, but bodies already finalized do not change.
A down endpoint does not block other endpoints. Later items in the same queue wait until it succeeds.
Deliveries pending at server shutdown are resent at the next startup.

## Body and Limits

The body includes create / update / force / delete, old / new OIDs, commits, and paths changed relative to the first parent.
The last commit also reflects the final diff from the old ref to the new ref, so path-based refresh decisions work
even for force rewinds and deletions. The commit array has at most 2,048 entries, with the total count and an omission flag included separately.
Non-UTF-8 strings are replaced in standard fields, and the original bytes are stored base64-encoded in the `kasu` extension.

| Setting | Default |
|---|---:|
| `poll_interval_ms` | 100 |
| `timeout_ms` | 5,000 |
| `retry_base_ms` | 100 |
| `retry_max_ms` | 60,000 |
| `max_payload_bytes` | 26,214,400 (25 MiB) |

Retries use exponential backoff with deterministic jitter, waiting from the end of the HTTP attempt.
There are at most 32 endpoints, and keys are 1–65,536 bytes.
Exceeding the body limit does not advance the cursor. Check the logs, raise the configured limit, and restart
(the maximum configurable value is 128 MiB). The current implementation enumerates reachable history when generating events,
so evaluating generation time and memory use on large histories is ongoing work.

```sh
cargo test -p kasu-store --test webhook --locked
cargo test -p kasu-server --test webhook --locked
```

These verify RFC 4231 signatures, reopening and independent connections, identical body / ID after HTTP 500 and timeouts,
redirect rejection, force / delete, and the body limit and termination.
Outbox delivery from native UpdateRef and from a real Git clone→commit→push is also verified.
Verification with real controllers uses `--webhooks` in the [kind E2E](gitops-e2e.md).
