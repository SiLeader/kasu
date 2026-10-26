# Object RPC

`kasu.v1.Objects` は Metadata／Writer と同じ gRPC ポートに登録します。
`GetTrees(ObjectRequest) -> stream TreeObject` と
`GetBlobs(ObjectRequest) -> stream BlobChunk` を提供します。
`ObjectRequest` は `repo` と取得する32バイト ID の `ids` を持ちます。
認証は Metadata と共通の Basic／Bearer です。

各チャンクは `id`、`offset`、`data`、`last` を持ちます。
`data` はストアに保存した zstd frame の一部分で、最大64 KiBです。
`offset` は圧縮済みバイト列内の位置です。同じオブジェクトのチャンクは連続し、
最後のチャンクだけ `last: true` になります。要求順に配信します。
Tree も大きい場合はチャンク化します。空のファイルも zstd frame として配信します。

クライアントはチャンクの ID／offset／終端を検査し、完全な frame を得てから
展開上限、内容の BLAKE3、Tree の canonical codec を検証してください。
成功した frame は変換せずディスクキャッシュに保存できます。
サーバーはこの配信経路で再圧縮しません。

## 所属と取得順序

`ResolveRef` → `GetRootTree` → 必要な子 Tree → 必要な Blob の順で取得します。
`GetRootTree` は認可済み Changeset から Tree の repo 所属索引を登録します。
この索引は親を最後に登録し、検証済みの共有 subtree を再走査しません。
初回は全 Tree を検証し、以後は新しい Merkle node だけを処理します。
Blob 本文は所属索引作成時に読みません。索引は SQLite 再オープン後も保持します。
`ListDir` でも所属を登録し、Merge／Rebase／Land の競合応答で公開する Tree／Blob も登録します。

Object RPC 自体はパスやスパース設定を受け取りません。
クライアントは root とプロファイルに必要な子 Tree だけをたどり、対象 Blob だけを要求します。
まだ `GetRootTree` 等で所属登録していない Tree は、ID が既知でも取得できません。
別 repo の ID を混ぜたバッチは、本文の送信前に拒否します。

## 上限と停止

1要求は最大128 ID、protobuf 本文は最大16 KiBで、重複 ID を拒否します。
空バッチは空ストリームです。
展開済みオブジェクトの既存上限は256 MiB、圧縮 frame の配信上限は257 MiBです。
バッチ全体をメモリに読み込まず、一度に1オブジェクトと最大2チャンクの送信キューを保持します。
受信側が停止すると backpressure がかかり、切断後は次のオブジェクトを読みません。
配信途中のエラーでは、受信済みの完全なオブジェクトは再利用できますが、
終端のない部分オブジェクトは利用できません。

```sh
RUSTC_WRAPPER= cargo test -p kasu-server --test objects --locked
RUSTC_WRAPPER= cargo test -p kasu-store --test tree_membership --locked
```

実 gRPC でのスパース要求、保存 frame とのバイト一致、チャンク境界、認可・バッチ上限、
切断、ストア破損／欠落、競合 Tree、再オープンと共有 subtree の省略を検証します。
[working copy／CLI／notify](native-cli.md) の実通信 E2E も追加しました。
共有 daemon／cache 階層は M9 の残作業です。

## ディレクトリ読み取り ACL

[read_directories](directory-read-acl.md) のある account は、GetRootTree で取得した canonical な読み取りビューをたどります。
Objects と GetBlobMetadata は、その repo／読み取りパス集合の索引に登録された ID だけを認可します。
repo 全体の所属索引や別の scope の索引を流用しません。
`KASU_OBJECT_TOKEN_FILE` が有効でも、範囲のある account は自身の PAT で server に直接接続でき、scope の検査を受けます。
無制限の PAT で hop token を代替することは拒否します。利用者の scope を保つ共有 cache の委譲は未実装です。
