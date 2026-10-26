# 06. マルチリポジトリ

1 つの kasu-server クラスタ（同一 MetaStore / BlobStore）で複数リポジトリをホストする。
リポジトリはクライアントから見て完全に独立した名前空間であり、権限・ref・Git OID 索引・設定・Webhook を共有しない。
一方、不変オブジェクト（Blob / Tree / Changeset / Derived Data）は content-addressed のまま **ストレージ上は共有** し、
重複排除とフォークの安価さを得る。境界の担保は「リポジトリ所属索引」で行う。

## 現状と課題

| 箇所 | 現状 | 課題 |
|---|---|---|
| `kasu-server` / `kasu-git-ssh` | 起動引数 `<repository-name>` で 1 リポジトリに固定（`Backend.repository`） | 要求ごとのリポジトリ解決がない。別リポジトリは `PermissionDenied` |
| `kasu-proto` | 全要求に `repo` フィールドあり | 意味は「固定リポジトリとの照合」だけ |
| RefStore / Land / Webhook 状態 | `(repo, …)` 主キー | そのまま複数化できる |
| 所属索引 | `repo/{changeset,tree,blob}/v1/<repo>/<id>` | キーがリポジトリ **名** に依存（改名・削除後の名前再利用で問題） |
| `ProjectionId` / 読み取り scope 名前空間 | リポジトリ名から導出 | 同上 |
| 設定（WritePolicy / accounts / webhooks / limits） | サーバー全体で 1 つ | リポジトリ単位の設定ができない |
| `kasu-cache` / `kasu-daemon` | 起動時に repo 固定 | ノード上の複数リポジトリで別プロセスが必要 |
| `kasu-ingest` | 宛先リポジトリ名を引数で指定 | 登録（作成）手順が別途必要 |

## 設計原則

1. **名前と識別子の分離**: 外部名（`team-a/infra`）と内部キー `RepoKey`（不変・再利用しない）を分ける。
   全ての永続キー・Derived Data の SubKey・名前空間は `RepoKey` を使う。
2. **オブジェクトは共有、可視性は所属で決める**: 不変オブジェクトは全リポジトリ共通の put-if-absent ストアに置き、
   各リポジトリは所属索引に載ったオブジェクトだけを読み書きの対象にできる。
3. **存在の秘匿**: 他リポジトリにだけ存在するオブジェクト・リポジトリの存在を、応答・エラー・タイミング差以外の
   手段で漏らさない（重複排除による存在オラクルを作らない）。
4. **リポジトリ間トランザクションなし**: ref の原子性・複数 ref CAS はリポジトリ内に閉じる。
   RefStore のパーティションキーは `RepoKey` とし、分散 KV でも単一パーティション Tx で完結させる。
5. **ステートレスの維持**: リポジトリ一覧はサーバーのメモリに固定せず、レジストリから TTL 付きで読む。
   どの server ノードも全リポジトリを処理できる（リポジトリ単位のシャーディングは前提にしない）。

## リポジトリレジストリ

```
repositories : RepoKey -> RepositoryRecord          # 単一行 CAS（version 付き）
repo_names   : normalized_name -> RepoKey           # 一意制約。改名時は旧名を alias として保持可能
repo_log     : seq -> (RepoKey, event, actor, time) # 作成/改名/状態変更/設定変更の監査ログ（変更フィード）

struct RepositoryRecord {
    key: RepoKey,                 // 16 バイト乱数（新規）または移行用 legacy キー（後述）
    name: RepoName,
    state: Active | ReadOnly | Archived | Deleting,
    default_branch: BString,      // HEAD symref・Webhook の default_branch
    git_commit_rule: u32,         // 合成 commit 規則のバージョン。作成時に固定し以後変更しない（04）
    config_version: u64,          // リポジトリ設定（下記）の世代
    created: (actor, time),
}
```

- レジストリは `RepositoryStore` trait として `kasu-store` に追加する（SQLite / PostgreSQL / 分散 KV）。
  名前の一意制約と record 更新を同一 Tx で行う能力を必須とする。
- server はレジストリを短 TTL のキャッシュで読む。状態変更（ReadOnly / Archived / Deleting）は `repo_log` の tail で
  即時無効化し、書き込み経路では ref CAS と同じ Tx 内で `state` を再確認する（削除中リポジトリへの着地を防ぐ）。

### 名前規則

- `RepoName` = `/` 区切りの 1〜4 セグメント、各セグメント `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`、全体 256 バイト以内。
- 禁止: `.git` 終端、`-` 単独セグメント（射影 URL の `/-/` と衝突）、`.` / `..`、予約トップレベル名
  （`metrics`, `api`, `v1`, `admin`, `-` 等 HTTP ルートと衝突するもの）。
- 一意性は小文字正規化で判定する（大文字小文字違いの別リポジトリを作らない）。表示は登録時の表記。
- 改名すると旧名は alias として残し、Git HTTP では旧 URL を新 URL として解決する（期限付き・設定で無効化可能）。
  alias が残っている間は旧名で新規作成できない。

### `RepoKey` と既存データの移行

- 既存の単一リポジトリは、**現在のリポジトリ名バイト列をそのまま legacy `RepoKey`** として登録する。
  これにより refs / ref_log / land_results / webhook_state / 所属索引 / `ProjectionId` / 読み取り scope 名前空間を
  書き換えずに済み、既存の Git OID・Derived Data キーは変わらない。
- 新規リポジトリの `RepoKey` は `b"r1\0"` + 16 バイト乱数とし、legacy キー（名前規則に合致する ASCII）と衝突しない。
- 移行は `kasu-server migrate-registry <kasu-directory> <repository-name>` の冪等コマンドで行う。
  レジストリが空のまま起動した場合は従来の起動引数を legacy キーとして自動登録する互換モードを 1 リリース残す。

## オブジェクト共有と所属

| オブジェクト | 保存 | 所属（可視性） |
|---|---|---|
| Blob / Tree / Changeset / raw Git object | 全体共通の put-if-absent | `repo/<kind>/v1/<RepoKey>/<id>` |
| Derived Data（RootTree, Generation, Blame 等） | 全体共通（`ChangesetId` キー、決定的なので共有可） | 入力 Changeset の所属で判定 |
| Git OID 索引 / GitTree / GitCommit | `ProjectionId`（= `RepoKey` + 射影パス）ごと | リポジトリ固有 |
| refs / git_refs / ref_log / land_results / webhook_state | `RepoKey` パーティション | リポジトリ固有 |
| 永続到達索引・pack キャッシュ | `RepoKey`（射影は `ProjectionId`）ごと | リポジトリ固有 |

### 存在オラクルの防止

- `UploadBlobs` の「既存はスキップ」判定は **当該リポジトリの所属** で行う。全体ストアに存在していても所属がなければ
  クライアントに本文の送信を要求し、受信本文のハッシュ検証後に所属だけを追加する（本文の再保存は put-if-absent で無害）。
- `CreateChangeset` の親・`FileChange` の blob、`Rebase` / `Merge` の入力、receive-pack の thin pack 基底・接続性チェックは、
  全て当該リポジトリの所属オブジェクトだけで解決する。
- 未所属 ID と存在しない ID は同じエラー（`NotFound`）を返す。
- 権限のないリポジトリと存在しないリポジトリも、運用者以外には同じ `NotFound`（Git HTTP は 404）を返す
  （現在の `PermissionDenied` を変更）。

### フォーク（後半）

- `ForkRepository(source, ref, new_name)`: 新しい `RepoKey` を作り、指定 ref から到達可能なオブジェクトの所属を
  新リポジトリに追加する（本文はコピーしない）。所属の追加は Derived Data 同様に反復・冪等で、途中失敗から再開できる。
- 合成 Git commit は Changeset 内容のみから決まるため（04）、フォーク元と同じ commit は同じ Git OID になる。

## リポジトリ単位の設定

サーバー全体の設定はクラスタの上限として残し、リポジトリ設定はその範囲内でのみ有効にする（既存の
「アカウント policy はサーバー policy を拡大しない」と同じ積の規則）。

| 設定 | 現在 | マルチリポジトリ後 |
|---|---|---|
| WritePolicy（ref パターン・force・作成/削除） | `KASU_WRITE_POLICY_FILE`（全体） | 全体 policy ∧ リポジトリ policy |
| Webhook endpoint・secret・`repository_url` | `KASU_WEBHOOKS_FILE`（1 リポジトリ分） | リポジトリごとの endpoint 列。dispatcher は全 Active リポジトリを巡回 |
| receive / pack / admission の上限 | `KASU_SERVER_LIMITS_FILE`（全体） | 全体上限 + リポジトリ単位の公平配分（後述） |
| default branch / Git 合成規則 | 暗黙 `main` / 固定 | レジストリの record |

- 初期実装は宣言的ファイル `KASU_REPOSITORIES_FILE`（JSON、既存設定ファイルと同じ相対パス規則）で
  `name` / `default_branch` / `write_policy` / `webhooks` / `limits` を与え、起動時と SIGHUP / 定期ポーリングで
  レジストリに同期する（宣言にないリポジトリは削除せず警告のみ）。
- Admin API（後述）実装後は、ファイルとレジストリのどちらを正とするかを起動オプションで選ぶ。両方からの同時変更は
  `config_version` の CAS で検出して拒否する。

## 認証・認可

アカウントにリポジトリ単位の権限付与（grant）を追加する。既存の `write_policy` / `read_directories` は grant の中に移る。

```json
{
  "accounts": [
    {
      "name": "deploy-bot",
      "token_file": "secrets/deploy-bot.pat",
      "grants": [
        { "repositories": ["team-a/*"], "read": true,
          "write_policy": { "allowed_ref_patterns": ["refs/heads/automation/*"], "allowed_path_prefixes": ["deploy"] } },
        { "repositories": ["shared/charts"], "read": true, "read_directories": ["charts/team-a"] }
      ]
    }
  ]
}
```

- `repositories` はリポジトリ名の glob（`*` は 1 セグメント、`**` は複数セグメント）。複数 grant が一致した場合は
  **最も具体的な 1 件** を採用する（権限の和にはしない。予測可能性のため）。
- grant のないリポジトリは不可視（`NotFound`）。運用者 PAT は全リポジトリ・Admin API を許可する。
- 照合は名前ではなく、解決後の `RepoKey` に対して行う（改名で権限が外れない / 旧名の alias で権限を得ない）。
  grant はロード時と `repo_log` 更新時に `RepoKey` 集合へ展開し直す。
- 射影専用アカウント（`KASU_PROJECTION_ACCOUNTS_FILE`）は `(repository, path)` の組で指定する。
- 旧形式（`grants` なしの `write_policy` / `read_directories`）は「legacy リポジトリへの単一 grant」として読み替える。
- Object hop 資格情報（cache → server）にもリポジトリ glob の許可範囲を持たせ、リージョン cache ごとに
  取り扱えるリポジトリを限定できるようにする。

## API の変更

### リポジトリ解決

- 全 RPC の `repo` フィールドを **リポジトリ名** として解釈し、要求の最初にレジストリで `RepoKey` を解決する。
  以後の処理（所属・ref・Derived Data の SubKey）は全て `RepoKey` を使う。`Backend` は固定 `repository` を持たず、
  要求ごとに `RepoContext { key, record, grant, read_scope, policy }` を構築する。
- 状態ごとの挙動: `ReadOnly` は Writer RPC・receive-pack を `FailedPrecondition`（Git は `report-status` で拒否）、
  `Archived` は加えて Webhook 配信を停止、`Deleting` は全 RPC を `NotFound`。

### Admin API（`kasu.v1.Admin`、gRPC + HTTP/JSON）

| RPC | 説明 |
|---|---|
| `CreateRepository(name, default_branch, config)` | レジストリ作成（unborn HEAD）。冪等キー対応 |
| `GetRepository(name)` / `ListRepositories(prefix, cursor)` | 呼び出し元に可視なものだけを返す（非運用者は grant のあるもの） |
| `UpdateRepository(name, expected_config_version, patch)` | 設定・状態変更（CAS） |
| `RenameRepository(name, new_name, keep_alias)` | 改名。`RepoKey` は不変 |
| `DeleteRepository(name)` | `Deleting` へ遷移 → 非同期に ref / 所属 / 索引を削除（下記） |
| `ForkRepository(source, ref, new_name)` | 後半フェーズ |

### Git

- Smart HTTP: `https://host/<repo-name>.git`（`<repo-name>` は `/` を含み得る）。射影は従来どおり
  `/<repo-name>/-/path/<dir>.git`。`-` 単独セグメントを名前で禁止しているので曖昧さはない。
- SSH: `kasu-git-ssh <kasu-directory> <credential-file>` とし、リポジトリはクライアントが送る
  `git-upload-pack '<repo-name>.git'` の引数から解決する（旧形式の固定引数は互換のため残す）。
- Webhook payload の `repository.name` / `full_name` / `clone_url` はレジストリの現在名から生成する。

### CLI / ingest / cache / daemon

- `kasu`: working copy の `.kasu/` にサーバー URL とリポジトリ名を記録済み。`kasu clone <server> <repo>` と
  `kasu repo list|create|show` を追加する。
- `kasu-ingest`: `--create` で未登録リポジトリをレジストリに作成してから取り込む。リポジトリが異なる取り込みは並行実行できる。
- `kasu-cache` / `kasu-daemon`: 起動時の repo 固定をやめ、要求の `repo` ごとに扱う。本文 frame は ID 単位で共有し、
  **(repo, id) の所属を記録した場合だけヒット** とする（他リポジトリで取得済みの本文でも、当該リポジトリでの上位
  確認が 1 回済むまでは返さない）。snapshot は対象リポジトリ集合を指定して作成できる。

## 削除とストレージ回収

- 削除は 2 段階: (1) `Deleting` に遷移し全アクセスを停止、名前を解放（alias も削除）。
  (2) バックグラウンドで当該 `RepoKey` の refs / git_refs / land_results / webhook_state / 所属索引 / Git OID 索引 /
  到達索引を範囲削除する。`RepoKey` は再利用しないため、同名で再作成しても旧データは決して見えない。
- 共有不変オブジェクトの物理回収は **初期スコープ外**（「メンテナンス不要」の原則を維持）。必要になった時点で、
  全リポジトリの所属索引を根とするオフライン mark-and-sweep を別ツールとして追加する。
  ref_log は監査用途のため保持期間を設定で決める。

## 多テナント運用

- **公平性**: admission にリポジトリ単位の同時実行・待機数の上限を追加し、1 リポジトリの重い clone / blame /
  Land 競合が他リポジトリを飢餓させないようにする（既存の IP / PAT 制限と積で適用）。
- **メトリクス**: リポジトリをラベルにすると cardinality が上限なく増えるため、既定ではラベルにしない。
  設定した許可リストのリポジトリだけラベル化し、それ以外は `other` に集約する。トレースの属性には `RepoKey` を付ける。
- **容量の可視化**: リポジトリ単位の論理サイズ（所属オブジェクトの合計）を Lazy に集計して Admin API で返す。
  共有があるので物理使用量の按分はしない。

## テスト

| 種別 | 内容 |
|---|---|
| 隔離（プロパティ） | ランダムな複数リポジトリ・共有 blob・フォークで、全 RPC / Git fetch / Objects が他リポジトリ専用オブジェクトを返さず、未所属と不存在が区別不能 |
| 存在オラクル | 他リポジトリにだけある blob の UploadBlobs が本文送信を要求すること。thin pack の基底に他リポジトリの OID を使った push の拒否 |
| ライフサイクル | 作成 → 書き込み → 改名（alias 経由の clone）→ 削除 → 同名再作成で旧 ref / OID が見えない。削除中の Land / push が確定しない（並行シミュレーション） |
| 移行 | 既存単一リポジトリのデータを `migrate-registry` 後に起動し、全 Git OID・Derived Data キー・webhook cursor が不変 |
| 認可 | grant の glob・最具体一致・改名後の権限維持・旧形式アカウントの読み替え |
| 公平性 | 1 リポジトリへの過負荷中に他リポジトリのレイテンシが上限内（`kasu-sim`） |
| E2E | 1 server 上の複数リポジトリを ArgoCD / Flux が別々に同期し、Webhook がリポジトリごとに正しい `repository` で届く |
