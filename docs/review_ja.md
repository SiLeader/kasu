# レビュー

[English](review.md) | 日本語

レビューは、Changeset のスタックをある ref（*target*）へ着地させる提案です。データと承認の判定は kasu-server の `kasu.v1.Review` サービスにあり、[Web UI](web-ui_ja.md)・CLI・bot が同じ API を使います。レビューは不変の Changeset を参照するだけで、何も複製しません。

| 用語 | 意味 |
|---|---|
| スタック | `c1 ← c2 ← … ← cn`。各 Changeset の第一親が直前の Changeset（1〜64 個） |
| base | `c1` の第一親。累積の変更は `diff(base, cn)` |
| リビジョン | スタックの版。更新（修正・rebase）のたびに追加し、古い版も残す |
| 投票 | リビジョンに対するアカウントの最新の `APPROVE` / `REQUEST_CHANGES` / `NONE` |
| Submit | 承認されたスタックを target へ着地させること |

レビューの番号はリポジトリごとの連番で、再利用しません。状態は `OPEN`、`CLOSED`（作者または管理者が再オープン可能）、`SUBMITTED`（終端。以後もコメントは可能）です。同じ target・同じ head のオープンなレビューは 1 件だけです（`ALREADY_EXISTS` に既存の番号が含まれます）。リポジトリが ReadOnly / Archived の間はレビューへの書き込みがすべて `FAILED_PRECONDITION` になります。

## ポリシー

リポジトリ設定（`config_json` またはリポジトリファイル）に `review` を置きます。省略時は何も保護せず、既存の挙動は変わりません。

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

| フィールド | 既定 | 意味 |
|---|---|---|
| `ref_patterns` | 必須 | 保護する ref（書き込みポリシーと同じ glob） |
| `required_approvals` | 1 | 0〜10。0 でもレビュー経由は必須 |
| `approvers` | target に書き込めるアカウント全員 | 承認を数えるアカウント名の glob |
| `allow_self_approval` | false | レビュー作者・リビジョン作成者の承認を数えるか |
| `block_on_request_changes` | true | 有資格者の `REQUEST_CHANGES` が 1 件でもあれば Submit 不可 |
| `keep_approvals_on_trivial_rebase` | true | スタックの指紋が変わらない間は承認を維持 |
| `require_resolved_threads` | false | 未解決の公開スレッドがあれば Submit 不可 |
| `methods` | 両方 | 許可する Submit 方式 |
| `trailer` | true | 着地したメッセージに `Review: <repo>#<number>` を付ける |
| `owners` | `[]` | パスの所有者: `{ "paths": ["deploy/", "charts/*.yaml"], "approvers": ["team-ops-*"], "required_approvals": 1 }`。スタックが一致するパス（`/` で終わるパターンは配下すべて）を変更する場合、一致するアカウントからの有効な承認がその数だけ追加で必要 |
| `bypass_accounts` | `[]` | 保護 ref を直接更新できるアカウント（運用者は常に可） |

最初に一致した rule を使い、Submit 時点のポリシーで判定します。アカウント設定から削除されたアカウントの承認は数えません。

### 強制

保護 ref は Submit か bypass アカウントによってのみ変わります。

| 経路 | それ以外の結果 |
|---|---|
| `Writer.Land`、`/v1/land`、`/v1/guarded-land` | `DENIED`、メッセージ `review required` |
| `Writer.UpdateRef`、`/v1/update-ref` | `PERMISSION_DENIED` |
| Git receive-pack（HTTP / SSH） | `ng <ref> review required`（atomic push は全体を拒否） |

保護 ref の作成・削除も含みます。bypass による更新は ref_log の `via` に `bypass` を含めて記録します。`Identity.GetPermissions` は ref ごとの `review_required` と `review_bypass` を事前に返します。

## 承認

`GetSubmitStatus` と `Submit` は同じ判定を使い、満たしていない条件を返します。

1. レビューがオープンである。
2. 承認が有効なのは、アカウントが有資格（`approvers` に一致、または target に書き込め、かつ読み取りが制限されていない）で、自己承認でなく（許可されていれば可）、最新リビジョンへの投票、または `keep_approvals_on_trivial_rebase` のもとで以後のすべてのリビジョンと **指紋** が等しい以前のリビジョンへの投票である場合。
3. 有効な承認が `required_approvals` 以上ある。
4. 有資格者の `REQUEST_CHANGES` が残っていない（ブロックする設定の場合）。
5. 未解決の公開スレッドが残っていない（必須の場合）。
6. 呼び出したアカウントが target に書き込める。

指紋は、スタックの各 Changeset について第一親との変更を（パス → 変更後の entry）として順に並べてハッシュしたものです。各 Changeset 自身の変更を変えない rebase（trivial rebase）では指紋は変わりません。新しい base だけに由来する違いは検出しません（Gerrit と同じ割り切り）。

## Submit

`Submit(number, expected_revision, method, idempotency_key)`:

- キーはアカウント・レビュー・リビジョン・方式に結び付き、再送・再実行では保存済みの結果（`replayed`）を返します。
- 新しいリビジョンがある、または条件を満たしていない場合は `NOT_READY` と状態を返します。
- `REBASE` は各 Changeset を target の先端へ順に適用します（作者は保持、コミッターは Submit したアカウント）。先端が既に base でトレーラーが無効なら、元の Changeset をそのまま着地させます。`SQUASH` は累積の変更を 1 つの Changeset として、レビューのタイトルと説明をメッセージ、レビュー作者を作者として着地させます。
- 競合時は Changeset の番号とパスを含む `CONFLICT` を返し、レビューはオープンのままです。`RebaseReview`（Web UI の「target に追従」）を使うか、CLI で解消してリビジョンを追加してください。
- ref の compare-and-swap・ref_log・`SUBMITTED` 化・冪等結果は **1 トランザクション** で確定します。target やレビューが変わっていれば条件を再判定し、試行予算（Land と同じ `max_attempts`、`timeout_ms`）の範囲で繰り返します。尽きれば `RETRY` です。

`RebaseReview(number, expected_revision, onto?)` は最新のスタックを target の先端（または `onto`）へ rebase してリビジョンを追加します。競合時はレビューを変えずに競合を返します。

## コメント

コメントはリビジョンに属し、任意で行に固定できます: `Anchor { path, side: HEAD | BASE, line, changeset_index? }`（`changeset_index` なしは累積差分上の行）。返信はスレッドの根を指し、解決できるのは根だけです。下書き（`SaveDraft`）は `Publish` まで作者にだけ見えます。`Publish` は呼び出したアカウントの下書きすべて、任意のトップレベルコメント、任意の投票を原子的に公開します。作者はコメントを編集でき、削除は tombstone を残します。1 レビューあたりの上限は、リビジョン 256、コメント 10,000、スタック 64 Changeset、タイトル 256 bytes、説明とコメント 64 KiB です。

## 権限

| 操作 | 必要なもの |
|---|---|
| レビューとコメントの閲覧 | `read_directories` のない読み取り権限（ディレクトリ限定アカウントは `NOT_FOUND`） |
| コメント・投票 | 上記に加えて匿名でないアカウント（`approvers` 外の投票は記録するが数えない） |
| 作成・リビジョン追加・rebase | target への書き込み権限（レビュー要件を除く） |
| タイトル・説明の編集、Close、Reopen | 作者またはリポジトリ管理者 |
| Submit | 承認条件の 6 |

不可視リポジトリのレビューは `NOT_FOUND` で、存在しないものと区別できません。

## API

gRPC サービス `kasu.v1.Review` と HTTP/JSON（`POST`、同じメッセージ）:

| RPC | HTTP |
|---|---|
| `CreateReview`（冪等キー） | `/v1/review/create` |
| `GetReview` | `/v1/review/get` |
| `ListReviews`（state・target・author。新しい順、既定 50 件、最大 500 件、cursor） | `/v1/review/list` |
| `UpdateReview`（`version` の CAS） | `/v1/review/update` |
| `AddRevision`（`expected_revision` の CAS） | `/v1/review/add-revision` |
| `RebaseReview` | `/v1/review/rebase` |
| `ListComments` / `SaveDraft` / `EditComment` / `ResolveThread` / `Publish` | `/v1/review/list-comments`、`save-draft`、`edit-comment`、`resolve-thread`、`publish` |
| `GetSubmitStatus` / `Submit` | `/v1/review/submit-status`、`/v1/review/submit` |

## CLI と Webhook

`kasu review list|show|create|vote|comment|update|rebase|submit` は同じ API を使います（[ネイティブ CLI](native-cli_ja.md)）。レビューイベントはリポジトリの Webhook endpoint に `X-GitHub-Event: kasu_review` で配信します（[Webhook](webhooks_ja.md#レビューイベント)）。

## 保存

`kasu-store` は `ReviewStore` を SQLite と PostgreSQL で、ref と同じデータベースに実装しています（`review_counters`、`reviews`、`review_revisions`、`review_votes`、`review_comments`、`review_submissions`、`review_log`。リポジトリキーで分割）。レビューの変更はすべて version の compare-and-swap を伴う 1 トランザクションで、Submit はそこに ref の更新を加えます。`review_log` は作成・リビジョン・公開・解決・更新・Submit を記録し、将来の Webhook や通知に使います。改名してもレビューは残り（キーは不変）、フォークはレビューを複製せず、リポジトリの削除時は ref とともに消去します。
