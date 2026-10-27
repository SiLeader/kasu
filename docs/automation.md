# Metadata API Test Gate, Bisect, and Revert Bot

English | [日本語](automation_ja.md)

`tools/automation/kasu_bot.py` is a reference implementation, using only the Python standard library, for post-land and pre-deploy tests.
It uses Python 3.11 or later and POSIX process groups / file locks. It does not use Git or a working copy.
It connects to the server's HTTP JSON port, not gRPC.

```sh
python3 tools/automation/kasu_bot.py \
  --endpoint http://127.0.0.1:50080 --repo my-repo \
  --token-file /run/secrets/kasu.pat --journal /var/lib/kasu-bot/check.json \
  check --revision "$CHANGESET_ID" --ref refs/heads/main --test-id "$TEST_IMAGE_DIGEST" \
  -- python3 tools/automation/check_file.py --path deploy/app.yaml --contains 'healthy: true'
```

IDs are 64-character native Changeset IDs, not Git SHAs. Integrations that receive Git SHAs
first convert them with Metadata's `FindChangesetByGitOid`. Integrations that use refs get an immutable ID with `ResolveRef`.
The PAT is read from a secret file. Projection-only PATs cannot use the native API, so they cannot be used with this bot.
`check` / `bisect` themselves and the bundled checker call only Metadata reads; `revert` also uses the Writer API.

## Tester Contract

What follows `--` is a tester prepared by CI, launched as argv without a shell.
It receives the environment variables `KASU_METADATA_ENDPOINT`, `KASU_REPO`, `KASU_CHANGESET`, and, when specified, `KASU_TOKEN_FILE`.
The tester evaluates the snapshot of the given ID and returns one of the following exit codes.

| Tester exit code | Meaning |
|---|---|
| 0 | good / test passed |
| 1 | bad / test of the target code or configuration failed |
| 125 | The revision cannot be judged, so skip |
| Other | Tester / authentication / network or other failure. Not recorded as bad; processing stops |

stdout / stderr are not mixed into the bot's output. Testers that need detailed CI logs specify their own log destination.
On exit, timeout, or cancellation, the dedicated process group is stopped, including children the tester forked.
The default tester limit is 30 seconds and the whole-operation budget is 300 seconds. Change them with `--test-timeout` and with `--timeout` before the subcommand.
No bad verdict is stored for a revision that timed out.

The bundled `check_file.py` is a small example that checks whether ReadFile content contains given bytes.
`--path-hex` / `--contains-hex` allow non-UTF-8 paths and binary values.
A missing file or missing expected value returns 1; authentication, network, or argument errors return 2.
In real CI, replace it with a tester suited to the application or manifests.

## Check and Deploy Gate

`check` verifies the given ID and returns `passed` / `failed` / `skipped`.
With `--ref`, the target and version are compared before and after the test; if the tip differs or an ABA happens midway, the result is `stale`.
Deployment is pinned to the result's `id`. The test result is never applied to a newer tip that advanced after the ref comparison.

For post-land tests, the Land result ID can be passed directly to `check --revision`.
To verify the landed snapshot itself even if the tip has already moved on, omit `--ref`.
For a pre-deploy current-head gate, use `--ref` as well and deploy the artifact corresponding to the passing ID.

## DAG Bisect

```sh
python3 tools/automation/kasu_bot.py \
  --endpoint http://127.0.0.1:50080 --repo my-repo \
  --token-file /run/secrets/kasu.pat --journal /var/lib/kasu-bot/bisect.json \
  bisect --good "$LAST_GOOD_ID" --bad "$FAILED_ID" --test-id "$TEST_IMAGE_DIGEST" \
  -- python3 tools/automation/check_file.py --path deploy/app.yaml --contains 'healthy: true'
```

GetChangeset reads the DAG along all parents of bad, and ancestors of good are removed from the candidates. It is not limited to first parents.
Multiple goods can be given, and all must be strict ancestors of bad. The boundaries are also verified with the tester.
It picks the revision that splits the candidates as evenly as possible; on good it removes that revision's ancestors, and on bad it restricts candidates to its ancestors.
It uses the usual bisect assumptions: a single cause, the same test definition, and a transition from good to bad in the descendant direction.

When skips prevent a unique answer, it returns `ambiguous` with the candidate set and does not assert a cause.
The default is at most 10,000 Changesets and 64 new tester runs in addition to boundary verification.
`--max-nodes` can be set up to 100,000 and `--max-tests` up to 10,000.
Search, RPCs, and the tester share the overall budget, and running out of budget gives a resumable `retry`.
It is a reference algorithm that compares ancestor sets per candidate, so search performance on large DAGs is unevaluated.

## 3-way Revert

```sh
python3 tools/automation/kasu_bot.py \
  --endpoint http://127.0.0.1:50080 --repo my-repo \
  --token-file /run/secrets/kasu.pat --journal /var/lib/kasu-bot/revert.json \
  revert --bad "$CULPRIT_ID" --branch refs/heads/main --operation-id "$CI_REVERT_ID"
```

This command actually updates the ref. Launch it explicitly from the CI job that performs reverts.
Reverting a merge requires a 1-based parent number such as `--mainline 1`. Root commits can also be reverted.

1. Get the reverse diff from bad toward the chosen parent with Metadata's Diff.
   Fill in the membership index for old blob IDs with GetRootTree, and reuse them without downloading contents.
2. Create an inverse Changeset with bad as parent via CreateChangeset. For a root, each original entry is deleted.
3. Rebase onto the current branch tip. Unrelated later edits to the same file are kept; overlapping edits are returned as conflicts.
4. Land with `/v1/guarded-land`. Each attempt checks that bad is an ancestor of the current tip and
   CASes that tip's target / version, so a force update, deletion, or ABA midway cannot slip past the condition.
5. The existing ref / final-tree ACLs, bounded retries, and Land idempotent results are used as is.

Older servers without the guarded endpoint return an error, and there is no fallback to a plain Land.
Rebase / Land conflicts stop with exit code 2 without updating the ref.
A single Land request is 8 attempts / 5 seconds. On `retry`, rerun with the same journal and arguments.
If bad has left the branch, the result is `denied`, and the reverse diff is not applied to a different history.

## Journal and Resumption

Every operation requires `--journal`. The file is 0600 with atomic replace / fsync, and concurrent use is rejected with an OS file lock.
It records the binding of repo, endpoint, operation, IDs, and test definition or revert settings.
Reusing the same journal for a different operation or tester is rejected. Only the budget can be increased when resuming.
For `--test-id`, use a reproducible value identifying the tester's image digest and configuration.
There is no mechanism that automatically treats a replaced tester, changed environment, or flaky result as the same.

revert stores metadata, the inverse ID, the rebase target, the Land ID, and the key before the next API call.
On a lost response after sending Land, it resends the same ID / key without creating a new draft.
Make `--operation-id` unique within the repository. Using the same ID for a different operation is rejected because it does not match the stored Land fingerprint.
The journal is required for resumption. A finalized receipt is a historical result and does not represent the current tip at refetch time.
To redo a terminal conflict against a different tip, use a new operation ID and journal.

| Bot status | Exit code |
|---|---|
| `passed` / `culprit` / `landed` | 0 |
| `failed` / `error` for configuration, authentication, or tester failures | 1 |
| `skipped` / `ambiguous` / `conflict` / `denied` | 2 |
| `retry` / `stale` | 3 |

Results are returned as JSON on stdout. For bisect, 0 means "cause identified"; for a deploy gate's success, use `check`'s `passed`.
Metadata responses / journals are limited to 16 MiB and authentication files to 4096 bytes. HTTP redirects are not followed.

## Verification

Verified with Python 3.14.8 on macOS arm64 and Python 3.11 on Linux arm64 with pinned Rust 1.90.

- Searching all 12 candidates in 12 kinds of branching / merge DAGs, 144 causes in total.
- Candidate sets with skips, resuming after running out of budget, verdict caching, tester timeouts / abnormal exits and stopping the process group.
- Identifying a regression in a second parent via the real Metadata API. check / bisect use only Metadata read endpoints.
- Reverts that keep later edits, conflicts, merge mainline, root reverts, and reruns with the same journal via the real Writer API.
- A lost response after Land is stored, and replay of the receipt after the branch is subsequently replaced, without the ref log growing twice.
- Rechecking the ancestor condition after losing the CAS, rejecting key reuse with a different condition, and rejecting redirects / missing conditions.
- check pass / fail, a tip mismatch before the test, and ABA during the test. Gate decisions pinned to native IDs.

The [verification record](simulations/automation/summary.json) stores source hashes and logs.
Python unit tests and the existing all-feature Rust tests have been added to GitHub CI, but new results on GitHub are unconfirmed.
Automatic connection to CI jobs or real deployment targets, OIDC, distributed journal storage, and capacity evaluation on large DAGs are future work.

```sh
python3 -m unittest discover -s tools/automation -v
cargo test -p kasu-server --test automation --locked
docker build -f tools/automation/Dockerfile -t kasu:automation-test .
```
