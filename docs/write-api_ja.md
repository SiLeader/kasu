# ネイティブ Write API

[English](write-api.md) | 日本語

`kasu-server` は Metadata と同じ gRPC ポートで `kasu.v1.Writer` を提供します。
HTTP JSON も同じ HTTP ポートで利用できます。
`KASU_TOKEN` を指定した場合は Bearer または Basic のトークンが必要です。
認可対象は起動時に指定した1 repo です。

| RPC | HTTP POST | 動作 |
|---|---|---|
| `UploadBlobs(stream BlobUpload)` | `/v1/upload-blobs` | raw blob の分割アップロード、既存 blob の省略 |
| `CreateChangeset` | `/v1/create-changeset` | 親と blob の所属、変更パス、内容を検証して作成 |
| `Merge` | `/v1/merge` | 全 best merge-base を使う3-way merge |
| `Rebase` | `/v1/rebase` | 1 Changeset の第一親との差分を onto へ再適用 |
| `UpdateRef` | `/v1/update-ref` | ref ポリシーと fast-forward を検証して CAS |
| `Land` | `/v1/land` | 永続的な冪等キー、上限・バックオフ・キャンセルを持つ rebase + CAS |

スキーマは [`kasu.proto`](../crates/kasu-proto/proto/kasu.proto) です。
既存 Metadata API と同じく、バイト列は protobuf の `bytes`、HTTP JSON では数値配列です。
author／committer／message、パス、競合内容を UTF-8 へ変換しません。
`CreateChangeset` の変更はパス順・一意である必要があります。
Git provenance をクライアントから指定するフィールドは提供しません。

## blob のアップロード

`BlobUpload` の `repo`、期待する raw BLAKE3 `id`、`offset`、`data`、`last` を送ります。
同じ blob のチャンクを連続して送り、offset は0から送信済みデータ長ずつ進めます。
最終チャンクでハッシュを検証して保存します。空 blob は空 data・offset=0・last=true です。
途中終了、offset 不一致、ID の切り替え、ハッシュ不一致はエラーです。
既に完了した blob は、後の blob に失敗しても不変オブジェクトとして残ります。

同じ repo で保存済みの blob は、空 data・offset=0・last=true で存在確認して送信を省けます。
結果の `BlobReceipt` は ID、実サイズ、`existing` を返します。
別 repo にだけ存在する blob は省略できず、内容をアップロードしてハッシュを証明する必要があります。
親 Changeset と各 Set の blob は repo 所属を検査します。
Git 取り込みも blob の所属を記録します。以前の DB で所属記録がない blob は、再取り込みまたはアップロードしてください。

gRPC は1メッセージ16 MiB、1 blob 256 MiB、1ストリームの送信データ512 MiB、
最大4,096 blob／65,536チャンクです。サーバーは同時に1 blob を組み立てます。
HTTP では `{ "chunks": [...] }` を送り、JSON リクエスト全体の上限は16 MiBです。

## Merge／Rebase と競合

`RewriteResponse.id` があれば成功です。fast-forward の Merge は既存 ID を返します。
競合時は ID がなく、`stage`、`bases`、`conflicts` に基底または最終 tree の競合を返します。
競合エントリーには path、種類、base／ours／theirs、行単位の範囲と内容があります。
成功した Merge／Rebase も ref を移動しません。
[マージの意味論とテスト](native-merge_ja.md) を参照してください。

## ref の更新

`UpdateRefRequest` には name、必須の expected Ref（name・target・version）、新 target を指定します。
新規 ref は target なし・version=0 が初期状態です。
CAS 不一致はエラーではなく、`applied=false` と実際の状態を返します。
削除後も version は単調増加し、削除・再作成による ABA を拒否します。
成功した CAS と ref_log の追記は同じトランザクションです。

デフォルトは `refs/heads/` の作成と fast-forward のみです。
force と削除は無効です。サーバー組み込み時の `Backend::with_write_policy` で、
書き込み可能な接頭辞、force 可能な接頭辞、作成／削除の可否を設定できます。
非 fast-forward は、リクエストの `force=true` とサーバー側の force 許可の両方が必要です。
CLI は `KASU_WRITE_POLICY_FILE` で JSON 設定を読み込めます。
ref の `*`／`?` パターン、バイト列パスの境界、Git と共通の最終 tree ACL は
[Git 書き込み](git-push_ja.md) を参照してください。

Git branch の可変状態はネイティブ ref のみです。
CAS の前に不変 Git commit 射影を準備し、Git に表現できない署名などを拒否します。
CAS 後の Git 広告は更新したネイティブ ref を参照します。

検証は gRPC ストリームと HTTP JSON、認証・repo 分離、CAS／ABA／force／削除ポリシー、
API で作成した履歴の Git clone と `fsck --strict` を含みます。

```sh
cargo test -p kasu-server --test write --locked
```

Land の要求・確定結果・再送・キャンセルは [Land](land_ja.md) を参照してください。
