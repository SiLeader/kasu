# Changeset 作成の履歴長ベンチマーク

[English](README.md) | 日本語

現在の eager index は [FirstParentSkip v2](first-parent-skip-v2_ja.md) です。
作成時の index 構築は7参照以下・82 bytes に固定し、両 OS で10万件を検証しました。
以下の CSV と対数コストの説明は、移行前の v1 の計測記録です。

[実際の分岐合流10万件とストア呼び出し回数](divergent-history_ja.md) の追加計測では、
macOS／Linux の内容 ID 一致と MergeBase の古い祖先走査の削減を検証しています。

ストレージ候補の比較と障害試験は [TiKV／PostgreSQL 初期評価](kv-evaluation_ja.md) を参照してください。

`history-100k.csv` は 2026-10-23 にローカル macOS（aarch64、Rust 1.98.1）で実測した結果です。
SQLite はファイル DB、WAL／synchronous=FULL、blob と tree はローカル FS／zstd です。

```sh
KASU_BENCH_HISTORY=1 cargo bench -p kasu-ops --bench history
```

- 100,000 Changeset を順に作成し、各 Changeset は `bench/file` を 2 種類の内容間で更新します。
- 毎 10 件目（ルート以外）に第二親を加え、最終的に 9,999 個の 2-parent Changeset を作成します。
  第二親は既存の第一親チェーン上の祖先です。分岐と criss-cross の正しさは別途プロパティ／単体テストで検証します。
- 親と blob の存在検証、Merkle tree の更新、Changeset 保存、RootTree／Generation／FirstParentSkip の
  eager 計算を含みます。ref 更新・ネットワーク・Git オブジェクト変換は含みません。
- `mean_create_microseconds` は直前 1,000 件の作成時間を 1 件あたりの µs に換算した値です。

| 履歴内の区間 | 平均作成時間／Changeset |
|---|---:|
| 最初の 10,000 件 | 411.42 µs |
| 45,001〜55,000 件 | 669.51 µs |
| 最後の 10,000 件 | 580.07 µs |

履歴が 100 倍になる区間でも、履歴全体の再走査に相当する線形増加は観測されませんでした。
実装上、tree 更新は変更パス上のノードに限定し、Generation は保存済みの親を読みます。
FirstParentSkip の構築には O(log 履歴長) の索引読み取り、SQLite の索引にもサイズ依存のコストがあります。
したがって厳密な定数時間を証明する結果ではありません。

これは 1 台・1 回・同時に他テストも走った測定であり、本番スループットや分散ストレージの性能を
保証しません。幅の広い tree、多数の変更パス、実際の分岐、複数 server の性能評価は今後の作業です。
