# OpenTelemetry tracing

`kasu-telemetry` は `tracing` span を OTLP/HTTP の binary protobuf で送信します。
CLI、daemon、cache、権威サーバー、Git ingest の実行ファイルが共通の初期化を使います。
既定では送信せず、stderr のログだけを初期化します。`OTEL_TRACES_EXPORTER=otlp` で有効になります。
メトリクスは引き続き [権威サーバー](server-runtime.md) と [cache／daemon](cache-hierarchy.md) の Prometheus endpoint で取得します。

```sh
export OTEL_TRACES_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
export OTEL_TRACES_SAMPLER=parentbased_traceidratio
export OTEL_TRACES_SAMPLER_ARG=0.1
```

この環境で各プロセスを起動します。汎用 endpoint の末尾に `/v1/traces` を追加するので、
上の例は `http://127.0.0.1:4318/v1/traces` に送信します。
`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` を指定した場合は、その URL をそのまま使います。
HTTPS は WebPKI の証明書検証を行います。collector は別途起動・運用してください。

## 設定

| 環境変数 | 対応 |
|---|---|
| `OTEL_TRACES_EXPORTER` | `none`（既定）または `otlp` |
| `OTEL_SDK_DISABLED` | `true` で無効化。省略または `false` で上の設定を使う |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | 汎用 base URL。省略時は `http://localhost:4318` |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | trace 専用の完全な URL。汎用設定より優先 |
| `OTEL_EXPORTER_OTLP_PROTOCOL`, `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` | `http/protobuf` のみ。trace 専用を優先し、省略時も同じ |
| `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_EXPORTER_OTLP_TRACES_HEADERS` | SDK の header 設定。collector の認証等に使用 |
| `OTEL_TRACES_SAMPLER` | `parentbased_always_on`（既定）、`parentbased_always_off`、`parentbased_traceidratio`、`always_on`、`always_off`、`traceidratio` |
| `OTEL_TRACES_SAMPLER_ARG` | ratio sampler では必須。有限の 0–1 |
| `RUST_LOG` | stderr ログの filter。OTLP の対象 span とサンプリングには影響しない |

有効化時の不正な設定は起動エラーです。endpoint は HTTP(S) のみで、4096 byte 以下、
userinfo・query・fragment を含まない URL に制限します。診断には設定値を出しません。
無効化時は exporter を作らず、endpoint・sampler 等の送信設定も読みません。

`service.name` は `kasu-cli`、`kasu-daemon`、`kasu-cache`、`kasu-server`、`kasu-ingest` の固定値です。
resource には `service.version` も設定します。`OTEL_SERVICE_NAME`／`OTEL_RESOURCE_ATTRIBUTES`、
`OTEL_PROPAGATORS`、`OTEL_BSP_*` による上書きは適用しません。
この表以外の一般的な SDK 設定との互換性は保証しません。

実装は既存の tonic／prost と Rust 1.90 に対応する OpenTelemetry 0.31／OTLP 0.31.1 と
tracing-opentelemetry 0.32.1 を lock しています。
[公式 exporter API](https://docs.rs/opentelemetry-otlp/0.31.1/opentelemetry_otlp/) と
[tracing bridge API](https://docs.rs/tracing-opentelemetry/0.32.1/tracing_opentelemetry/) を使います。

## 通信と span の寿命

CLI のコマンドは root operation span を持ち、Metadata／Writer の gRPC に現在の
W3C `traceparent`／`tracestate` を付加します。Objects の取得は全 stream を含む client span を持ちます。
Unix socket の daemon → cache → authority でも隣接する RPC ごとに親子関係を引き継ぎます。
HTTP JSON と Git Smart HTTP も同じ W3C context を受け取れます。

受信側は server span を新設します。欠落・不正・重複・512 byte を超える traceparent は
新しい root にします。`traceparent` と `tracestate` をそれぞれ512 byte 以下に制限し、構文検証は
SDK の TraceContext propagator が行います。baggage は抽出・転送しません。
trace context は認可に使いません。既定の ParentBased sampler は、受信した sampled flag を尊重します。

権威サーバーの span は既存の admission lease と同じ寿命を持ち、応答 header の生成で閉じません。
body の終了・破棄と、開始済み blocking worker の終了を待ちます。
cache／daemon の server span も応答 body と producer が保持します。
blocking worker と async task には span と scoped subscriber を引き継ぎます。

cache miss は `object_fill` operation span を持ちます。singleflight では最初の要求の trace に
1つの fill と上位 RPC を紐付け、合流した各要求は自身の server span を持ちます。
他の trace への fan-in link は追加しません。warm hit は上位への client span を作りません。

`kasu.code` は gRPC status の数値です。HTTP は [サーバーメトリクス](server-runtime.md) と同じ対応付けを使います。
正常終了は span status `OK`、エラーは `ERROR` です。途中の body 破棄は `CANCELLED`、
body error は `INTERNAL` に対応します。CLI／ingest の一般エラーは `UNKNOWN` です。
HTTP 2xx の Land business outcome は transport としては成功です。ref の競合は別の CAS メトリクスで確認します。

## 送信の上限と障害

exporter は専用 thread の batch processor を使い、アプリケーションの Tokio worker では HTTP 送信を行いません。

| 上限 | 値 |
|---|---:|
| 未送信 span queue | 2048 |
| 1 batch | 128 span |
| 送信間隔 | 1秒 |
| collector 接続 timeout | 1秒 |
| 1回の HTTP export timeout | 2秒 |
| 終了時 shutdown／flush の待機 | 最大3秒 |
| span attributes | 最大16 |
| span events／links | 0 |

queue が満杯なら span を捨て、通常処理を待たせません。collector の拒否・停止・遅延は、
既に完了したリポジトリ操作の結果を変えません。短命の CLI／ingest は終了時に有限の flush 待ちを行います。
長く停止した collector や queue 超過で失われた trace の永続再送は行いません。
HTTP redirect は追わず、collector 用の認証情報を別 URL に転送しません。
SIGINT による通常終了時は flush します。SIGKILL や process crash での flush は保証しません。

OTLP に送る対象は専用 target の固定 span です。既存の診断 event、Git pkt-line、
本文、パス、repo、Changeset／Blob ID、接続先 URL、PAT を span attribute に取り込みません。
tracestate は W3C の vendor context としてそのまま扱います。stderr の既存診断ログは `RUST_LOG` に従う別の出力です。
OTLP metrics／logs、gRPC／JSON exporter、独自 CA、動的設定変更は実装していません。

## ライブラリと受け入れ試験

埋め込みアプリケーションは `kasu_telemetry::init` の guard を保持し、全 operation span を閉じてから guard を破棄します。
subscriber を独自に構成する場合も、公開された span／injection helper と `GrpcLayer` を利用できます。
`http_router`、権威サーバーの各 `into_server()`、cache／daemon の `Service::into_server()` は既に組み込み済みです。

```sh
RUSTC_WRAPPER= cargo test -p kasu-telemetry --locked
RUSTC_WRAPPER= cargo build -p kasu-server -p kasu-cli -p kasu-cache -p kasu-daemon -p kasu-ingest --locked
RUSTC_WRAPPER= cargo build -p kasu-telemetry --example inspect_otlp --locked
python3 tools/telemetry/verify.py --out /tmp/kasu-telemetry
```

runner は実 ingest と5つの実バイナリ、TCP gRPC／HTTP と Unix socket、OTLP HTTP 受信先を使います。
OTLP request は公式 protobuf 型の `inspect_otlp` example で decode し、trace ID と全親子関係・時刻、
cold／warm の経路、認証失敗、重複・不正 context、sampling、無効化、秘匿値・baggage の不在、
collector の遅延・redirect・停止、通常終了を assert します。
unit test は scoped subscriber と blocking worker の寿命、および詰まった exporter に対する有界 queue も検証します。

[検証ログと hash](simulations/telemetry/summary.json) に macOS arm64 と固定 Rust 1.90 Linux arm64 の記録を保存します。
Linux は `tools/telemetry/Dockerfile` で再実行できます。CI 設定にも追加しましたが、GitHub 上の実行結果は未確認です。
実運用の collector、長期保存、大規模な trace volume、別ホスト間の時計ずれ、durable webhook を跨ぐ trace の連結は未検証・今後の対象です。
