# 権威サーバーの制限とメトリクス

[English](server-runtime.md) | 日本語

`kasu-server` は HTTP JSON、Git Smart HTTP、Metadata／Writer／Objects gRPC に、
同じプロセス内で共有する admission 制御を適用します。`GET /metrics` は Prometheus text
format 0.0.4 です。完全な repository PAT で認証し、射影専用 PAT と Objects hop token は拒否します。
認証なしの公開サーバーでは metrics も公開になります。

```sh
curl --fail -H "Authorization: Bearer $KASU_TOKEN" http://127.0.0.1:50080/metrics
```

## 起動設定

省略時も既定の制限を適用します。変更する場合は `KASU_SERVER_LIMITS_FILE` に JSON ファイルのパスを設定します。
省略フィールドには既定値を使い、未知フィールド、不正な範囲、16 KiB 超過、読めないファイルは起動エラーです。
`per_identity_active` は `active` 以下に設定します。

```json
{
  "active": 64,
  "per_identity_active": 16,
  "pending": 1024,
  "queue_timeout_ms": 1000,
  "client_rate_per_second": 10000,
  "client_burst": 20000,
  "token_rate_per_second": 1000,
  "token_burst": 2000,
  "max_clients": 4096,
  "idle_ms": 60000
}
```

| 設定 | 許容範囲 | 意味 |
|---|---|---|
| `active` | 1–4096 | 通常リクエストの同時処理数 |
| `per_identity_active` | 1–`active` | 認証主体ごとの同時数。公開サーバーでは接続元 IP ごと |
| `pending` | 1–65536 | 待機リクエストの上限 |
| `per_repository_active` | 1–`active` | 1 リポジトリの同時処理数（既定 `active` の半分）。リポジトリ設定の `limits.active` でさらに絞れます |
| `per_repository_pending` | 1–`pending` | 1 リポジトリの待機数（既定 `pending` の 1/4） |
| `queue_timeout_ms` | 1–30000 | 同時実行枠を待つ最大時間 |
| `client_rate_per_second`, `client_burst` | 各1–1000000 | 接続元 IP の token bucket の補充速度と容量 |
| `token_rate_per_second`, `token_burst` | 各1–1000000 | 認証主体の token bucket の補充速度と容量 |
| `max_clients` | 1–65536 | 接続元の管理表上限。満杯なら新しい IP を拒否 |
| `idle_ms` | 1000–3600000 | 未使用の接続元を回収できる最短時間 |

IP の割当量を確認してから資格情報を検証します。認証済み要求は IP と認証主体の両方の
レート制限を消費します。Bearer と Basic の username を変えても同じ PAT は同じ主体です。
HTTP／gRPC と各サービスで割当量を共有し、運用者 PAT、[各 repository account](repository-accounts_ja.md)、各射影 PAT、Objects hop token は別の主体です。
接続元にはソケットの peer address を使い、IPv4-mapped IPv6 を IPv4 に揃えます。
`Forwarded`／`X-Forwarded-For` は使いません。reverse proxy の背後では、その proxy の IP に集約されます。

管理表の掃除は、アイドル期限経過に加えて bucket が満充填になり、待機・実行中の保持者がいないことを条件とします。
期限による割当量のリセットや、処理中の主体の二重作成を防ぎます。
無効な資格情報から token ごとのエントリーを作ることはありません。

枠がない要求は有界キューで待ち、満杯・待機期限・レート超過は HTTP 429 または gRPC
`RESOURCE_EXHAUSTED` と `Retry-After: 1` を返します。副作用のある要求の自動再送は行いません。
Writer／Land の再送には、それぞれ既存の CAS／冪等性の契約を使ってください。

同時実行枠は応答ストリームの終了・破棄まで保持します。クライアントが future を破棄しても、
既に開始した blocking worker が終了するまで枠は残ります。Objects の producer と Git pack 配信も対象です。
`/metrics` は通常処理が満杯でも取得できるよう、専用の2枠を使います。この2枠も body の破棄まで保持し、
満杯なら待機せず 429 を返します。metrics にも IP／token のレート制限と認証を適用します。

複数プロセスの admission は独立しています。クラスタ全体の共通割当量や接続数制限、
reverse proxy の信頼設定はこの実装に含みません。これはリクエスト数の制限であり、CPU・メモリの容量保証ではありません。

リポジトリ枠を待つ要求は、待機中にサーバー全体の実行枠を返します。
混んでいるリポジトリが `active` を占有して他のリポジトリを止めることはありません
（[公平性](multi-repository_ja.md#多テナント運用)）。リポジトリ単位の計測は
`KASU_METRICS_REPOSITORIES` に挙げたリポジトリだけをラベルにし、それ以外は `other` に集約します。

## 計測の意味

全カウンターはプロセス起動時にリセットされます。固定された transport／method／status と登録済み
Derived Data の name／version だけをラベルに使い、PAT、IP、repo、パス、Changeset ID を含めません。
未知の URL は `Unknown` に集約します。

| メトリクス | 意味 |
|---|---|
| `kasu_server_requests_total{transport,method}` | 受け取った要求。認証・制限による拒否を含む |
| `kasu_server_results_total{transport,method,code}` | 完了した要求。HTTP の結果も gRPC status 名へ対応付ける |
| `kasu_server_duration_seconds{transport,method}` | 待機からストリーム・worker の保持者が全て終了するまでの histogram |
| `kasu_server_response_bytes_total{transport,method}` | admission 通過後に body から送り出した data bytes。ネットワークの ACK や拒否応答のサイズは含まない |
| `kasu_server_active`, `kasu_server_pending` | 通常の処理中・待機中の数 |
| `kasu_server_method_active{transport,method}` | method ごとの保持中の要求数。metrics の専用枠も含む |
| `kasu_server_metrics_active`, `kasu_server_client_entries` | 使用中の scrape 枠と接続元エントリー数 |
| `kasu_server_git_cache_hits_total`, `kasu_server_git_cache_misses_total`, `kasu_server_git_cache_bytes` | 有界 Git zlib cache の hit／miss と保持 bytes |
| `kasu_server_ref_cas_total{operation,result}` | backend が発行した native／git／mixed／land の保存操作の結果 |
| `kasu_derived_cache_lookups_total{name,version}`, `kasu_derived_cache_hits_total{name,version}` | scheduler による永続キャッシュの存在確認回数とヒット回数 |
| `kasu_derived_compute_success_total{name,version}`, `kasu_derived_compute_errors_total{name,version}` | 実際の derive と保存の成功・失敗 |
| `kasu_derived_compute_active{name,version}`, `kasu_derived_compute_seconds{name,version}` | 実計算と保存の実行数・遅延 histogram |

切断や待機中の future 破棄は `cancelled`、body のエラーは `internal` として集計します。
完了前の要求は requests と results に差が生じます。HTTP 2xx の Land business outcome は transport としては `ok` です。
CAS は操作の単位で数え、バッチの ref 数ではありません。`land` の `replayed` は保存 API 内で receipt が再利用された場合です。
Land が事前の receipt 読み取りで返した replay は CAS を呼ばないため含みません。
`matched` には ref を動かさず terminal result を確定した Land も含みます。

Derived Data の lookup は要求数ではなく、依存探索・singleflight 後の再確認も含みます。
計算遅延は依存の探索・計算と flight lock の待機を除き、その型の derive と保存を測定します。
依存解決前のエラーは compute error に含みません。計測は保存キー、codec、Derived Data の version を変えません。
既存の cache／daemon のメトリクスは [cache 階層](cache-hierarchy_ja.md) を参照してください。

例として、派生キャッシュのヒット率と計算遅延の95パーセンタイルは次のように求められます。

```promql
rate(kasu_derived_cache_hits_total[5m]) / rate(kasu_derived_cache_lookups_total[5m])
histogram_quantile(0.95, sum by (name, version, le) (rate(kasu_derived_compute_seconds_bucket[5m])))
```

## ライブラリと検証範囲

バイナリと `http_router(Arc<Backend>)`、各 Service の `into_server()` が共有制御を組み込みます。
`Backend::with_limits` で設定し、`Backend::metrics` で同じ計測値を取得できます。
独自に generated tonic server を作る場合は `ServerLayer::grpc` を適用します。
`write_http_router`／`git_http_router` は合成用の下位 router なので、単独で使う場合は
`ServerLayer::http` と `ConnectInfo<SocketAddr>` を設定してください。
peer のない埋め込みリクエストは、全て同じ unknown-client bucket を使います。
新しい blocking worker を追加するときは、server 内の共通 helper で枠と tracing span を引き継ぎます。

テストは補充と回収の決定的な時計、待機数・期限・キャンセル、blocking worker、body の保持、
CAS の競合／replay／cancel／error、並行 singleflight の計測と失敗後の再試行を検証します。
実 HTTP／gRPC は資格情報と IP の共有、転送ヘッダー無視、認証、未知パスのラベル集約、
実バイナリでの設定読み込み、8 MiB の Objects／Git pack の backpressure と切断後の回復を検証します。
実行ログと環境・ソースの hash は [検証記録](simulations/server-runtime/summary.json) に保存します。

tracing の request span は固定 method／transport を持ち、blocking worker に引き継ぎます。
[OpenTelemetry の exporter と trace context の伝搬](telemetry_ja.md) も追加しました。OIDC、アカウント別 native ACL、
大規模な実通信での容量・公平性の評価は残作業です。
