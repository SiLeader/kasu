# Land

English | [日本語](land_ja.md)

`kasu.v1.Writer.Land` and HTTP POST `/v1/land` land a Changeset onto a given branch.
Authentication and repository membership rules are the same as the [Write API](write-api.md).

A request has `repo`, `id`, `name`, `idempotency_key`, and optional `max_attempts` / `timeout_ms` / `required_ancestor`.
The key is 1–256 bytes and is bound, within the repository, to the "source Changeset, ref, and API actor", plus `required_ancestor` when given.
Reusing the same key with different contents is InvalidArgument.
The attempt and time budgets are not part of the binding, so after a Retry the same key can be resent with a different budget.

`required_ancestor` is omitted or a 32-byte Changeset ID. When given, each attempt checks that the current tip is a descendant of it.
The checked tip's target / version are used for the CAS of the same Land, so a branch replacement, deletion, or ABA after the check fails the CAS and is rechecked on the next attempt.
If there is no tip or the ancestor condition is not met, the result is `Denied` and the branch is not created.
Replaying an existing receipt takes precedence over checking the condition against the current tip. The fingerprint of existing requests that do not specify the condition is unchanged.

HTTP POST `/v1/guarded-land` is an endpoint that requires a 32-byte ancestor condition.
Against an older server without this endpoint, a conditional operation stops instead of silently falling back to a plain Land;
the [revert bot](automation.md) uses this endpoint. Specifying the condition over gRPC requires a server that implements this field.

## Landing and Results

1. If a stored final result exists, return it with `replayed=true`.
2. Read the current ref and policy, and 3-way rebase the source's diff against its first parent onto the current tip.
   For a new branch, the source is used as is. If the source is already an ancestor of the tip, it is not reapplied.
3. Make the result a member of the repository and prepare the Git commit projection.
   If the content is identical to the tip, the current tip is adopted instead of landing a new empty Changeset.
4. CAS the ref's target and version, and store the final result and ref_log in the same transaction.
5. If the CAS conflicts, recheck the policy and reapply the original source onto the new tip.

| outcome | Meaning |
|---|---|
| `Landed` | Landing result in id and actual. If the change is already included, the ref version / log is not incremented |
| `Conflict` | Structured conflicts in conflict. Stored after checking that the ref is still in the state it was computed against |
| `Retry` | The attempt budget ended due to CAS conflicts, storage failures, cancellation, or timeout. Can be resent with the same key |
| `Denied` | The ref write / create policy does not allow it |

Authentication failures, other repositories, nonexistent sources, and malformed input are ordinary RPC / HTTP errors.
Landed and Conflict are persisted; Retry and Denied are not stored as final results.
To land a different source after resolving a Conflict, specify a new key.
The stored actual is the state at the time of landing. Even if the ref moves on later, a resend returns the original result.

Land reapplies a single Changeset. A merge Changeset is also treated as a diff against its first parent,
and the reapplied result has a single parent: the current ref tip.
It is not an operation that reapplies an entire multi-commit feature branch at once.

## Limits and Cancellation

The default is at most 8 attempts / 5 seconds; the maximum that can be specified is 32 attempts / 30 seconds.
Conflicts and transient storage failures use exponential backoff with jitter based on the request and attempt number.
Waits are at most 200 ms. Cancellation and deadlines are monitored while waiting.

`Backend::land_with_cancellation` accepts an external `LandCancellation`.
When the RPC / HTTP handler future is dropped, cancellation is also propagated to the running worker.
SQLite checks for cancellation inside the transaction, and if a post-store check finds cancellation,
it rolls back the ref, ref_log, and result together.
A timed-out response does not wait for the blocking worker to finish.
Immutable objects being prepared may remain, but publishing the ref always passes the cancellation check.

If cancellation races with a commit, resending with the same key reveals the stored result.
The same procedure works after a lost response or a server restart, preventing double landing.

## Verification

```sh
cargo test -p kasu-store --test land --locked
cargo test -p kasu-server --test write --locked
cargo test -p kasu-sim --test land_runtime --locked
cargo run --release --locked -p kasu-sim -- --seed 42 --clients 1000
```

These check duplicate-key races over 16 store connections, reopening, cancellation mid-transaction,
resends over real gRPC / HTTP, the original result after the ref moves, storage failures, lost responses, timeouts, dropped futures,
and policy changes between prepare and commit.
`kasu-sim` uses the production prepare / commit logic and real SQLite / FS stores, and with virtual time
deterministically reorders every client's requests, CASes, resends, and failures.
It is not a network throughput benchmark.
The [1,000-client run record](simulations/README.md) is stored.
