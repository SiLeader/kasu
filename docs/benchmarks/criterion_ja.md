# Criterion ベンチマーク

[English](criterion.md) | 日本語

計画05のテスト戦略に対応し、Changeset 作成、Derived Data 計算、pack 生成の throughput とサイズを測定します。
[Criterion 0.7.0](https://docs.rs/criterion/0.7.0/criterion/) の20サンプル・95%信頼区間を使い、raw sample／推定値／outlier 境界も保存します。
2026-10-26 に macOS arm64／Rust 1.98.1 と、digest 固定 Rust 1.90／Linux arm64 で全11ケースを実測しました。

| ケース | macOS 平均 µs | Linux 平均 µs |
|---|---:|---:|
| `changeset_create/100` | 185.892 | 89.281 |
| `changeset_create/1000` | 184.978 | 93.254 |
| `changeset_create/10000` | 187.966 | 100.569 |
| `pack_encode/cold/262144` | 5,304.184 | 3,907.389 |
| `pack_encode/cold/4096` | 3,120.932 | 1,657.885 |
| `pack_encode/warm/262144` | 485.006 | 498.287 |
| `pack_encode/warm/4096` | 132.266 | 134.557 |
| `path_history/cold/100` | 14,286.035 | 4,042.626 |
| `path_history/cold/1000` | 120,877.387 | 39,528.348 |
| `path_history/warm/100` | 2.643 | 2.771 |
| `path_history/warm/1000` | 2.668 | 2.673 |

## 入力と計測範囲

- `changeset_create/100,1000,10000`: 第一親深さを変え、10世代ごとに第二親を持つ履歴を準備します。同じ tip を親とする新しい子を毎回作成し、1ファイルの内容を変更します。message が一意なので既存 Changeset の再送にはなりません。Changeset 構築は計測外、親・blob・tree の検証と eager 計算、不変レコード保存は計測内です。
- `path_history/cold/100,1000`: 各 iteration に新しいストアを準備し、未保存の PathHistory を最後まで計算します。fixture の作成・破棄は計測外です。cold は派生レコードが未保存という意味で、OS の page cache を破棄する測定ではありません。
- `path_history/warm/100,1000`: 保存済みの同じ結果を返す問い合わせです。cold の throughput 単位は履歴内 commit 数、warm は1問い合わせです。
- `pack_encode/cold,warm/4096,262144`: 固定 seed の4 KiB／256 KiB blob を2種類持つ101 commit・109 object です。cold は圧縮キャッシュなし、warm は全 object を事前圧縮済みにします。OID 列挙と Git 検証は計測外、索引読み取り・必要な本文取得／圧縮・pack checksum と stream 書き込みは計測内です。writer は実バイト列を black_box で消費し、全 pack を蓄積する allocation を行いません。

全ケースは SQLite memory store と実 FS／zstd BlobStore を使います。PostgreSQL／S3／network の性能ではありません。
Changeset の計測中には保存レコードが増えます。履歴長に対する参照回数の検証は [10万件の FirstParentSkip v2 計測](first-parent-skip-v2_ja.md) が対象です。
cold Derived の fixture 準備を含む実時間は、Criterion の計測時間設定より長くなります。
pack の throughput は選択した Git object の非圧縮本文 bytes／秒です。ネットワークの転送量は次表の pack bytes を使います。

| blob bytes | commit | object | raw bytes | pack bytes |
|---:|---:|---:|---:|---:|
| 4,096 | 101 | 109 | 38,099 | 28,605 |
| 262,144 | 101 | 109 | 554,195 | 545,088 |

cold／warm の pack 本文が完全一致し、Git の `index-pack --strict` と ref 作成後の `fsck --strict` が通過してから計測します。
[pack-sizes.csv](criterion/pack-sizes.csv) は両環境で一致しました。この固定した小さな合成入力から本番の帯域・遅延 SLA は判断しません。

## 再実行

```sh
KASU_CRITERION_OUTPUT=/tmp/kasu-criterion-run CRITERION_HOME=/tmp/kasu-criterion-run/criterion \
  cargo bench -p kasu-ingest --bench throughput --locked
```

Criterion JSON は `CRITERION_HOME`、pack サイズ CSV は `KASU_CRITERION_OUTPUT` に保存します。
独立した実行には新しい出力 directory を指定します。既存出力を使う場合は Criterion が baseline と比較します。
`-- --test` を付けると各ケースを1回実行するスモーク試験になります。
`cargo test --workspace --all-targets --all-features --locked` でも計測ループを回さず、この11ケースを検査します。

```sh
docker build -f tools/criterion/Dockerfile -t kasu:criterion .
mkdir -p /tmp/kasu-criterion-linux
docker run --rm --network none --user "$(id -u):$(id -g)" \
  -e KASU_CRITERION_OUTPUT=/reports \
  --mount type=bind,source=/tmp/kasu-criterion-linux,target=/reports \
  kasu:criterion
```

[summary.json](criterion/summary.json) にソース hash、[macOS](criterion/macos.json)／[Linux](criterion/linux.json) に全 sample と推定値、
[実行ログ](criterion/macos.log)・[Linux ログ](criterion/linux.log)・[ビルド情報](criterion/linux-build.log) を保存しています。
`tools/criterion/record.py` は20個の sample から平均を再計算し、全11ケース、信頼区間、strict pack 結果と両環境の source hash を照合します。
全 target／全 feature の workspace 試験212件とベンチのスモーク11件が通過しました。
外部サービスや固定入力を要求する21件は通常実行では ignored です。全 target／全 feature の clippy も通過しています。
[夜間 CI](../../.github/workflows/criterion.yml) は同じ固定 Linux 環境で測定し、JSON／CSV／ログを artifact に保存します。GitHub 上の実行結果は未確認です。
