# GitOps リポジトリの pack サイズ

2026-10-23 に [fluxcd/flux2-kustomize-helm-example](https://github.com/fluxcd/flux2-kustomize-helm-example/tree/4f1a5b1051b469108d1bdf0ebec6c814427e3834)
の `4f1a5b1051b469108d1bdf0ebec6c814427e3834` を固定して測定しました。
124 commit／588 object の小さなサンプルです。macOS arm64、Rust 1.98.1、Git 2.54.0、release ビルド、
SQLite ファイル DB と FS blob store を使用しました。

| 要求 | object 数 | kasu 非デルタ | Git 非デルタ | Git デルタ |
|---|---:|---:|---:|---:|
| 全履歴 | 588 | 735,303 B | 702,658 B | 526,220 B |
| depth=1 | 46 | 26,158 B | 24,096 B | 22,128 B |
| 1 commit 増分 | 11 | 15,140 B | 13,837 B | 13,837 B |
| 10 commit 増分 | 123 | 77,308 B | 71,948 B | 41,829 B |

[CSV](gitops-pack-flux.csv) に固定 SHA、have、列挙／cold・warm 圧縮時間も保存しています。
pack 本文のバイト数であり、HTTP／TLS／pkt-line のオーバーヘッドは含みません。
増分は第一親の 1 件／10 件前を、全履歴を持つクライアントの have とします。

`plan_fetch` が選んだ**同一の OID 集合**を kasu と Git で pack 化しました。
kasu は既存の非デルタ・fast zlib、Git は `pack.compression=1`、再利用なしで、
非デルタは `--window=0`、デルタは `--window=10 --depth=50` です。
圧縮実装と並び順も異なるため、Git 非デルタとの差にはそれらの影響を含みます。
外部の have object をベースとする thin pack は測定していません。
全 kasu pack は元の完全リポジトリを使った `git index-pack --strict --stdin` を通過しました。

このサンプルの1 commit 増分は Git のデルタ有無で同じサイズでした。
10 commit 増分では Git デルタは kasu より約46%小さく、全履歴でも約28%小さくなりました。
現時点では depth=1 と頻繁な単発更新の配信に非デルタを継続使用します。
多数の更新をまとめる利用や大規模リポジトリでは、デルタ導入の効果を追加評価します。
小さな1リポジトリ・各条件1回の結果から、大規模運用での不要性は判断しません。

列挙は全履歴で約121 ms、1 commit 増分でも約76 msでした。
この初回計測の実装では have の到達集合を再列挙するため、増分の列挙時間が履歴サイズに依存していました。
2026-10-24 に通常 fetch の永続到達索引を追加し、[同じ固定ソースで再計測](pack-reachability.md) しました。
shallow 列挙と大規模なバックフィル・容量評価は引き続き必要です。cold／warm 時間は圧縮キャッシュの差を含み、
各ケースでキャッシュを空から作っています。ネットワーク転送速度のベンチマークではありません。

再現手順（取得先はこの計測専用の一時リポジトリを指定）:

```sh
git clone --bare --single-branch --branch main \
  https://github.com/fluxcd/flux2-kustomize-helm-example.git /tmp/kasu-flux-bandwidth.git
RUSTC_WRAPPER= cargo run --release --locked -p kasu-ingest --example pack_bandwidth -- \
  /tmp/kasu-flux-bandwidth.git 4f1a5b1051b469108d1bdf0ebec6c814427e3834
```

計測用 native store は一時ディレクトリへ作成し終了時に削除します。
strict 検証は入力の Git リポジトリに pack を追加するので、通常作業用のリポジトリは指定しないでください。
