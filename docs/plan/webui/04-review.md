# 04. レビュー

レビューは「ある ref（target）へ着地させたい Changeset スタック」に対する提案・議論・承認・着地の単位である。
データと判定は kasu-server の `kasu.v1.Review` に置き、Web UI・CLI・bot が同じ API を使う。
Changeset は不変なので、レビューは Changeset ID の列を参照するだけで、オブジェクトを複製しない。

## 用語

| 用語 | 意味 |
|---|---|
| target | 着地先の ref（例 `refs/heads/main`） |
| スタック | `c1 ← c2 ← … ← cn` の Changeset 列。`c(i+1)` の第一親が `ci` |
| base | `c1` の第一親。スタック全体の累積差分は `diff(base, cn)` |
| リビジョン | スタックの版。更新（修正・rebase）のたびに番号を 1 増やして追加し、過去版は消さない |
| 投票 | アカウントがリビジョンに対して付ける `APPROVE` / `REQUEST_CHANGES` / `NONE` |
| Submit | 承認条件を満たしたレビューのスタックを target に着地させる操作 |

## データモデル

```
Review {
  repo: RepoKey, number: u64,            // リポジトリ内の連番（再利用しない）
  version: u64,                          // 単一行 CAS 用
  state: OPEN | SUBMITTED | CLOSED,
  target: RefName,
  title: bytes (≤ 256), description: bytes (Markdown, ≤ 64 KiB),
  author: AccountName, created_at, updated_at,
  submitted: Option<{ by, at, method, landed: [ChangesetId], ref_version }>,
}
Revision {
  number: u32 (1..), base: ChangesetId, changesets: [ChangesetId] (1..=64),
  fingerprint: [u8; 32],                 // 変更内容の指紋（後述）
  created_by: AccountName, created_at,
}
Vote    { account, revision, value: APPROVE | REQUEST_CHANGES | NONE, at }   // アカウントごとに最新 1 件
Comment {
  id: u64 (レビュー内連番), parent: Option<u64>,     // 返信はスレッドの根を parent に持つ
  revision: u32, anchor: Option<Anchor>,             // なしはレビュー全体へのコメント
  author, body: bytes (Markdown, ≤ 64 KiB), draft: bool,
  resolved: bool (根のみ), created_at, edited_at, deleted: bool (tombstone)
}
Anchor { path: RepoPath, side: BASE | HEAD, line: u64 (1-based), changeset_index: Option<u32> }
```

- `changeset_index` なしのアンカーは累積差分（`base` → `cn`）上の行、ありはスタック内の個別 Changeset の差分上の行。
- 下書きコメント（`draft=true`）は作者本人にだけ見え、投票と一緒にまとめて公開する（GitHub の pending review 相当）。

### 指紋（fingerprint）

各 Changeset の第一親との差分を「パス → 変更後 Entry（削除はなし）」の列として正規化し、スタック順に連結して
BLAKE3 でハッシュする。base が変わっても各 Changeset の変更結果が同じなら一致する（trivial rebase）。
変更前の内容は含めないため、base 側の変化による意味の変化は検出しない（Gerrit の trivial rebase と同じ割り切り）。

### 検証規則

- 全 Changeset が当該リポジトリに所属し、スタックの第一親の連鎖が成り立つこと。
- マージ Changeset もスタックに含められる（Land と同じく第一親との差分として扱う）。
- 作成・リビジョン追加には、target に対する書き込み権限（レビュー要件を除いた実効 policy）が必要。
- 同じ target・同じ `cn` のオープンなレビューは重複作成できない（`ALREADY_EXISTS`、既存の番号を返す）。

## 状態遷移

```
          CreateReview
               │
               ▼
   ┌──────── OPEN ◀──────── Reopen（作者・管理者）
   │           │
   │ Close     │ Submit（Landed）
   ▼           ▼
 CLOSED     SUBMITTED（終端。以後はコメントのみ可）
```

- target が Submit 以外の経路（bypass アカウントの push 等）で進み、スタックの全変更が既に含まれた場合も
  自動では SUBMITTED にしない（明示的な Close を促す表示のみ）。
- リポジトリが ReadOnly / Archived の間はレビューへの全書き込みを `FAILED_PRECONDITION` にする。

## レビューポリシー

リポジトリ設定（`config_json`）に `review` を追加する。省略時は保護 ref なし（既存の挙動を変えない）。

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
        "methods": ["REBASE", "SQUASH"]
      }
    ],
    "bypass_accounts": ["image-automation-bot"]
  }
}
```

| フィールド | 既定 | 意味 |
|---|---|---|
| `ref_patterns` | 必須 | 保護する ref。既存 WritePolicy と同じパターン構文 |
| `required_approvals` | 1 | 必要な承認数（0〜10。0 は「レビュー経由を必須にするが承認は不要」） |
| `approvers` | 省略 | 承認できるアカウント名の glob。省略時は target に書き込めるアカウント全員 |
| `allow_self_approval` | false | レビュー作者・リビジョン作成者の承認を数えるか |
| `block_on_request_changes` | true | 有資格者の `REQUEST_CHANGES` が 1 件でもあれば不可 |
| `keep_approvals_on_trivial_rebase` | true | 指紋が直前リビジョンと同じなら承認を引き継ぐ |
| `require_resolved_threads` | false | 未解決スレッドがあれば不可 |
| `methods` | 両方 | 許可する着地方式 |
| `bypass_accounts` | `[]` | 保護 ref にも直接書き込めるアカウント（GitOps 書き戻し bot 等）。運用者 PAT は常に bypass |

- `rules` は先頭から順に照合し、最初に一致した 1 件を使う。
- ポリシーは Submit 時点の設定で評価する（作成時ではない）。

## 承認の判定

`GetSubmitStatus` と `Submit` は同じ関数で判定し、満たしていない条件を構造化して返す。

1. レビューが OPEN で、最新リビジョンが存在する。
2. 有効な承認 = 有資格者（`approvers` と現在の読み取り権限を満たすアカウント）の `APPROVE` のうち、
   投票したリビジョンが最新である、または `keep_approvals_on_trivial_rebase` のもとで投票リビジョンから最新まで指紋が不変であるもの。
   `allow_self_approval=false` のとき、レビュー作者と各リビジョンの作成者を除く。
3. 有効な承認数 ≥ `required_approvals`。
4. `block_on_request_changes` のとき、有資格者の `REQUEST_CHANGES`（リビジョンを問わず最新の投票）がない。
5. `require_resolved_threads` のとき、未解決の公開スレッドがない。
6. Submit を要求したアカウントが target への書き込み権限を持つ（パス ACL は着地時の最終差分で検査）。

アカウントが設定から削除された場合、その承認は数えない（判定時点のアカウント設定を使う）。

## 強制

保護 ref（いずれかの rule に一致する ref）の更新は、**Submit の内部経路**と bypass アカウントにだけ許す。

| 経路 | 保護 ref への挙動（bypass 以外） |
|---|---|
| `Writer.Land` / `/v1/land` / `/v1/guarded-land` | `Denied`、メッセージ `review required` |
| `Writer.UpdateRef` | `PERMISSION_DENIED` |
| Git receive-pack | 当該 ref のみ `ng <ref> review required`（atomic なら全体拒否） |
| 保護 ref の作成・削除 | 同様に拒否。unborn の target への最初の着地も Submit 経由 |

- 判定は既存の WritePolicy 検査と同じ場所（ref CAS 直前のポリシー検査）に追加し、Land の再試行ごとにも再検査する。
- `GetPermissions` の `review_required` / `review_bypass` で UI と CLI に事前に示す。
- bypass による保護 ref の更新は ref_log に `bypass` 理由付きで記録する。

## Submit

```
Submit(repo, number, expected_revision, method, idempotency_key, max_attempts?, timeout_ms?)
  -> { outcome: LANDED | CONFLICT | RETRY | DENIED | NOT_READY, landed: [ChangesetId], actual: Ref,
       conflict: { changeset_index, RewriteResponse }, status: SubmitStatus, replayed }
```

1. 冪等結果があれば返す（キーは「アカウント + レビュー + リビジョン + method」に結び付ける。Land と同じ規則）。
2. `expected_revision` が最新でなければ `NOT_READY`（画面を見ている間に更新された場合）。
3. 承認判定。満たさなければ `NOT_READY` と不足条件。
4. target の現在の先端へスタックを適用する。
   - `REBASE`: `c1 … cn` を順に第一親差分として 3-way rebase し、新しい Changeset 列を作る（元の作者を保持、
     コミッターは Submit したアカウント）。先端が既に base なら再作成しない。
   - `SQUASH`: 累積差分を 1 つの Changeset として適用。メッセージはタイトル + 説明、作者はレビュー作者。
5. 競合したら `CONFLICT`（何番目の Changeset で、どのパスか）を返し、レビューは OPEN のまま。
6. **ref CAS・ref_log・レビューの SUBMITTED 化・冪等結果を 1 トランザクション** で確定する。
   CAS 競合時は承認とポリシーを再判定して手順 4 から再試行する（上限・バックオフは Land と同じ）。

- 着地後のメッセージ末尾に `Review: <repo>#<number>` のトレーラーを付けるかを rule で選べる（既定 付ける）。
- Webhook は通常の ref 更新として既存の配信に乗る。

### サーバー側 rebase（Rebase Review）

`RebaseReview(repo, number, expected_revision, onto?)` は最新リビジョンのスタックを target の先端（または `onto`）へ
rebase し、競合がなければ新リビジョンとして追加する。競合時はリビジョンを追加せず競合情報を返す。
Web UI の「target に追従」ボタンから使う。

## Review API

```proto
service Review {
  rpc CreateReview(CreateReviewRequest) returns (ReviewRecord);
  rpc GetReview(GetReviewRequest) returns (ReviewRecord);
  rpc ListReviews(ListReviewsRequest) returns (ListReviewsResponse);   // state / target / author で絞り込み、cursor
  rpc UpdateReview(UpdateReviewRequest) returns (ReviewRecord);       // タイトル・説明・Close / Reopen。version の CAS
  rpc AddRevision(AddRevisionRequest) returns (ReviewRecord);         // expected_revision の CAS
  rpc RebaseReview(RebaseReviewRequest) returns (RebaseReviewResponse);
  rpc ListComments(ListCommentsRequest) returns (ListCommentsResponse);
  rpc Publish(PublishRequest) returns (PublishResponse);              // 下書きの一括公開 + 投票（原子的）
  rpc SaveDraft(SaveDraftRequest) returns (Comment);
  rpc EditComment(EditCommentRequest) returns (Comment);              // 作者のみ。削除は tombstone
  rpc ResolveThread(ResolveThreadRequest) returns (Comment);
  rpc GetSubmitStatus(GetReviewRequest) returns (SubmitStatus);
  rpc Submit(SubmitRequest) returns (SubmitResponse);
}
```

- HTTP/JSON でも既存と同じ規則（`/v1/review/<method>`）で公開する。
- `CreateReview` は冪等キーを取る。`ListReviews` は既定 50 件、最大 500 件。
- 1 レビューあたりの上限: リビジョン 256、コメント 10,000、1 スタック 64 Changeset。

## 保存

`kasu-store` に `ReviewStore` trait を追加し、SQLite / PostgreSQL で実装する。すべて `RepoKey` をパーティションキーにする。

| テーブル | 主キー | 内容 |
|---|---|---|
| `review_counters` | repo | 次の番号 |
| `reviews` | (repo, number) | 本体と version |
| `review_revisions` | (repo, number, revision) | スタック・base・指紋 |
| `review_votes` | (repo, number, account) | 最新投票 |
| `review_comments` | (repo, number, id) | コメント（索引: revision, path） |
| `review_submissions` | (repo, account, key) | Submit の冪等結果 |
| `review_log` | (repo, seq) | 作成・リビジョン・投票・公開・Submit・Close のイベント（将来の Webhook / 通知の outbox） |

- Submit は RefStore の ref CAS と `reviews` の更新を同一トランザクションで行う必要がある。SQLite / PostgreSQL では
  同一 DB のため可能。将来の分散 KV では両者を同一パーティション（`RepoKey`）に置くことを要件とし、
  RefStore の能力モデルに `review_transaction` を追加する。この能力がないバックエンドでは Review API を無効にする。
- 一覧のための索引: (repo, state, number desc)、(repo, target, state)、(repo, author, state)。

## リポジトリのライフサイクルとの関係

| 操作 | レビュー |
|---|---|
| 改名 | `RepoKey` 不変のためそのまま。`<repo>#<n>` の表示は現在名 |
| ReadOnly / Archived | 閲覧のみ |
| 削除 | 2 段階削除のバックグラウンド処理で review 系テーブルを範囲削除 |
| フォーク | レビューはコピーしない |
| マイグレーション | `migrate-registry` は対象外（レビューは新機能のため既存データなし） |

## 可視性と権限

| 操作 | 必要な権限 |
|---|---|
| 閲覧（一覧・詳細・コメント） | リポジトリの読み取り（ディレクトリ限定でない） |
| コメント・投票 | 読み取り + 非匿名（`approvers` 外の投票は記録するが承認数に数えない） |
| 作成・リビジョン追加・Rebase | target への書き込み権限（レビュー要件を除く） |
| タイトル・説明の編集、Close / Reopen | 作者、またはリポジトリ管理者 |
| Submit | 承認判定の 6 |

- `read_directories` を持つアカウントは Review API 全体を利用不可（`NOT_FOUND`）。タイトル・コメント・範囲外パスへの
  アンカーから制限範囲外の情報が漏れるため。範囲付きのレビュー閲覧は将来検討とする。
- 存在の秘匿: 不可視リポジトリのレビューは `NOT_FOUND`。レビュー番号の推測で存在を知られないよう、
  権限のないリポジトリでは番号の有無を区別しない。

## 画面（W6）

| 画面 | 内容 |
|---|---|
| `/<repo>/-/reviews` | 状態・target・作者で絞り込める一覧。承認状況のバッジ |
| `/<repo>/-/reviews/new` | compare 画面・Web 編集から遷移。target、スタック（`head` から base まで第一親を辿り選択）、タイトル、説明 |
| `/<repo>/-/reviews/<n>` | 概要: 説明、Submit 状態（不足条件の一覧）、投票一覧、タイムライン（リビジョン追加・投票・コメント・Submit） |
| `/<repo>/-/reviews/<n>/files` | 累積差分。`?rev=k` でリビジョン、`?against=j` でリビジョン間の比較。行コメント（下書き）とスレッド表示 |
| `/<repo>/-/reviews/<n>/changesets` | スタック内の各 Changeset とその個別差分 |

- リビジョン間比較は、両リビジョンの base が同じなら `diff(cn_j, cn_k)`、異なる場合は両者の累積差分を
  ファイルごとに並べて表示し、「base が異なる」ことを明示する（rebase 由来の差分を誤って変更と見せないため）。
- 古いリビジョンのコメントは、該当行の内容が最新リビジョンでも変わっていなければ最新表示に重ねて出し、
  変わっていれば「古いリビジョンのコメント」として折りたたむ。
- Submit ボタンは `GetSubmitStatus` が満たされたときだけ有効。方式（REBASE / SQUASH）を選ぶ。
  `CONFLICT` 時は競合パスを表示し、「target に追従（RebaseReview）」または CLI での解消を案内する。
- Web 編集で保護 ref を対象にした場合、作成した Changeset でレビューを作成する画面へ進む。
  既存レビューの作者が Web 編集を行う場合は「このレビューの新リビジョンとして追加」を選べる（スタック末尾に追加）。

## CLI 連携（W7）

Review API は公開 RPC なので、`kasu review create|list|show|update|vote|submit` を後続フェーズで追加する。
`kasu land` は保護 ref に対して `review required` で失敗し、`kasu review create` を案内する。
