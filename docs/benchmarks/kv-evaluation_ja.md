# 分散 KV の初期評価

[English](kv-evaluation.md) | 日本語

M10 の候補として TiKV v8.5.8 を実測しました。現在の本番ストレージは
[PostgreSQL／S3](../production-storage_ja.md) を継続します。
TiKV では複数 region の原子更新と replica 障害後の再送を確認できましたが、
読み取り条件の保護、メタデータサイズ、順序付きログの競合への対応が移植時に必要です。

評価用コードは [`tools/kv-eval`](../../tools/kv-eval/) の独立した Go module です。
ここで測定するのは保存プリミティブの試作実装です。Rust の `kasu-store` の
PostgreSQL 実装と実サーバーの検証は [共有ストレージ試験](../production-storage_ja.md#実通信の検証) に記録しています。

## 再実行

```sh
python3 tools/kv-eval/run.py --workdir /tmp/kasu-kv-evaluation
```

Go 1.27.3 と Docker を使いました。Docker のメモリは最低10 GiBを検査し、12 GB以上を推奨します。
スクリプトは3 PD、3 TiKV、比較用の単一 PostgreSQL を専用 Compose project に起動します。
ホストへのポート公開はありません。検証用の公開パスワードだけを使用し、
終了時にこの実行のコンテナと volume を削除します。`--keep` で調査用に保持できます。
`--operations` の既定値は各 workload 256件です。

`compose.yml` の TiKV／PD v8.5.8 と PostgreSQL 18 はイメージ digest に固定しています。
TiKV は各1 CPU／3 GiB上限、block cache 128 MiB、Raft Engine のメモリ設定64 MiBです。
PD は各512 MiB、PostgreSQL は1 GiB上限です。
同一 Docker ホストで実行するため、別マシンや AZ の障害・ネットワーク遅延は含みません。
TiKV の起動は当初の1.5 GiB制限で OOM になり、上記設定へ調整しました。

クライアントは [TiDB v8.5.8 の go.mod](https://raw.githubusercontent.com/pingcap/tidb/v8.5.8/go.mod) にある
`client-go/v2 v2.0.8-0.20260803075849-c3b50791b9fb` を使い、gRPC／etcd／x/net などの
依存は監査の修正版に固定しています。`go vet` と `govulncheck v1.9.0` も毎回実行します。
Go の実行版、バイナリと全評価ソースの SHA-256、設定、各段階の JSON／ログを作業 directory に残します。

## 保存する内容と測定条件

TiKV はクライアント既定の TxnKV API v1 を使い、optimistic transaction と通常の2PCに固定します。
1PC／async commit は無効です。[クライアントの API 選択](https://docs.rs/tikv-client/0.4.0/tikv_client/#choosing-an-api) に従い、
この評価では RawKV を混在させません。API v2／keyspace 分離や別 RawKV cluster は未評価です。

不変 KV は BLAKE3 の索引と元キー・値を持つ envelope を transaction で put-if-absent します。
ref は target と単調な version、ログは repo／stream のカウンター、
冪等結果は repo／キーと fingerprint を持ちます。更新と両種ログ・結果を同じ transaction に保存します。
この試作のレコード形式は JSON／バイナリ envelope です。
PostgreSQL の比較用実装も同じキー・値を `SERIALIZABLE` の KV テーブルに保存します。
製品の PostgreSQL は専用 schema と明示的な行ロックを使います。

region を明示的に split し、ref／ログ／カウンター／結果が異なる region になることを照合します。
8 repo の workload はカウンターを8つの region と複数の leader に配置し、
各 region に3 peer があることと scatter operator の完了を待ちます。
route／接続を warm up してから測定します。この配置では region 間の2PCコストも発生します。
repo の全レコードを同じ region へ配置する最適化は未評価です。

各要求は最大4回の transaction 試行、既定3秒の期限を持ちます。
`Retry` は初回応答の統計に残し、その後同じキー・fingerprint・expected を新しい有限予算で再送します。
再送後の全 ref／ログを検査し、成功・再送結果の欠落や二重着地を拒否します。
キャンセル、保存前 rollback、保存後の応答喪失、native／Git の tombstone と ABA、
長い非 UTF-8 キー・空の値、別 fingerprint によるキー再利用も検査します。

この workload はメタデータ KV の比較です。Blob は引き続き S3 に保存します。
PostgreSQL の単一ノードと TiKV の3レプリカは構成・耐障害性が異なり、
測定値を同じ HA 構成の比較や Rust サーバーの性能保証として扱うことはできません。

## 読み取り条件を持つ結果の保存

ref を動かさずにチェック済みの Land 結果を保存する場合を想定し、次の順序を作りました。

1. A が ref v1 を読み、別キーの結果を書き込む transaction を保持する。
2. B が ref v2 を commit する。
3. C が1つの snapshot で ref v2 と「A の結果なし」を読む。
4. A が結果の commit を試みる。

比較用 PostgreSQL の `SERIALIZABLE` は A を競合として拒否しました。
TiKV の snapshot isolation では、A の read-only dependency だけでは commit を止められませんでした。
TiKV で A が同じ ref 値を書き戻す fence を加えると、競合が検出され、古い結果の commit は拒否されました。
この結果から、TiKV への移植ではチェック対象を write set に含めるなど、読み取り条件を明示的に守る必要があります。
製品の PostgreSQL `commit_land` は ref の行ロックを結果の保存まで保持します。

## サイズと障害試験

1 KiB／1 MiB の不変値は両方で保存・再取得できました。
8 MiB の raw 値は比較用 PostgreSQL で保存でき、今回の TiKV 設定は
envelope を含む Raft entry のサイズ超過として拒否しました。拒否した値が公開されないことも検査します。
3点のプローブなので、最大許容サイズ全体を特定した結果ではありません。
大きい Changeset などの格納には、設定変更や S3 への間接参照の評価が必要です。

障害は、検証済みの3 peer の region に対して注入します。

- データ leader と PD leader をそれぞれ `SIGKILL`。leader 置き換え後、更新・応答喪失・再送が1回の公開に収まることを検査。
- データ leader を含む2ノードを停止。2秒の transaction 期限内で書き込み成功を返さないことを検査。
- quorum 復旧後に同じ要求を最大10個の有限予算で再送。各 `Retry` と所要時間も記録し、元の expected を変更せず着地させる。
- 全プロセスを再起動。不変値・ref・ログ・結果などの全安定レコードの digest と件数を照合し、
  故障中に変わる ref／カウンター／ログ本文／4つの冪等結果を別に全件検査。

leader 置き換え時間は Docker 操作と観測 poll を含み、置き換え後の要求時間と分けて記録します。
復旧直後の TCP 接続可能状態だけでは、書き込みが直ちに成功するとは限りません。
進行中 commit の全ての強制終了タイミングや、Jepsen 相当の全インターリーブを網羅したものではありません。

## 2026-10-23 の観測値

[実行 summary](kv-evaluation/summary.json)、[PostgreSQL](kv-evaluation/postgres-baseline.json)、[TiKV](kv-evaluation/tikv-baseline.json) の生データを保存しています。
Linux arm64 の同一ホスト、各 workload 256件、4 KiB の不変値です。

| 試作 | 操作 | 並行数 | 初回成功 | Retry | 応答 p95 ms | 初回 Ack/s | 再送 drain ms |
|---|---|---:|---:|---:|---:|---:|---:|
| postgres | 不変 PUT | 1 | 256 | 0 | 0.276 | 4551.3 | 0.0 |
| postgres | 不変 GET | 1 | 256 | 0 | 0.144 | 8279.4 | 0.0 |
| postgres | ref CAS・1 repo | 8 | 235 | 21 | 32.636 | 2395.7 | 9.5 |
| postgres | ref CAS・8 repo | 8 | 249 | 7 | 7.993 | 3671.9 | 3.3 |
| tikv | 不変 PUT | 1 | 256 | 0 | 2.284 | 504.9 | 0.0 |
| tikv | 不変 GET | 1 | 256 | 0 | 0.454 | 4776.4 | 0.0 |
| tikv | ref CAS・1 repo | 8 | 196 | 60 | 3000.337 | 19.3 | 209.8 |
| tikv | ref CAS・8 repo | 8 | 254 | 2 | 10.485 | 1052.2 | 6.5 |

p95 は Retry を含む初回の全応答です。Ack/s は初回に成功・replay を確認した件数を測定時間で割り、
drain は初回 Retry を同じキーで順次再送した別の時間です。再送後は全256件の内容とログを検証しています。
1 repo の TiKV ではカウンター競合とロック解消待ちが目立ち、8 repo の配置で改善しました。
キー配置・retry 方式・2PC 設定を含む今回の試作1回の観測です。一般的な性能比較へ広げることはできません。

16同時 mixed CAS の初回応答は PostgreSQL が `{'Applied': 1, 'Mismatch': 15}`、
TiKV が `{'Applied': 1, 'Mismatch': 8, 'Retry': 7}` でした。再送後は各1回の公開と両種のログ一致を確認しました。

データ leader の置き換え観測は 14.766秒、PD leader は 12.066秒でした。
quorum 喪失時は約2.001秒で Retry。復旧後は
5回の有限 Retry の後、元のキーと expected のまま着地しました。
[各復旧応答](kv-evaluation/tikv-recover.json) に回数と時間を保存しています。

全再起動後は PostgreSQL の安定レコード1,853件、TiKV の1,857件の
digest／件数が元と一致し、故障処理の全 ref／カウンター／ログ／receipt も一致しました。
[TiKV の全件検証](kv-evaluation/tikv-verify.json)、[PostgreSQL の全件検証](kv-evaluation/postgres-verify-baseline.json) を参照してください。

`docker stats` の測定時点では TiKV は各約2.2 GiB、3 PD は合計約0.2 GiB、
比較用 PostgreSQL は約0.08 GiBでした。これは小さいデータセットでの1時点の値で、容量見積もりやピーク値ではありません。
[resource sample](kv-evaluation/resource-sample.log) に元の表示を保存しています。

## 採用判断

現在は、実サーバーで検証した PostgreSQL／S3 を既定の共有ストレージとして使います。
TiKV の本番アダプターを追加する前に、次を揃えます。

- Rust の全保存 trait、checked-result の fence、webhook cursor／pending body、未知の commit 結果の再送を含む実サーバー E2E。
- 大きいメタデータの格納方針と、repo のキー配置・順序付きログカウンターの競合対策。
- TLS／認可、複数ホスト・同等 HA 構成、長時間負荷、バックアップ／復元、障害タイミングの追加検証。

FoundationDB は今回の比較には含めていません。
TiKV の観測結果から FoundationDB の性能・適合性を推定することもしていません。
