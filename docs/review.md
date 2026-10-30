# Reviews

English | [日本語](review_ja.md)

A review proposes landing a stack of Changesets on a ref (the *target*). Its data and the approval decision live in kasu-server's `kasu.v1.Review` service, so the [Web UI](web-ui.md), CLIs and bots use the same API. A review only references immutable Changesets; nothing is copied.

| Term | Meaning |
|---|---|
| Stack | `c1 ← c2 ← … ← cn`, each Changeset's first parent being the previous one (1 to 64) |
| Base | `c1`'s first parent; the cumulative change is `diff(base, cn)` |
| Revision | A version of the stack. Updates (amend, rebase) append revisions; old ones stay |
| Vote | An account's latest `APPROVE`, `REQUEST_CHANGES` or `NONE`, on a revision |
| Submit | Landing the approved stack on the target |

Reviews are numbered per repository and never reuse a number. A review is `OPEN`, `CLOSED` (the author or an administrator can reopen it) or `SUBMITTED` (final; comments stay possible). Only one open review may have a given target and head (`ALREADY_EXISTS` names the existing number). While a repository is ReadOnly or Archived every review write is `FAILED_PRECONDITION`.

## Policy

The repository configuration (`config_json`, or the repositories file) takes a `review` object. Without it nothing is protected and existing behavior is unchanged.

```json
{
  "review": {
    "rules": [
      {
        "ref_patterns": ["refs/heads/main", "refs/heads/release/*"],
        "required_approvals": 1,
        "approvers": ["team-a-*"],
        "allow_self_approval": false,
        "block_on_request_changes": true,
        "keep_approvals_on_trivial_rebase": true,
        "require_resolved_threads": false,
        "methods": ["REBASE", "SQUASH"],
        "trailer": true
      }
    ],
    "bypass_accounts": ["image-automation-bot"]
  }
}
```

| Field | Default | Meaning |
|---|---|---|
| `ref_patterns` | required | Protected refs (glob, as in write policies) |
| `required_approvals` | 1 | 0–10; 0 still requires going through a review |
| `approvers` | every account that can write the target | Account name globs whose approvals count |
| `allow_self_approval` | false | Count approvals by the review author and revision creators |
| `block_on_request_changes` | true | Any eligible `REQUEST_CHANGES` blocks Submit |
| `keep_approvals_on_trivial_rebase` | true | Keep approvals while the stack's fingerprint is unchanged |
| `require_resolved_threads` | false | Unresolved published threads block Submit |
| `methods` | both | Allowed Submit methods |
| `trailer` | true | Append `Review: <repo>#<number>` to landed messages |
| `owners` | `[]` | Path owners: `{ "paths": ["deploy/", "charts/*.yaml"], "approvers": ["team-ops-*"], "required_approvals": 1 }`. When the stack changes a matching path (a pattern ending in `/` covers everything below it), that many valid approvals from matching accounts are also required |
| `bypass_accounts` | `[]` | Accounts that may update protected refs directly (the operator always may) |

The first matching rule applies, and the policy in force at Submit time decides. Accounts removed from the account configuration no longer count.

### Enforcement

Protected refs change only through Submit or a bypass account:

| Path | Result for others |
|---|---|
| `Writer.Land`, `/v1/land`, `/v1/guarded-land` | `DENIED`, message `review required` |
| `Writer.UpdateRef`, `/v1/update-ref` | `PERMISSION_DENIED` |
| Git receive-pack (HTTP and SSH) | `ng <ref> review required` (an atomic push rejects all) |

This includes creating and deleting protected refs. Bypass updates are recorded in the ref log with `via` containing `bypass`. `Identity.GetPermissions` reports `review_required` and `review_bypass` for each ref in advance.

## Approval

`GetSubmitStatus` and `Submit` use the same evaluation and return the unmet conditions:

1. The review is open.
2. An approval counts when its account is eligible (matches `approvers`, or can write the target, and has unrestricted read access), it is not a self-approval (unless allowed), and it was given on the latest revision — or on an earlier one whose **fingerprint** equals every later one when `keep_approvals_on_trivial_rebase` is set.
3. At least `required_approvals` approvals count.
4. No eligible `REQUEST_CHANGES` remains (when blocking).
5. No unresolved published thread remains (when required).
6. The caller can write the target.

The fingerprint hashes, for each Changeset of the stack in order, its changes against its first parent as (path → resulting entry). A rebase that changes no Changeset's own changes keeps it ("trivial rebase"); differences that only come from the new base are not detected, as in Gerrit.

## Submit

`Submit(number, expected_revision, method, idempotency_key)`:

- The key is bound to the account, review, revision and method; retries and replays return the stored result (`replayed`).
- A newer revision or an unmet condition returns `NOT_READY` with the status.
- `REBASE` replays each Changeset onto the target's tip (authors kept, the submitter as committer). When the tip is already the base and trailers are off, the original Changesets land unchanged. `SQUASH` lands one Changeset with the cumulative change, the review title and description as message and the review author as author.
- A conflict returns `CONFLICT` with the Changeset index and paths; the review stays open. Use `RebaseReview` (the Web UI's *Rebase onto target*) or resolve with the CLI and add a revision.
- The ref compare-and-swap, the ref log entry, `SUBMITTED`, and the idempotent result commit in **one transaction**. If the target or the review moved, conditions are evaluated again and the attempt repeats within the budget (`max_attempts`, `timeout_ms`, as for Land); otherwise `RETRY`.

`RebaseReview(number, expected_revision, onto?)` rebases the latest stack onto the target's tip (or `onto`) and appends a revision, or returns the conflict without changing the review.

## Comments

Comments belong to a revision, optionally anchored to a line: `Anchor { path, side: HEAD | BASE, line, changeset_index? }` (without `changeset_index` the line is on the cumulative diff). Replies point at a thread root; only roots are resolved. Drafts (`SaveDraft`) are visible only to their author until `Publish`, which publishes all of the caller's drafts, an optional top-level comment and an optional vote atomically. Authors edit their comments; deletion leaves a tombstone. Limits per review: 256 revisions, 10,000 comments, 64 Changesets per stack; titles up to 256 bytes, descriptions and comments up to 64 KiB.

## Permissions

| Operation | Requires |
|---|---|
| Read reviews and comments | Read access without `read_directories` (directory-limited accounts get `NOT_FOUND`) |
| Comment, vote | The above and a non-anonymous account (votes outside `approvers` are kept but do not count) |
| Create, add revision, rebase | Write access to the target, ignoring the review requirement |
| Edit title and description, close, reopen | The author or a repository administrator |
| Submit | Approval condition 6 |

Reviews of invisible repositories are `NOT_FOUND`, indistinguishable from missing ones.

## API

gRPC service `kasu.v1.Review` and HTTP/JSON (`POST`, same messages):

| RPC | HTTP |
|---|---|
| `CreateReview` (idempotency key) | `/v1/review/create` |
| `GetReview` | `/v1/review/get` |
| `ListReviews` (state, target, author; newest first; 50 by default, up to 500; cursor) | `/v1/review/list` |
| `UpdateReview` (CAS on `version`) | `/v1/review/update` |
| `AddRevision` (CAS on `expected_revision`) | `/v1/review/add-revision` |
| `RebaseReview` | `/v1/review/rebase` |
| `ListComments` / `SaveDraft` / `EditComment` / `ResolveThread` / `Publish` | `/v1/review/list-comments`, `save-draft`, `edit-comment`, `resolve-thread`, `publish` |
| `GetSubmitStatus` / `Submit` | `/v1/review/submit-status`, `/v1/review/submit` |

## CLI and Webhooks

`kasu review list|show|create|vote|comment|update|rebase|submit` uses the same API ([native CLI](native-cli.md)). Review events are delivered to the repository's webhook endpoints with `X-GitHub-Event: kasu_review` ([webhooks](webhooks.md#review-events)).

## Storage

`kasu-store` implements `ReviewStore` on SQLite and PostgreSQL in the same database as refs (`review_counters`, `reviews`, `review_revisions`, `review_votes`, `review_comments`, `review_submissions`, `review_log`), partitioned by repository key. Every review change is one transaction with a version compare-and-swap; Submit adds the ref update to it. `review_log` records creation, revisions, publications, resolutions, updates and submissions for future webhooks and notifications. Renames keep reviews (the key is unchanged), forks do not copy them, and deleting a repository purges them with its refs.
