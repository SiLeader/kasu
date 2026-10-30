# Metadata API

[English](metadata-api.md) | 日本語

既存の Git リポジトリを取り込み、チェックアウトせずに問い合わせられます。
ビルドには Rust stable、C コンパイラ、`protoc` が必要です。

```sh
cargo run -p kasu-ingest -- /path/to/repo.git /path/to/kasu-data my-repo
KASU_TOKEN=dev-token cargo run -p kasu-server -- /path/to/kasu-data my-repo
```

既定の待ち受けは gRPC `127.0.0.1:50051`、HTTP `127.0.0.1:50080` です。
任意のアドレスを第 3、第 4 引数に指定できます。`RUST_LOG` でログレベルを指定します。
`KASU_TOKEN` が設定されている場合、両トランスポートで `Authorization: Bearer <token>` を要求します。
未設定時は開発用の認証なし構成です。現在の認可単位は起動時に指定した 1 リポジトリです。

## gRPC

正となる定義は [kasu.proto](../crates/kasu-proto/proto/kasu.proto) の `kasu.v1.Metadata` です。
Rust クライアントは `kasu_proto::v1::metadata_client::MetadataClient` で接続できます。
ID は BLAKE3 の 32 バイト、Git OID は SHA-1 の 20 バイトです。パス・署名・メッセージは bytes のまま扱います。
同じ gRPC ポートの `kasu.v1.Writer` による作成・マージ・ref 更新は [Write API](write-api_ja.md) を参照してください。

| RPC | 動作 |
|---|---|
| `GetChangeset` / `GetRootTree` | Changeset 本体／ルート Merkle tree ID |
| `ResolveRef` / `ListRefs` | ネイティブ ref の target と CAS version。取り込んだ branch 名は `refs/heads/main` 等 |
| `Log` | 第一親ログ。path 指定時はそのパス／ディレクトリが第一親との差分で変わった Changeset だけを返す |
| `IsAncestor` / `MergeBase` | 全親を対象に祖先判定／すべての best common ancestor を返す |
| `Diff` | パス単位の before／after Entry。content=true で行単位の変更範囲も返す |
| `Blame` | 各行の由来 Changeset、パス、元の 1-based 行番号。全親から変更されていない行を引き継ぐ |
| `ReadFile` | File／Symlink の Entry と内容。gitlink の内容は取得しない |
| `ListDir` | 直下の entry。空パスはルート。子 tree は ID で返す |
| `FindChangesetByGitOid` | このリポジトリの Git OID 索引で commit を逆引き。見つからない場合は id なし |
| `FindChangesetsByPrefix` | 16 進 ID が `prefix`（4〜64 桁）で始まるこのリポジトリの Changeset（最大 10 件。2 件以上なら曖昧）。短縮 ID に使う |

`Log.limit` は 1〜1000 です。次ページには同じ start／path と返された cursor を渡します。
cursor は第一親チェーンと path の変更点にも照合します。ref が動いても ID を指定したページング結果は変わりません。
終了時の cursor は空です。path の追跡は名前に基づきます。

Diff の paths は各パス自身とその配下を選びます。内容の変更範囲の start は 0-based、before／after は改行を
含むバイト列のリストです。改行なしの末尾も保持します。NUL を含む内容は binary=true とし、行 diff は返しません。
ファイルの mode のみが変わった場合は Entry の差分を返し、内容の変更範囲は空です。

PathHistory と Blame はパスを SubKey とする Lazy Derived Data です。
Blame は rename 元の親パスも必要に応じて計算します。`PathRequest.follow_moves=true` なら同一ファイル内の行移動を追跡し、
`follow_copies=true` なら移動と別ファイルからの全体／断片 copy を追跡します。両方とも省略時は false です。
[CopyDetect／PathIndex と行追跡](blame-lineage_ja.md) に生成規則、Git 比較、対応範囲を記録しています。

## HTTP/JSON

同じメッセージを JSON として POST します。bytes は **0〜255 の整数配列**、oneof は kind オブジェクトです。
フィールド名は protobuf と同じ snake_case です。省略したフィールドには既定値を使います。

| パス | リクエスト型 |
|---|---|
| `/v1/changeset` / `/v1/root-tree` | ChangesetRequest |
| `/v1/resolve-ref` | RefRequest |
| `/v1/refs` | ListRefsRequest |
| `/v1/log` | LogRequest |
| `/v1/is-ancestor` / `/v1/merge-base` | CompareRequest |
| `/v1/diff` | DiffRequest |
| `/v1/blame` / `/v1/read-file` / `/v1/list-dir` | PathRequest |
| `/v1/find-git-oid` | GitOidRequest |
| `/v1/find-prefix` | PrefixRequest |

例: `my-repo` の ref 一覧。

```sh
curl -s http://127.0.0.1:50080/v1/refs \
  -H 'Authorization: Bearer dev-token' \
  -H 'Content-Type: application/json' \
  --data '{"repo":[109,121,45,114,101,112,111],"prefix":[]}'
```

入力値の不正は 400、未認証は 401、別リポジトリは 403、見つからない Changeset／ファイルは 404 を返します。
ストレージ内部のエラー詳細はクライアントへ返さず、tracing に記録します。
HTTP リクエストと gRPC の受信メッセージ上限は 16 MiB、gRPC 応答上限は 256 MiB です。

## リポジトリ分離と検証

Changeset はリポジトリ所属の不変レコードと照合します。別リポジトリの ID が同じストアに保存されていても、
このリポジトリの問い合わせには使えません。取り込み時に所属を記録し、再取り込みでも補完します。

テストでは生成した Git 履歴で `git log --first-parent -- <path>`、`git blame --line-porcelain`、
`git merge-base --all`、`git diff --no-renames --unified=0` と比較します。
削除・再追加・mode 変更・繰り返し行・二親マージ・criss-cross を含みます。
[rename／copy と行断片](blame-lineage_ja.md) は、純粋／編集 rename・ファイル全体／行断片の copy・行移動とその後の編集を3モード・24通りで比較しています。
[固定した実在 OSS の Metadata 検証](metadata-oss_ja.md) では、全21 commit のパス履歴189件・Blame 77件・merge-base 441組が macOS／Linux で一致しました。
Git の全 rename／copy ヒューリスティクスや任意の OSS 全般の parity は残作業です。
gRPC 実通信、HTTP JSON、サーバー実行ファイルと永続ストア、認証・所属・cursor の検証も行っています。

[Git Smart HTTP の読み取り](git-read_ja.md) は同じ HTTP ポートで提供します。
[Write API](write-api_ja.md)、[Git 書き込み](git-push_ja.md)、[Object RPC](object-api_ja.md)、[射影の読み取り ACL](projection-accounts_ja.md) も実装しています。
[native のディレクトリ ACL](directory-read-acl_ja.md)、[telemetry](telemetry_ja.md)、[権威サーバーの負荷制限](server-runtime_ja.md) も実装しています。

`GetBlobMetadata(ObjectRequest)` と `/v1/blob-metadata` は、同一 repo に属する最大128個の blob ID について、
サイズと Git blob OID を返します。本文の読み取りは行いません。[仮想 checkout](virtual-checkout_ja.md) の属性問い合わせで利用します。

## ディレクトリ別の読み取りビュー

[read_directories](directory-read-acl_ja.md) を持つ account の GetChangeset／Log は許可パスの changes だけを返し、
raw Git provenance を省略して `read_filtered=true` とします。id は元 Changeset の参照です。
この応答から元 Changeset の canonical bytes は再構成できません。
GetRootTree は許可範囲だけで作った canonical TreeId と `read_filtered=true` を返します。
ListDir／ReadFile／Diff／Blame／GetBlobMetadata にも同じ制限を適用します。
元の Changeset／Tree の保存形式・ID は変更しません。無制限の account の flag は false です。
