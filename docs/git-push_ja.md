# Git Smart HTTP 書き込み

[English](git-push.md) | 日本語

Metadata／upload-pack と同じ HTTP ポートで `git-receive-pack` を提供します。
Basic（任意ユーザー名＋トークン）または Bearer で認証し、repo 単位で認可します。
認証は ref 広告と要求解析の前に行います。

```sh
git -c http.extraHeader='Authorization: Bearer dev-token' \
  push http://127.0.0.1:50080/my-repo.git main
```

Git 2.54.0／go-git v5.19.3 で初回・増分 push、通常／depth=1 の clone→commit→push を検証しました。
annotated tag、多段 tag、blob を指す tag、削除、force の許可／拒否、複数 ref の一部拒否も検証しています。
サーバーは Git コマンドを起動せず、Git リポジトリや受信 pack を常設しません。
メモリ上の pack を検証し、raw オブジェクトとネイティブ Changeset へ変換します。

## プロトコルと検証

受信の広告は v0 形式で、`report-status`、`side-band-64k`、`delete-refs`、`ofs-delta`、
`object-format=sha1`、agent を提供します。push の protocol v2 拡張は提供しません。
`report-status-v2`、push options は広告せず、要求されても拒否します。
`atomic` は branch と Git-only ref を同じ CAS／ログトランザクションで保存できる
バックエンドで広告します。SQLite は対応しています。
`--atomic` を指定しない複数 ref は個別 CAS です。
正しい pack に対するポリシー拒否や CAS 競合では、他の ref は成功できます。
pack／接続性エラーでは全 ref を拒否し、`unpack` と各 ref の `ng` を返します。

```sh
git -c http.extraHeader='Authorization: Bearer dev-token' \
  push --atomic http://127.0.0.1:50080/my-repo.git main refs/tags/v1
```

[Git の atomic capability](https://git-scm.com/docs/protocol-capabilities#_atomic) に従い、
要求時は全コマンドの old OID、ポリシー、最終 tree ACL を検査した後、
branch／tag の作成・更新・削除と両種のログ追記を1トランザクションで保存します。
1つでも拒否、競合、検査後の ref version の変化があれば全コマンドを `ng` にします。
不変オブジェクトの取り込みは先に行うため、失敗した要求のオブジェクトが残る場合はあります。
ref が同じ OID に戻る ABA 競合も、検査時に取得した version を使って拒否します。

組み込み用 `GitRefStore::supports_atomic_publication()` は既定で false です。
対応するバックエンドだけが true を返し、`compare_native_and_git` の原子性を保証します。
非対応時は広告せず、クライアントが直接送った atomic 要求も保存前に拒否します。
push の原子性は保存トランザクションに対する保証です。
独立した複数の読み取り RPC が同じ snapshot を共有する機能は提供しません。

`gix-pack` で entry と delta を解析し、SHA-1 trailer、zlib の実展開長、型、
ofs-delta の entry 境界、ref-delta の基底、最終オブジェクトサイズを検査します。
forward ref-delta と、既存の公開オブジェクトを基底にした thin pack に対応します。
薄い pack の基底と省略された依存は、この repo の公開 ref から到達できるオブジェクトに限ります。
未公開索引の OID を知っているだけでは、pack を省略して ref を作成できません。

| 既定上限 | 値 |
|---|---:|
| pack | 128 MiB |
| オブジェクト数 | 100,000 |
| 1 entry／最終オブジェクト | 64 MiB |
| 展開量（delta 命令・結果・外部基底を含む） | 512 MiB |
| delta 深さ | 64 |
| ref コマンド／shallow 宣言 | 各1,024 |

HTTP 本文は pack 上限＋コマンド領域2 MiBまでです。受信 HTTP の gzip Content-Encoding は未対応です。
組み込み時は `Backend::with_receive_limits` で上限を設定できます。
shallow 宣言は既存の公開 commit に限り受理し、新 ref まで完全な接続性を検証します。
欠落した親を持つ shallow リポジトリをサーバー側に作成する機能はありません。

接続性・型の検証が終わるまで ref を動かしません。
親が先となる順序で取り込み、第一親との差分と raw Git commit／tree／tag を保存し、元の OID を保持します。
ref の old OID を確認した後、その時点のネイティブ ref version を使って CAS します。
各成功と ref_log／git_ref_log は同じトランザクションです。
拒否した要求の不変オブジェクトが残る場合はありますが、Git の want では公開 ref からの到達性を検査します。

## ref ポリシーと最終 tree ACL

デフォルトは branch／tag の作成、branch の fast-forward、全パスへの変更を許可します。
ref の削除、branch の non-fast-forward、既存 tag の置き換えは禁止です。
書き込み可能な名前空間は `refs/heads/` と `refs/tags/` に限ります。

CLI は `KASU_WRITE_POLICY_FILE=/path/to/policy.json` を指定して起動します。
例:

```json
{
  "allowed_ref_patterns": ["refs/heads/main", "refs/heads/image-updates/*", "refs/tags/*"],
  "force_ref_patterns": ["refs/heads/image-updates/*"],
  "allowed_path_prefixes": ["app", "deploy"],
  "allow_create": true,
  "allow_delete": false
}
```

`*` は `/` を含む任意のバイト列、`?` は1バイトに一致します。
pattern のリストを指定すると、対応する従来の `allowed_prefixes`／`force_prefixes` より優先します。
pattern が空なら prefix のリストを使います。全 ref を拒否する場合は `allowed_prefixes: []` を指定します。
各ルールは UTF-8 文字列またはバイト値の配列で指定でき、未知の設定キーは起動エラーになります。

パスはコンポーネント境界で判定し、`app` は `app/config` を許可して `application/config` を許可しません。
空文字列のパス prefix は全パス、空のリストは変更可能なパスなしです。
新規 ref は空 tree、削除は新 tree が空として判定します。
commit／tree tag も検査し、パス制限中の blob tag は拒否します。
検査対象は旧 ref の tree と新 ref の tree の最終差分であり、merge の第二親経由の変更やモード変更も含みます。
force 許可でもパス ACL は迂回できません。
ネイティブ UpdateRef／Land も同じ最終 tree ACL を使用します。

## 再実行と現在の残作業

```sh
cargo test -p kasu-git --test pack_input --test receive --locked
cargo test -p kasu-server --test push --locked
docker build -f tools/git-compat/Dockerfile -t kasu:atomic-test .
go -C tools/git-compat build -o /tmp/kasu-go-git-compat .
KASU_GO_GIT_PUSH_CLIENT=/tmp/kasu-go-git-compat \
  cargo test -p kasu-server --test push --locked -- --include-ignored
```

`KASU_CAPTURE_DIR` を設定すると pkt-line を保存します。Git の時刻／PID と Go の sideband 本文を省き、
HTTP 認証ヘッダーを記録しません。
[互換性マトリクス](../crates/kasu-git/tests/fixtures/clients/matrix.json) に実測した版と fixture を記録しています。
clone した Git ディレクトリを `fsck --strict` し、サーバーの ref OID とネイティブ内容も比較します。
欠落オブジェクト、未公開 OID、古い ref、認証エラー、パケット／pack の任意バイト列も検査します。
atomic push は macOS の Git 2.54.0 と固定 Rust 1.90 の Linux コンテナで検証します。
branch／tag の混合成功、拒否時の全件・全ログ不変、ACL、保存直前に別 SQLite 接続が起こす
branch／tag の ABA 競合、非対応バックエンドの広告と拒否を含みます。
ストレージでは16個の独立接続による混合 CAS の単一勝者とログ一致を検査し、
最後の Git ログ書き込みに SQL エラーを注入して両種の ref／ログが rollback されることも検証します。

[ref_log からの永続 webhook](webhooks_ja.md) を実装しています。
[kind E2E](gitops-e2e_ja.md) で Image Updater v1.4.0／Flux Image Automation v1.2.5 の
実 commit・push、webhook による Deployment 更新、ネイティブ内容一致と strict fsck が通過しました。
M7 の受け入れ条件を検証済みです。夜間 CI は追加済みですが、GitHub 上での結果は未確認です。
