# FirstParentSkip v2 と作成時の固定参照コスト

[English](first-parent-skip-v2.md) | 日本語

FirstParentSkip の eager index を v2 に変更しました。
v1 は各距離 `2^k` の祖先を保存する表で、作成時に履歴長に応じた参照と保存量が必要でした。
v2 は第一親の深さ、直前の親、1本の jump を保存します。
新規作成時の index 参照は7回以下、レコードは root が10 bytes、それ以外は82 bytes です。
元の Changeset／Tree／Git OID は変わりません。

## 構造と問い合わせ

root の深さを0、子の深さを第一親の深さ+1とします。
既定の jump は直前の親です。親の jump と、その行き先の jump が同じ距離の場合、
二つをまとめた行き先へ跳びます。子からの距離は `1 + 2d` となり、1、3、7、15…の長さになります。
親の値はスケジューラから受け取り、追加で読む既存 index は高々1件です。
親が保存済みの通常の eager 作成では、スケジューラを含む index の MetaStore get は7回以下になります。

第一親の祖先問い合わせでは、対象の深さを越えない jump を使い、越える場合は直前の親へ進みます。
同じ深さに到達したら ID を比較するため、別 branch や第二親を第一親の祖先とは扱いません。
第一親の深さと、全親の最大を使う Generation は別の値です。
全親の IsAncestor／MergeBase は引き続き Generation を使います。

## v1 との共存とバックフィル

- eager の Rust 型 `FirstParentSkip` は `derived/FirstParentSkip@2/...` に保存します。
- `FirstParentSkipV1` は旧 `@1` 形式の読み取り・明示的な遅延計算用として残します。
- `Engine::standard()` は両版を登録しますが、新規作成時は v1 の表を作りません。
- v1 の値を削除・上書きせず、元の Changeset ID と既存の ref／log cursor の形式も維持します。

既存履歴に v2 がない場合、初回の問い合わせまたは新規作成で、必要な第一親チェーンを
反復スケジューラでバックフィルします。この初回処理は履歴長に依存します。
通常の作成を固定参照コストにするには、親の v2 index が計算済みである必要があります。
移行時には各 ref の先端に対して、通常の Derived Data API で先に計算できます。

```rust
engine.derive::<kasu_derived::FirstParentSkip>(&ctx, branch_head, &())?;
```

この処理は不変の Derived Data を追加するだけで、ref を動かしません。
新規専用の管理 RPC／コマンドは追加していません。バックフィルは一度計算した共通祖先を再利用します。
旧 v1 を明示的に再計算する場合、その計算と保存量は従来どおり対数に依存します。

## 2026-10-24 の10万件計測

[分岐合流のベンチ](divergent-history_ja.md) と同じ、100,000 Changeset・10,000個の
分岐合流を使いました。測定は macOS arm64／Rust 1.98.1 と Linux arm64／固定 Rust 1.90.0 です。
両 OS の最終 Changeset ID と Tree ID は、v1 の計測結果とも一致しました。
計測コードが作る履歴・ファイル内容は変更していません。

| 区間 | v1 MetaStore get／件 | v2 MetaStore get／件（両 OS） | v2 区間内の最大 get／件 |
|---|---:|---:|---:|
| 1〜10,000 | 35.479 | 24.299 | 27 |
| 45,001〜55,000 | 39.300 | 24.300 | 27 |
| 90,001〜100,000 | 40.300 | 24.300 | 27 |

作成処理全体の get の最大は43回から27回へ減りました。
MetaStore put は各区間で平均4.00回、BlobStore get／put は平均3.00回／2.00回でした。
これはストア API の呼び出し数で、内部 SQL 文や OS の物理 I/O を数えたものではありません。

| 区間 | macOS 作成時間／件 | Linux 作成時間／件 |
|---|---:|---:|
| 1〜10,000 | 379.80 µs | 531.34 µs |
| 45,001〜55,000 | 466.66 µs | 910.88 µs |
| 90,001〜100,000 | 451.03 µs | 639.17 µs |

小さく固定した tree と2種類の blob を再利用し、SQLite WAL／synchronous=FULL とローカル FS／zstd を使いました。
入力構築、結果の ancestry／merge-base／内容照合、CSV 出力は作成時間から除外しています。
カウンターの atomic increment は時間に含まれます。
各 OS で1回の計測で、他のビルド・テストも一部並行しています。

作成時の index 構築は、既存履歴の長さに応じた表の走査・保存を必要としなくなりました。
一方、SQLite の B-tree、filesystem、キャッシュ、ディスク同期の実時間まで定数とは保証しません。
幅の広い tree、多数の変更パス、並行 writer、本番ストレージの性能を保証する結果でもありません。
M1 の評価では、固定した変更量でのアプリケーション側の参照数・index 保存量と、実ストレージの遅延を区別します。

[macOS CSV](first-parent-skip-v2/macos.csv)、[Linux CSV](first-parent-skip-v2/linux.csv)、
[集計・最終 ID・ソースハッシュ](first-parent-skip-v2/summary.json) を保存しています。

## 検証

- 1万件を作成し、各作成の v2 index get が7回以下、保存が10／82 bytes、v1 参照が0回であることを検査。
- 深さの境界と決定的なサンプルで第一親祖先を照合し、問い合わせの index get が100回以下であることを検査。
- 第一親が浅く第二親が深い merge で、全親 ancestry と第一親 ancestry を区別。
- 全1,024個の5 node DAG・25,600 tip ペアで、第一親判定を実際の親チェーンと照合。
- SQLite 再オープン後の v1 保存内容の維持、v2 バックフィル、追記、Changeset ID 不変を検査。
- 不整合な depth／parent／jump を codec が拒否。

両 OS で kasu-ops の16テスト、kasu-derived の選択した5テストが通過しました。
macOS の実 Metadata／Writer／ACL の16テスト、実 CLI の12 E2E も通過しています。
[演算テスト](first-parent-skip-v2/macos-tests.log)、[Derived Data](first-parent-skip-v2/derived-tests.log)、
[Linux ビルド](first-parent-skip-v2/linux-build.log)、[実 API](first-parent-skip-v2/server-tests.log)、
[CLI](first-parent-skip-v2/cli-tests.log) に記録しています。

```sh
cargo test -p kasu-ops --test first_parent --test operations --locked
KASU_BENCH_DIVERGENT_HISTORY=1 \
  cargo bench -p kasu-ops --bench divergent_history --locked
docker build -f tools/history-benchmark/Dockerfile -t kasu:skip-v2-test .
docker run --rm --network none kasu:skip-v2-test
```

[CI](../../.github/workflows/history-benchmark.yml) も移行・DAG 検証と10万件の計測を行います。
GitHub 上の実行結果は未確認です。旧計測の CSV は当時の v1 の記録として保持しています。
