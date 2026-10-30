# 決定的 Land シミュレーション

[English](README.md) | 日本語

`land-1000-seed42.json` は 2026-10-23、macOS arm64／Rust 1.98.1 の release ビルドで実行した結果です。
本番の `Backend::prepare_land`／`commit_land` と実 SQLite WAL／synchronous=FULL、FS／zstd を使っています。

```sh
cargo run --release --locked -p kasu-sim -- --seed 42 --clients 1000
```

1,000クライアントは同じ ref と基底から開始します。全件の最初の prepare を済ませてから、
commit を seed によって決めた順序で競合させます。
各要求は最大3回の試行と決定的 jitter のバックオフを使います。
保存前の失敗33件、永続化後の応答喪失23件、キャンセル27件を注入しました。
イベントは仮想時刻で進み、スレッドの偶然の実行順に依存しません。

| 段階 | Landed | Conflict | Retry |
|---|---:|---:|---:|
| 最初の並行要求 | 9 | 0 | 991 |
| Retry を同じキーで再送した後 | 991 | 9 | 0 |

最初の競合波は全要求が試行予算内で終了します。
その後、Retry を返したクライアントが新しい有限予算で再送し、競合のないスケジュールで drain します。
これにより、単に全件 Retry を返して内容検査を避けるテストにはなりません。
100件に1件は同じ `shared.txt` を異なる内容にするため、最終的に9件が明示的な Conflict になります。

シミュレーターは結果を出す前に、次の条件を assert します。

- 受理した全991件の path／blob が最終 tree に残る。
- 着地先 ID の重複がなく、ref_log は初期 ref 作成1件＋着地991件の992件。
- 全確定結果が repo／key ごとに永続化され、返した結果と一致する。
- 新しい SQLite 接続と Backend から全1,000件を再送し、元の結果が replay される。
- replay 前後で ref_log は変わらない。

仮想時間21 msはスケジューラーの時計です。OS 上の実時間や1,000 TCP接続の処理時間ではありません。
小規模32クライアントでは同じ seed を2回実行し、結果 digest を含む全 summary の一致も検査します。
実非同期処理の期限・キャンセル・future 破棄は `land_runtime` テストで別途検証しています。
再実行・複数 seed 用の夜間 CI は [`land-sim.yml`](../../.github/workflows/land-sim.yml) です。
GitHub 上での実行結果は未確認です。

## cache 階層の負荷試験

`cache-1000.json` は実 Objects RPC による M9 の観測値です。
1つの認可済み Blob を、2つのノード daemon に各500クライアントで同時要求します。
両ノードは同じメモリ cache と権威 server に接続します。
各 daemon で499要求が coalesce され、cache から server への取得は1回でした。
別の信頼済み下位からの取得もメモリヒットになりました。
cache の既存 HTTP/2 接続を含めて終了した後、各ノードの10要求はディスクヒットし、
新しい Blob の cold miss は失敗しました。server の取得数は1回のままです。

```sh
KASU_CACHE_CAPTURE="$PWD/docs/simulations/cache-1000.json" \
  cargo test -p kasu-cache --test layers thousand_clients --locked
```

この記録は単一 repo・小さい1オブジェクトでの fan-in と障害時の経路を検証します。
実 TCP 通信のスケジュールは OS に依存します。次節に、同じ Layer を使う決定的な cache 障害シミュレーションを追加しました。
大きいオブジェクトや多数の cache インスタンスに対する容量・遅延の保証ではありません。
別テストで異なる12 ID を25 QPS・同時 fill 2以下に制限し、
上位障害時の遮断・復旧と stream／pending 上限も確認します。

## 決定的な cache 階層障害

`kasu-sim --scenario cache` は、本番の `Layer`、Objects client／handler、2つの実 DiskStore と
1つの regional memory cache を使います。上位リンクは Tokio duplex 上の HTTP/2／gRPC です。
client は2つの daemon Layer に要求し、上位の認証・role header・chunk・zstd／内容 hash の検証を通ります。
権威のデータは実 SQLite／FS store に保存します。

専用 current-thread runtime の Tokio 時計を pause し、スクリプトのイベントでだけ時刻を進めます。
blocking worker の IO 完了を待つ間は executor を runnable に保ち、自動時刻進行を防ぎます。
同じ clock で pacing・取得期限・回路遮断器の cooldown／probe を動かします。
回路遮断器が OS 時計を使っていた問題は、仮想時刻だけでは cooldown が解除されない回帰テストで再現し、修正しました。

```sh
cargo run --release --locked -p kasu-sim -- \
  --scenario cache --seed 42 --clients 1000
```

seed は payload と client の投入順序を決めます。入力0／1は区別され、結果 digest は seed、要求数、
投入順序、全 client の完了結果、イベント列と入力 ID を含みます。
従来の引数だけの実行は `--scenario land` と同じ結果を返します。

2026-10-26、seed 0／1／42／2026 を各2回、macOS arm64／Rust 1.98.1 と
固定 Rust 1.90 Linux arm64 で実行しました。計16回の1,000クライアントシナリオが同じ seed で byte 単位一致しました。
初回は両 node の499要求ずつが coalesce され、regional cache では2要求を1回の権威取得へまとめました。

| フェーズ | 仮想時刻 ms | 累積権威 RPC |
|---|---:|---:|
| 1,000要求の集約 | 5 | 1 |
| cache 不通時は cached read／cold miss の両方を fail closed | 9 | 1 |
| 空の cache を再起動 | 18 | 2 |
| 不完全な応答を拒否 | 20 | 3 |
| 上位 RPC の期限切れ | 80 | 5 |
| 保存失敗後の30要求を遮断 | 110 | 9 |
| probe も保存失敗し、再遮断 | 140 | 10 |
| 保存先復旧、全 tier が回復 | 170 | 11 |
| daemon store 再オープンの warm hit | 170 | 11 |

以下の保存済み trace は、キャッシュ読取ごとの権威識別子確認より前に取得したものです。現在のシミュレーションでは、識別子を確認する上位が不通の場合、cached read と cold miss の両方を拒否します。
途中で切れる応答は、実 handler が作った64 KiB chunk の後で失敗させます。未完了 frame は保存されません。
取得が停止した場合は仮想50 msの上位期限で終了し、その後の再取得で回復します。
保存失敗3回の後は30要求が追加の権威取得をせずに終了し、warm node hit は使えます。
cooldown 後の1回の probe が保存にも失敗すると再遮断し、復旧後には取得・保存・cache hit に戻ります。
cache 不通時と daemon DiskStore 再オープン時の追加権威取得は0回、全フェーズ末尾の pending は0です。

170 msは仮想時計であり、実時間や性能の値ではありません。
OS の packet loss／実プロセス SIGKILL、物理 ENOSPC／電源断、全 fault ordering の網羅を主張しません。
RPC 不通・不完全 stream・応答停止と Store::put エラーを注入する試験です。
既存の実 TCP の階層6テストも両 OS で通過しています。
macOS の cache／daemon／CLI／sim 全50テストも通過し、実プロセスと Land runtime の回帰を確認しました。

[summary.json](cache-deterministic/summary.json)、[seed 42 の全イベント](cache-deterministic/seed-42.json)、
[時計修正前の再現](cache-deterministic/clock-before.log)、[unit](cache-deterministic/unit.log)、
[実 TCP](cache-deterministic/layers.log)、[Linux ビルド](cache-deterministic/linux-build.log) に結果を保存しています。
`tools/cache-sim/record.py` は4 seed × 2回 × 2環境の一致、各フェーズの時刻・権威取得数・pending、source hash を照合します。

```sh
docker build -f tools/cache-sim/Dockerfile -t kasu:cache-sim .
mkdir -p /tmp/kasu-cache-sim
docker run --rm --network none --user "$(id -u):$(id -g)" \
  -e KASU_SIM_OUTPUT=/reports \
  --mount type=bind,source=/tmp/kasu-cache-sim,target=/reports \
  kasu:cache-sim
```

夜間の `land-sim.yml` に独立した cache job を追加しました。GitHub 上の実行結果は未確認です。

## 共有ストレージの受け入れ試験

`storage-host.json`／`storage-linux.json` は2026-10-23の実行結果です。
`storage-linux-tests.log` に Linux の全ストレージ試験と2サーバー試験の出力を保存しています。
PostgreSQL 18／SeaweedFS 4.45 は `tools/storage/compose.yml` の digest に固定しています。
ホストは macOS arm64、Linux は固定 Rust 1.90 の arm64 コンテナです。

```sh
python3 tools/storage/run.py --workdir /tmp/kasu-storage-host
python3 tools/storage/run.py --linux --workdir /tmp/kasu-storage-linux
```

16接続の mixed CAS／conditional PUT、8接続の同一 Land、SQL 障害と feed の順序、
webhook の再送、TLS／認証拒否を含みます。実 ingest と2つの server プロセスで、
Git atomic push／strict fsck、Metadata／Objects の共有、CAS の単一勝者、
Land の別 server・再起動後の replay を検証します。
時間はビルド・環境起動も含む受け入れ試験の実時間で、処理性能のベンチマークではありません。
ローカルの S3 互換サービスでの検証範囲と運用手順は [PostgreSQL／S3](../production-storage_ja.md) を参照してください。

## 権威サーバーの admission と計測

[server-runtime/summary.json](server-runtime/summary.json) は、共有する HTTP／gRPC の IP／PAT 割当量、
有限キュー、blocking worker と Objects／Git pack の切断、独立した有限 scrape 枠、CAS 結果と Derived Data の計測の検証記録です。
macOS arm64 の全 feature ワークスペース試験・clippy と、固定 Rust 1.90 Linux arm64 の Git 互換コンテナを使います。
各ログとソースの SHA-256 を記録します。実 TCP のスケジュールは OS に依存し、
bucket の補充・回収は別の決定的な時計の unit test で検証します。
これは分散 rate limiter や本番容量の保証ではありません。設定・計測の契約は [server-runtime.md](../server-runtime_ja.md) にあります。

## OpenTelemetry の実プロセス試験

[telemetry/summary.json](telemetry/summary.json) は、5つの実バイナリと OTLP HTTP 受信先による検証です。
公式 protobuf decoder が保存 request を読み、cold checkout の CLI → daemon → cache → authority の全親子関係、
warm hit、HTTP の外部親・認証失敗、sampling／無効化、秘匿値と baggage の不在、
collector の遅延・redirect・停止でも有限に終了することを検査します。macOS と固定 Rust 1.90 Linux で実行し、
ソースと各ログの hash を保存します。queue の飽和と scoped subscriber は unit test で別途検証します。
設定と保証範囲は [telemetry.md](../telemetry_ja.md) にあります。

## daemon の容量管理と eviction 復旧

[disk-lru/summary.json](disk-lru/summary.json) に、byte／索引数上限、永続 LRU、共有 Tree／Blob frame、
並行 open と snapshot、単体過大 frame の非保存、snapshot の容量事前拒否・pin・世代数を検証した記録を保存します。
SIGSTOP 後に file／SQLite の不一致を観測してから SIGKILL した試験と、実 daemon の Unix socket を通した
eviction 後の再取得・再起動後の warm hit を含みます。fixture 子プロセスのテストは親から実行するため通常実行では ignore です。
macOS／固定 Rust 1.90 Linux のログ・実測値・ソース hash を記録します。設定と範囲は [disk-cache.md](../disk-cache_ja.md) にあります。
