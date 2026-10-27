# 分岐と合流を含む10万 Changeset の計測

[English](divergent-history.md) | 日本語

このページは FirstParentSkip v1 時点の計測です。現在は [v2 の固定参照構造](first-parent-skip-v2_ja.md) に
変更し、同じ履歴・内容 ID での再計測を追加しています。以下の CSV は比較用に保持します。

M1 の合成履歴を、実際に分岐する第二親へ拡張しました。
macOS arm64（Rust 1.98.1）と固定 Rust 1.90.0 の Linux arm64 で、
100,000 Changeset と10,000個の分岐合流 Changeset を作成しました。
両 OS の最終 Changeset ID と Tree ID は一致しています。

[既存の計測](README_ja.md) は第二親が第一親チェーン上の祖先でした。
今回は双方に異なる tree 変更を持つ tip を合流させます。
また、確認中に見つけた MergeBase の不要な祖先走査も修正しました。

## ワークロードと計測範囲

10 Changeset を1周期にして、8個の main（最初の周期は root を含む）、
1個の side、1個の2-parent merge を作成します。
side は main の3世代前から分岐し、`bench/side` を変更します。
main は `bench/main` を更新します。merge は main を第一親、side を第二親にし、
side の変更を第一親との差分として記録します。
各 merge で、tip 同士が祖先でないこと、best merge-base が分岐点であること、
第二親が merge の祖先であること、両方のファイル内容を保持することを確認します。

計測対象は `Operations::create_changeset` の呼び出しです。
親・blob の検証、Merkle tree の更新、Changeset の保存、RootTree／Generation／FirstParentSkip の
eager 計算を含みます。入力構築、各種の結果照合、CSV 出力は時間とストア呼び出し回数から除外します。
3-way merge 自体、ref CAS、ネットワーク、Git 変換は計測しません。

ストアはファイル SQLite（WAL／synchronous=FULL）とローカル FS／zstd です。
Tree は固定幅、blob 本文は2種類を再利用します。
ストアを包むカウンターで MetaStore の get／put-if-absent と BlobStore の
get-compressed／put-compressed 呼び出しを数えます。
内部 SQL 文や OS の物理 I/O、実際に新規保存したオブジェクト数とは異なります。
カウンターの atomic increment は計測時間に含まれます。

## 2026-10-24 の結果

各 CSV 行は直前1,000件の平均です。下表はそれぞれ10行、10,000件分の平均です。

| 区間 | macOS 作成時間／件 | Linux 作成時間／件 | MetaStore get／件（両 OS） | 区間の最大 get／件 |
|---|---:|---:|---:|---:|
| 1〜10,000 | 416.73 µs | 863.42 µs | 35.479 | 40 |
| 45,001〜55,000 | 531.22 µs | 790.30 µs | 39.300 | 42 |
| 90,001〜100,000 | 575.45 µs | 836.84 µs | 40.300 | 43 |

各行の MetaStore put は平均4.00回、BlobStore get／put は平均3.00回／2.00回でした。
MetaStore get の全件最大は43回です。
全履歴の線形走査に相当する呼び出し回数の増加は見られませんでした。
一方、読み取りは履歴長とともに増えています。
[FirstParentSkip の構築](../../crates/kasu-derived/src/types.rs:83) は各距離の祖先の skip table を読むため、
当時は履歴長の対数に応じた処理と保存量がありました。SQLite の索引もサイズ依存です。
M1 の「厳密に履歴長非依存」という条件を証明・達成した結果ではありません。

作成時間は両環境で1回ずつの計測で、途中にビルド・他テストの実行もあります。
Docker 内と macOS のストレージ条件が異なるため、OS 間の優劣や本番スループットの比較には使えません。
このケースは同じ2種類の blob と小さな tree を再利用します。
幅の広い tree、大量の変更、全て異なる大きな blob、多数の並行 writer は対象外です。

[macOS CSV](divergent-history/macos.csv)、[Linux CSV](divergent-history/linux.csv)、
[最終 ID・集計・ソースハッシュ](divergent-history/summary.json) を保存しています。

## MergeBase の探索終了条件

従来は、最良の共通祖先を見つけた後も、その祖先を root まで走査していました。
新しい実装は、今後 best base になり得る frontier の集合を追跡し、
残る候補が全て既に見つかった base の祖先になった時点で終了します。

他の有効な frontier が残る間は、古い候補にも stale の印を祖先へ伝搬します。
最初の共通祖先で打ち切る処理や、stale node を無条件に飛ばす処理にはしていません。
これにより criss-cross の複数 best base を維持します。

検証は次のとおりです。

- 5 node の全1,024 DAG と全25,600 tip ペアを、推移閉包から列挙した全 best base と照合。
- 長さ10／100／1,000／5,000の main 履歴上に短い分岐を作り、fresh engine と保存済み Generation を使用。
  各 merge-base 問い合わせの MetaStore get が32回以下であることを検査。
- 既存のランダム DAG、2個・3個の best base、複数 base の仮想統合、行 merge の Git 差分テストを含む kasu-ops の13テストが両 OS で通過。
- macOS の実 Metadata／Writer／アカウント API の16回帰テストが通過。

[macOS の演算テスト](divergent-history/macos-tests.log)、
[Linux のビルド・テスト](divergent-history/linux-build.log)、
[実 API の回帰テスト](divergent-history/server-tests.log) に記録しています。

## 再実行

```sh
KASU_BENCH_DIVERGENT_HISTORY=1 \
  cargo bench -p kasu-ops --bench divergent_history --locked \
  > /tmp/history-macos.csv 2> /tmp/history-macos.log

docker build -f tools/history-benchmark/Dockerfile -t kasu:history-benchmark .
docker run --rm --network none kasu:history-benchmark \
  > /tmp/history-linux.csv 2> /tmp/history-linux.log
```

`KASU_BENCH_HISTORY_COUNT` は10〜1,000,000の10の倍数を指定でき、既定100,000です。
変数なしで通常の cargo test／bench に含まれる場合、このベンチ本体は実行しません。
Docker イメージは計測用の有効化変数を設定済みです。
タイミング閾値による合否は設定せず、結果の正しさと構造的な読み取り回数を検証します。
[CI workflow](../../.github/workflows/history-benchmark.yml) は同じ10万件を実行して記録を保存します。
GitHub 上での実行結果は未確認です。
