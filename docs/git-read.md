# Git Smart HTTP 読み取り

Metadata API と同じ `kasu-server` の HTTP ポートで Git を配信します。

```sh
RUSTC_WRAPPER= cargo run -p kasu-ingest -- /path/to/source.git /path/to/kasu-data my-repo
KASU_TOKEN=dev-token RUSTC_WRAPPER= cargo run -p kasu-server -- /path/to/kasu-data my-repo

git -c http.extraHeader='Authorization: Bearer dev-token' \
  ls-remote http://127.0.0.1:50080/my-repo.git
git clone --depth=1 http://git:dev-token@127.0.0.1:50080/my-repo.git
```

Basic のユーザー名は任意、パスワードは PAT として扱います。Bearer も使えます。
認証を設定した場合は ref 広告より前に検査し、Basic challenge を返します。運用者 PAT の認可は起動時に指定した repo 単位です。
[射影専用 PAT](projection-accounts.md) では、許可したディレクトリの射影だけを読み取れます。

## 対応する通信

- Smart HTTP の `info/refs?service=git-upload-pack` と `git-upload-pack`。
- protocol v0／v1 の ref 広告・ACK、v2 の独立した capability 広告・`ls-refs`・`fetch`。
- HEAD の symref、v2 の unborn、tag の peeled OID、ref-prefix。
- 通常 clone、depth=1、増分 fetch、deepen／unshallow、時刻と ref による shallow 境界。
- 複数 want、全親方向の履歴、途中の commit だけが参照する tree／blob、include-tag。
- 接続先 repo の公開 ref から到達可能な SHA 指定 want。明示公開した ref なし commit も取得可能。
- `filter blob:none` の partial clone、promisor の後続 blob fetch、sparse checkout、増分 fetch、unshallow。
- ディレクトリを root にする [パス射影 URL](git-projection.md)。読み取り専用、射影ごとの履歴と OID 索引。
- pkt-line の長さ・特殊マーカー・要求個数の検証。未知の capability と未対応の引数は拒否。
- gzip リクエスト。圧縮前後とも 16 MiB の要求サイズ上限。

`filter` は `blob:none` だけを提供し、`blob:limit`、`tree`、`combine` などの未対応フィルターは明示的に拒否します。
`no-done`、`sideband-all`、SSH、LFS はまだ広告・提供していません。
receive-pack の `atomic` は [書き込み](git-push.md) を参照してください。
receive-pack の対応範囲と書き込みポリシーは [Git 書き込み](git-push.md) を参照してください。

pack はネイティブストアから生成します。サーバー処理で `git upload-pack` を起動したり、Git リポジトリを別途保持したりしません。
非デルタの zlib pack を bounded channel でストリームし、クライアント切断時は書き込みを中断します。
圧縮済みオブジェクトの LRU は 32 MiB の課金容量上限を持ち、payload と 1 オブジェクトあたり 256 bytes の管理分を数えます。
キャッシュキーには Git OID と raw オブジェクトの BLAKE3 ID を含めます。ヒット数・ミス数・容量は
`Backend::git_cache_stats()` で取得できます。
pack 全体を保存するキャッシュはまだありません。

## 認可と pack の集合

OID 索引は repo でスコープを分けます。索引に存在するだけの未公開 Changeset や、他 repo の OID は want できません。
明示公開は、サーバー側の `GitIndex::publish_commit` で行い、commit とその全履歴・tree・blob を永続的に公開します。
これにより、ref なし commit の partial fetch 後にも省略した blob を取得できます。
HTTP から公開状態を書き換える機能はまだありません。
commit の到達可能性判定は Generation を使う全親 ancestry で検査します。

common commit からクライアントが確実に持つ集合を計算するときは、クライアントの既存 shallow 境界で親探索を止めます。
通常の全履歴 fetch は [永続到達索引](benchmarks/pack-reachability.md) を使い、親の集合と共有する radix tree を保存します。
索引がある部分の履歴・Git tree 本文を再走査せず、複数 want の到達集合から複数 common の集合を正確に引きます。
未導出の commit は全親から反復計算し、親の履歴に含まれる subtree の配下を再走査しません。
中間版や第二親だけに含まれる内容も保存するため、revert や merge の増分でも欠落しません。
shallow／deepen の要求は既存の境界付きグラフ列挙を使います。
shallow のマージ境界は全親を切るという Git の意味論に合わせます。
索引の大規模バックフィル・容量・複数ノード負荷と、shallow 列挙・ネゴシエーションの改善、大規模 repo の delta 評価は残作業です。

## Partial clone

```sh
git clone --filter=blob:none --no-checkout http://git:dev-token@127.0.0.1:50080/my-repo.git
cd my-repo
git sparse-checkout set deploy
git checkout main
```

初回は commit／tree／tag を配信し、blob は省略します。checkout や `git show` で必要になった blob は、
Git が promisor remote に OID を指定して取得します。`--depth=1` との併用、`fetch --unshallow` にも対応します。
v0／v1 は `filter` と `allow-reachable-sha1-in-want`、v2 は `fetch=shallow filter` を広告します。
明示的な want はフィルターや common commit による差し引きより優先します。
後続 fetch にも同じ repo 認可と公開範囲を適用し、索引に存在するだけの未公開 blob は取得できません。
ref の削除・force push 後に到達不能になったオブジェクトの保持・読み取り保証はまだありません。

Git 2.54.0／macOS arm64 と Git 2.39.5／固定 Rust 1.90 の Linux arm64 で、v0／v1／v2 の全履歴・shallow を検証しました。
初回の物理オブジェクト DB に blob がないこと、`transfer.fsckObjects=true` の clone、
過去の blob の単独取得、sparse checkout、unshallow、3回の増分 fetch、全 blob 取得後の到達 OID 一致と strict fsck が通過しています。
通信 fixture は [互換性マトリクス](../crates/kasu-git/tests/fixtures/clients/matrix.json)、
検証範囲・ソースと通信のハッシュは [結果](../crates/kasu-git/tests/fixtures/clients/partial-clone-summary.json) に保存しています。
大規模 repo での帯域・列挙性能評価と、他のフィルターは今後の対象です。

```sh
KASU_CAPTURE_DIR=/tmp/kasu-partial-traces RUSTC_WRAPPER= \
  cargo test -p kasu-server --test git_http partial_clone --locked
docker build -f tools/git-compat/Dockerfile -t kasu:partial-clone-test .
```

Docker の検証は固定 Rust 1.90 の Linux で Git の集合・parser、HTTP、push、複数 ref の保存契約を実行します。
外部 go-git バイナリが必要な ignored テストは、この Dockerfile では実行しません。

## 互換性の根拠

[バージョン付きマトリクス](../crates/kasu-git/tests/fixtures/clients/matrix.json) と同じディレクトリに通信 fixture を保存しています。

| クライアント | 検証済み |
|---|---|
| Git 2.54.0 / darwin arm64 | v0／v1／v2、ls-remote、全履歴／depth=1 bare clone、増分 fetch、deepen／unshallow、時刻／ref 境界、strict fsck |
| go-git v5.19.3 / Go 1.27.3 / linux arm64 | HTTP Basic、通常／depth=1 clone と checkout、symlink／実行権限／非 UTF-8 パス、取得した Git ディレクトリの strict fsck |
| ArgoCD v3.5.6 / Git 2.53.0 / kind linux arm64 | HTTP Basic、v2 fetch、初回同期と ref 更新後の再同期、Application Synced／Healthy、SHA と ConfigMap の一致 |
| Flux v2.9.6 / source-controller v1.9.6 / kind linux arm64 | HTTP Basic、v0 depth=1、初回同期と ref 更新後の再同期、GitRepository／Kustomization Ready、SHA と ConfigMap の一致 |

生成 pack のテストは `git index-pack --strict` と `git fsck --strict` を使用します。
HTTP E2E では clone/fetch 後の全オブジェクト集合と OID、shallow 履歴長も検査します。
中間 blob、第二親、shallow have、未公開 want、明示公開、無効な pkt-line、LRU 上限にもテストがあります。
分岐とマージを含む2ブランチ・annotated tag を8回更新し、v0／v1／v2 の全履歴／shallow クライアントで
毎回の複数 ref fetch、strict fsck、先端 OID を検証します。全履歴クライアントは全到達 OID も比較します。
[固定 Flux サンプルの pack サイズ比較](benchmarks/gitops-pack.md) では、同じ OID 集合で非デルタ／デルタを測定しています。

macOS は raw `0xff` ファイル名の checkout を拒否するため、同じ fixture を Linux コンテナ上で checkout しました。
Go クライアントは checkout 後の各ファイル内容、symlink、実行権限を元の tree と照合します。
ホストへ戻すのは `.git` と検証用の ASCII 名の成果物だけです。

通常の Rust テスト:

```sh
RUSTC_WRAPPER= cargo test --workspace --all-targets
```

Linux 上で go-git E2E を有効化:

```sh
go -C tools/git-compat build -o /tmp/kasu-go-git-compat .
KASU_GO_GIT_CLIENT=/tmp/kasu-go-git-compat RUSTC_WRAPPER= \
  cargo test -p kasu-server --test git_http -- --include-ignored
```

macOS arm64 から Linux で実行する場合（Docker、Python 3.12 以降が必要）:

```sh
GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go -C tools/git-compat build -o /tmp/kasu-go-git-compat-linux .
docker pull rust:1.90-bookworm
KASU_GO_GIT_CONTAINER=1 KASU_LINUX_GO_GIT_BINARY=/tmp/kasu-go-git-compat-linux \
  KASU_GO_GIT_CLIENT="$PWD/tools/git-compat/linux-client.py" RUSTC_WRAPPER= \
  cargo test -p kasu-server --test git_http -- --include-ignored
```

`KASU_CAPTURE_DIR` を設定すると実行したクライアントの通信を保存します。Go の sideband 内容は省略し、Git の時刻／PID は除きます。
夜間 CI `git-read.yml` に Linux での直接実行を追加しました。GitHub 上の実行結果は未確認です。

2026-10-23 に kind v0.33.0／Kubernetes v1.37.0 で両コントローラーの同期を検証しました。
[手順・夜間 CI](gitops-e2e.md) と [結果・実イメージ ID](../crates/kasu-git/tests/fixtures/clients/gitops-kind-summary.json) を保存しています。
Image Updater／Image Automation の書き戻しと webhook は M7 を参照してください。
指定ディレクトリだけを配信する [パス射影](git-projection.md) も提供しています。

プロトコルの一次資料: [Smart HTTP](https://git-scm.com/docs/http-protocol)、
[pack protocol](https://git-scm.com/docs/pack-protocol)、[protocol v2](https://git-scm.com/docs/protocol-v2)。
partial clone の設計は [promisor と後続 fetch](https://git-scm.com/docs/partial-clone) を参照しています。
