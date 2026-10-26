# マルチリポジトリ

1 つの kasu-server（同じ MetaStore / BlobStore を共有するクラスタ）で複数のリポジトリをホストします。
設計は [06. マルチリポジトリ](plan/06-multi-repository.md) です。
リポジトリ同士は権限・ref・Git OID 索引・設定・Webhook を共有しません。
不変オブジェクト（Blob / Tree / Changeset / Derived Data）は content-addressed のままストレージ上で共有し、
各リポジトリは自分の所属索引に載ったオブジェクトだけを扱えます。

## 名前とキー

| 用語 | 内容 |
|---|---|
| リポジトリ名 | `/` 区切り 1〜4 セグメント、各 `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`、全体 256 バイト以内。`.git` 終端と先頭セグメント `-`・`admin`・`api`・`metrics`・`v1` は使えません。一意性は ASCII 小文字で判定し、表示は登録時の表記です |
| `RepoKey` | 不変の内部識別子。新規リポジトリは `r1\0` + 16 バイト乱数で、再利用しません。refs、ref_log、Land 結果、webhook cursor、所属索引、Git OID 索引、読み取り scope 名前空間はすべてこのキーで保存します |
| legacy キー | レジストリ導入前の単一リポジトリは、名前のバイト列をそのままキーとして登録します。既存のキーは書き換えません |

改名すると旧名は期限付き alias として残り、旧 URL・旧名の RPC は新しいリポジトリとして解決します。
alias が生きている間は旧名で新規作成できません。期限は `KASU_REPOSITORY_ALIAS_TTL_SECS`（既定 30 日、`0` で alias なし）です。

レジストリは SQLite と PostgreSQL の `repository_records` / `repository_names` / `repository_log` に保存します。
名前の一意性と record の更新は同じトランザクションで行い、すべての変更は `repository_log` に追記します。
サーバーはレジストリを短い TTL（`KASU_REGISTRY_CACHE_MS`、既定 1000 ms）でキャッシュし、
250 ms ごとに `repository_log` を追って変更を即時に無効化します。
ref の CAS・Land の確定・Git push の公開は、同じトランザクション内でリポジトリの状態を再確認します。
キャッシュが古くても、Active でないリポジトリへの書き込みは確定しません。

## 起動と移行

```sh
# 既存の単一リポジトリ環境を登録（冪等）。名前のバイト列が legacy キーになります。
kasu-server migrate-registry /srv/kasu demo

# すべての登録済みリポジトリを提供します。
KASU_TOKEN_FILE=/etc/kasu/operator.pat kasu-server /srv/kasu [grpc-address] [http-address]

# 従来の形式。demo を legacy リポジトリとして扱い、他の登録済みリポジトリも提供します。
KASU_TOKEN_FILE=/etc/kasu/operator.pat kasu-server /srv/kasu demo
```

従来形式でレジストリが空の場合は、引数の名前を legacy リポジトリとして自動登録します（1 リリースの互換モード）。
レジストリが空でなく名前が未登録なら起動を中止します。
リポジトリ名は `:` を含まないため、2 番目の引数がアドレスか名前かは曖昧になりません。
legacy リポジトリは、`grants` を持たない旧形式のアカウント、`directories` だけの射影アカウント、
`KASU_WEBHOOKS_FILE` の endpoint の対象です。
移行後も既存の Git OID、Derived Data のキー、webhook cursor は変わりません。
一度削除した legacy キーは `migrate-registry` で再登録できません。

`kasu-git-ssh <kasu-directory> <credential-file>` は、クライアントのコマンド（`git-upload-pack 'team/app.git'`）から
リポジトリを解決します。従来の `kasu-git-ssh <kasu-directory> <repository-name> <credential-file>` は、
指定したリポジトリだけを提供します。

## リポジトリ単位の設定

`KASU_REPOSITORIES_FILE` でリポジトリと設定を宣言できます。起動時、SIGHUP、
`KASU_REPOSITORIES_POLL_MS`（既定 30 秒）ごとにレジストリへ同期します。
未登録のリポジトリは新しいキーで作成し、宣言にない登録済みリポジトリは警告するだけで削除しません。

```json
{
  "repositories": [
    {
      "name": "team-a/infra",
      "default_branch": "main",
      "write_policy": { "allowed_ref_patterns": ["refs/heads/main", "refs/heads/automation/*"] },
      "webhooks": [
        { "id": "argocd", "url": "https://argocd.example.com/api/webhook", "secret_file": "secrets/argocd" }
      ],
      "limits": { "active": 8, "pending": 64 }
    },
    { "name": "team-b/app" }
  ]
}
```

| 設定 | 扱い |
|---|---|
| `write_policy` | サーバー全体（`KASU_WRITE_POLICY_FILE`）∧ リポジトリ ∧ アカウントの grant。どれも他を拡大しません |
| `webhooks` | このリポジトリの ref 変更を通知する endpoint。`secret_file` の相対パスはファイルの場所が基準です |
| `limits` | このリポジトリの同時実行数・待機数。サーバー全体の `per_repository_*` が上限です |
| `default_branch` | Git の HEAD symref と webhook の `default_branch` |

`KASU_REPOSITORIES_SOURCE=file`（既定）ではファイルが正で、Admin API による設定・既定 branch の変更は拒否します。
`registry` ではファイルは未登録リポジトリの作成だけに使い、設定は Admin API で管理します。
どちらも `config_version` の compare-and-swap で同時変更を検出します。

## 認証・認可

アカウントは `grants` でリポジトリごとの権限を持ちます。旧形式の `write_policy` / `read_directories` は
legacy リポジトリへの単一 grant として読み替えます。両方を同じアカウントに書くことはできません。

```json
{
  "accounts": [
    {
      "name": "deploy-bot",
      "token_file": "secrets/deploy-bot.pat",
      "grants": [
        { "repositories": ["team-a/*"],
          "write_policy": { "allowed_ref_patterns": ["refs/heads/automation/*"], "allowed_path_prefixes": ["deploy"] } },
        { "repositories": ["shared/charts"], "read_directories": ["charts/team-a"] }
      ]
    }
  ]
}
```

- `repositories` は大文字小文字を区別しない glob です。`*` は 1 セグメント内、`**` セグメントは 1 個以上のセグメントに一致します。
- 一致する grant が複数ある場合は最も具体的な 1 件（ワイルドカードなし > 具体的なセグメント数 > 具体的な文字数 > `**` の少なさ、同順位は先の grant）を使います。権限の和にはしません。
- grant は、リクエストで使った名前ではなく、解決後のリポジトリの現在名と過去の主名に照合します。改名しても権限は外れず、alias の文字列が照合に使われることはありません。
- grant のないリポジトリは存在しないリポジトリと区別できない `NotFound`（Git HTTP は 404）になります。運用者 PAT はすべてのリポジトリと Admin API を使えます。
- 射影専用アカウントは `"projections": [{"repository": "team-a/*", "directories": ["deploy"]}]` で `(リポジトリ, パス)` を指定します。旧形式の `directories` は legacy リポジトリの射影です。
- cache → server の hop 資格情報は `KASU_OBJECT_REPOSITORIES`（カンマ区切りの glob、既定 `**`）のリポジトリだけを取得できます。

## 状態と存在の秘匿

| 状態 | 挙動 |
|---|---|
| `ACTIVE` | すべての操作が可能 |
| `READ_ONLY` | 読み取りと webhook は継続。Writer RPC は `FailedPrecondition`（HTTP 412）、receive-pack は ref ごとに `ng ... repository is read-only` |
| `ARCHIVED` | `READ_ONLY` に加えて webhook の配信を停止 |
| `DELETING` | 名前と alias を即座に解放し、すべての RPC が `NotFound` |

他のリポジトリにだけあるオブジェクトは、存在しないオブジェクトと同じエラーになります。
UploadBlobs の「既存はスキップ」は当該リポジトリの所属で判定するため、他リポジトリにある blob でも本文の送信が必要です。
受信した本文のハッシュを検証してから所属だけを追加し、保存済みの frame は再利用します。
CreateChangeset の親と blob、Merge / Rebase の入力、receive-pack の thin pack の delta 基底と接続性は、
すべて当該リポジトリの所属だけで解決します。

## Admin API

`kasu.v1.Admin` を gRPC と HTTP JSON（`/v1/admin/<method>`）で提供します。

| RPC | HTTP | 説明 |
|---|---|---|
| `CreateRepository` | `/v1/admin/create-repository` | unborn HEAD で作成。`idempotency_key` を同じ内容で再送すると `replayed=true` |
| `GetRepository` | `/v1/admin/get-repository` | 呼び出し元に見えるリポジトリだけ。`config_json` は運用者にだけ返します |
| `ListRepositories` | `/v1/admin/list-repositories` | `prefix` と `cursor` でページング（既定 100 件、最大 1000 件） |
| `UpdateRepository` | `/v1/admin/update-repository` | `expected_config_version` の CAS で `config_json`・`state`・`default_branch` を変更 |
| `RenameRepository` | `/v1/admin/rename-repository` | `RepoKey` は不変。`keep_alias` で旧名を alias として残します |
| `DeleteRepository` | `/v1/admin/delete-repository` | `DELETING` へ遷移。データの削除は非同期です |
| `ForkRepository` | `/v1/admin/fork-repository` | 指定 ref から到達可能な履歴・tree・blob・raw Git オブジェクトの所属を新しいキーに追加（本文はコピーしません）。同じ冪等キーの再送で中断から再開します。フォーク元と同じ commit は同じ Git OID です |
| `GetRepositoryUsage` | `/v1/admin/repository-usage` | ネイティブ ref から到達可能なオブジェクトの論理サイズ。ref の状態ごとにキャッシュします。共有オブジェクトは各リポジトリで全量を数えます |

作成・更新・改名・削除・フォークは運用者の資格情報が必要です。
衝突は `AlreadyExists` / `Aborted`（HTTP 409）で返します。

## 削除とストレージ回収

削除は 2 段階です。`DeleteRepository` で `DELETING` に遷移し、全アクセスを止めて名前を解放します。
バックグラウンドの purge（`KASU_PURGE_INTERVAL_MS`、既定 60 秒）は refs、Git refs、Land 結果、webhook 状態、
所属索引、Git OID 索引、公開・到達索引を削除します。ref_log は監査用に
`KASU_DELETED_REF_LOG_RETENTION_SECS`（既定 90 日）保持してから削除します。
`RepoKey` は再利用しないため、同名で再作成しても旧データは見えません。

共有不変オブジェクト（blob / tree / changeset の本文と Derived Data）の物理回収は対象外です。
キーで列挙できない索引（ディレクトリ読み取り scope の所属、読み取り root のキャッシュ、射影ごとの Derived Data）も
残りますが、どのリポジトリからも参照されません。必要になった時点で、全リポジトリの所属索引を根とする
オフライン mark-and-sweep を別ツールとして追加します。

## 多テナント運用

- **公平性**: admission はリポジトリごとに同時実行数（`ServerLimits.per_repository_active`、既定 `active` の半分）と待機数（`per_repository_pending`、既定 `pending` の 1/4）を制限します。混んでいるリポジトリで待つ要求は、待機中にサーバー全体の実行枠を返すため、他のリポジトリを飢餓させません。IP / PAT の制限とは積で適用します。
- **メトリクス**: `kasu_server_repository_requests_total` / `_rejected_total` / `_active` は、`KASU_METRICS_REPOSITORIES`（カンマ区切りの名前、最大 256）のリポジトリだけを `repository` ラベルにし、それ以外は `other` に集約します。
- **トレース**: サーバー span の `kasu.repository` 属性に `RepoKey`（`r1-<hex>` または `legacy-<name>`）を記録します。

## Webhook

`KASU_WEBHOOKS_FILE` は配信設定（ポーリング間隔、タイムアウト、再試行、本文上限）と `public_url` を与え、
`endpoints` があれば legacy リポジトリの endpoint になります。リポジトリごとの endpoint は
`KASU_REPOSITORIES_FILE` または Admin API の設定で指定します。
supervisor が Active / ReadOnly のリポジトリごとに dispatcher を起動し、改名・設定・状態の変更時に作り直します。
cursor はリポジトリのキーごとに保存するため、作り直しても再配信しません。
payload の `repository.name` / `full_name` / `clone_url` は現在の名前から生成します
（`team-a/infra` は `full_name="team-a/infra"`, `name="infra"`。1 セグメントの名前は従来どおり `kasu/<name>`）。
`clone_url` は `KASU_PUBLIC_URL`（または `public_url`）+ `/<name>.git` です。

## クライアント

- `kasu clone <server> <repo> [destination]` は既定 branch（取得できなければ `main`）を checkout します。working copy はサーバー URL とリポジトリ名を記録します。
- `kasu repo list [--prefix P]` / `kasu repo create <name> [--default-branch B]` / `kasu repo show <name>` は Admin API を使います（`--server` / `KASU_SERVER`）。
- `kasu-ingest --create <git-repository> <kasu-directory> <name>` は未登録のリポジトリを作成してから取り込みます。`--create` なしで未登録の名前を指定するとエラーです（レジストリが空の場合だけ legacy として登録します）。リポジトリが異なる取り込みは並行実行できます。
- `kasu-cache` / `kasu-daemon` は要求の `repo` ごとに扱います。`--repo` は省略可能・複数指定可能な許可リストで、それ以外の名前は `NotFound` です。frame は ID 単位で共有し、上位がそのリポジトリでの所属を一度確認した `(repo, id)` だけをヒットにします。所属は disk カタログと snapshot に保存し、サーバーが返すリポジトリ識別子（`x-kasu-repository`）が名前に対して変わると、その名前の所属を破棄します。`kasu-daemon snapshot --repo A --repo B` で対象リポジトリを選べます。単一リポジトリ時代のカタログと snapshot は、そのリポジトリの所属として読み込みます。

## 検証

| 種別 | テスト |
|---|---|
| レジストリ | `kasu-store/tests/registry.rs`（メモリ・SQLite・PostgreSQL 共通の契約、状態の Tx 内再確認、範囲削除） |
| 隔離（プロパティ） | `kasu-server/tests/isolation.rs`（ランダムなリポジトリ・共有 blob・フォークで Metadata / Objects / Git upload-pack） |
| 存在オラクル | `kasu-server/tests/multi_repository.rs`（UploadBlobs、親、thin pack の delta 基底） |
| ライフサイクル | `kasu-server/tests/admin.rs`、`kasu-sim/tests/repository_lifecycle.rs`（削除と並行する Land） |
| 移行 | `migrating_a_single_repository_keeps_every_key_and_git_oid` |
| 認可 | `grants_choose_the_most_specific_match_and_hide_other_repositories`、`accounts.rs` の単体テスト |
| 公平性 | `admission.rs` の単体テスト、`kasu-sim/tests/repository_fairness.rs` |
| cache / CLI / ingest | `kasu-cache/tests/repositories.rs`、`kasu-cli/tests/repositories.rs`、`kasu-ingest/tests/roundtrip.rs` |
| プロセス | `kasu-server/tests/multi_repository_process.rs`（実 server バイナリ、宣言ファイル、署名付き webhook、`git clone`） |
| E2E | `tools/gitops/multi_repository.py`（[kind 上の GitOps](gitops-e2e.md#複数リポジトリ)） |
