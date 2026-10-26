# Git pack の永続到達索引

通常の全履歴 fetch に、Git OID の到達集合を保存する不変 radix tree を追加しました。
公開する capability、Git オブジェクトの内容／OID、非デルタ pack の形式は変わりません。
浅い履歴・deepen・時刻／ref 境界を持つ要求は既存の境界付き列挙を使います。

## 計算と保存

`git/reachability/v1/<projection>/<git-oid>` は到達集合の root を指します。
ノードは `git/reachability-node/v1/<BLAKE3>` に put-if-absent で保存します。
OID の hex digit を使う16分岐、64 OID 以下のソート済み leaf、最大40段の木です。
同じ集合は挿入順や親の順に依存せず同じ root になります。

commit の全親を反復的に導出し、親の到達集合を統合します。
新しい root tree をたどるとき、親の履歴にすでに含まれる subtree は配下を省略します。
現在の snapshot から削除された古い blob も集合に残すため、中間版・revert・第二親の内容を扱えます。
blob の内容は列挙のために読みません。Gitlink の参照先は含めません。
初回の導出は未処理の履歴と変更 tree を読みます。

fetch では複数 want と common の保存済み root を比較し、同じノードの部分を省略します。
client が指定した have の組合せごとの一時 union は永続化しません。
要求内のノード LRU は1,024個に制限し、commit DAG は再帰しません。
索引の root は依存ノードの保存後に公開し、途中で停止しても再計算できます。
repo／パス射影ごとに root を分離し、既存の want／have 認可後に使います。

## 実測

2026-10-24（JST）、macOS arm64／Rust 1.98.1／Git 2.54.0、release ビルド、
SQLite ファイル DB／FS blob store で、[初回計測](gitops-pack.md) と同じ
Flux サンプルの固定 SHA `4f1a5b1051b469108d1bdf0ebec6c814427e3834` を使いました。
124 commit／588 object です。

| 要求 | object 数 | 最初の列挙 | 同じ要求の再列挙 | kasu pack |
|---|---:|---:|---:|---:|
| 全履歴 | 588 | 167.665 ms | 0.097 ms | 735,303 B |
| depth=1 | 46 | 3.343 ms | 0.757 ms | 26,158 B |
| 1 commit 増分 | 11 | 0.266 ms | 0.157 ms | 15,140 B |
| 10 commit 増分 | 123 | 0.507 ms | 0.439 ms | 77,308 B |

ケースは上から順番に実行します。全履歴の初回列挙は索引の構築を含み、
増分ケースはその索引を再利用しています。depth=1 は到達索引を使いません。
再列挙でも要求内の LRU は新しく作りますが、SQLite／OS のキャッシュは温まっています。
初回計測の1 commit 増分は約76 msでした。実行日時やキャッシュ条件も異なるため、
この差だけから本番性能や厳密な速度比は判断しません。

[CSV](gitops-pack-reachability.csv) に全 SHA、圧縮・列挙時間を保存しました。
全履歴／増分の OID 集合は実 Git の `rev-list --objects` と一致し、全 pack が
`git index-pack --strict --stdin` を通過しました。kasu pack の object 数とサイズは初回計測と同じです。
これは小さな1ソース・各条件1回の計測です。索引の大規模バックフィル、保存容量、
多 cache／多 server の負荷、shallow 列挙は今後の評価・改善対象です。

## 正しさと読み取り量の検証

- radix tree の union／差分／所属判定を順序付き集合と比較する proptest。長い共通 OID prefix と複数 common も検査。
- ランダムな分岐・merge・同一 blob の再利用を含む DAG で、複数 want／have と `blob:none` を全走査と比較。
- 64ディレクトリ・512ファイル・129 commit の履歴を導出後、1件の変更に必要な本文読み取りが
  新 commit と変更した root／directory tree の3回だけであることを検査。
  別の SQLite 接続と新しい索引インスタンスでは、同じ列挙の本文読み取りが0回。
- v0／v1／v2 の通常／shallow／partial／パス射影、8回連続の分岐・merge・複数 ref fetch、
  push、各 pack／repository の strict 検証を既存 E2E で再実行。

macOS の [Git テスト](pack-reachability-git.log) と [実 HTTP テスト](pack-reachability-http.log)、
固定 Rust 1.90／Git 2.39.5／Linux arm64 の [Docker テストとソース hash](pack-reachability-linux.log) を保存しています。
固定 Go クライアントを外部指定する ignored テストはこの実行には含みません。

```sh
cargo test -p kasu-git --locked
cargo test -p kasu-server --test git_http --test push --locked
cargo clippy -p kasu-git -p kasu-ingest --all-targets --locked -- -D warnings
docker build -f tools/git-compat/Dockerfile -t kasu:reachability-test .
```

サイズ計測の再現は [固定ソースの手順](gitops-pack.md) を使います。
