# Docker による kasu のデプロイ

[English](README.md) | 日本語

kasu をコンテナで動かす方法を、シナリオごとに説明します。目的に合うものを選んでください。

| シナリオ | 用途 | ストレージ | サーバー数 | ガイド |
|---|---|---|---|---|
| 開発 | 試用、ローカル開発、CI のフィクスチャ | ボリューム上の SQLite とローカルファイル | 1(認証なし) | [開発用](development_ja.md) |
| 1 ノード | 小規模なチームや 1 台構成の本番運用 | ボリューム上の SQLite とローカルファイル(または同一ホスト上の PostgreSQL / S3) | 1 | [1 ノード構成](single-node_ja.md) |
| 高可用性(HA) | 複数ノード、ローリングアップグレード、kasu 自体に単一障害点を作らない構成 | すべてのサーバーが共有する PostgreSQL と S3 互換ストレージ | ロードバランサー配下に 2 台以上 | [高可用性構成](high-availability_ja.md) |

ガイドは順に積み上げる構成です。最初にこのページで、全シナリオに共通する内容を読んでください。

## イメージ

`Container` ワークフローが、マルチアーキテクチャ(`linux/amd64`、`linux/arm64`)のイメージを 2 つ公開します。タグについては[リリースとコンテナイメージ](../release_ja.md)を参照してください。

| イメージ | 内容 | ポート |
|---|---|---|
| `ghcr.io/sileader/kasu/server:<version>` | `kasu-server`(`production` feature 付きでビルドされており、PostgreSQL と S3 を使えます)と `kasu-ingest` | 50051(gRPC)、50080(HTTP: JSON API、Git Smart HTTP、`/metrics`) |
| `ghcr.io/sileader/kasu/web:<version>` | `kasu-web`([Web UI](../web-ui_ja.md)) | 50090 |

どちらも `debian:bookworm-slim` 上で UID/GID 10001 として動作します。以下の例ではバージョンを `0.1.0` と書いています。実際にデプロイするリリースに置き換え、`latest` ではなくバージョンを固定してください。

チェックアウトからイメージを自分でビルドする場合は次のようにします(BuildKit が必要です)。

```sh
docker build -f docker/Dockerfile --target server -t kasu/server:local .
docker build -f docker/Dockerfile --target web -t kasu/web:local .
```

イメージには `kasu-cache`、`kasu-daemon`、`kasu-git-ssh`、`kasu` CLI は含まれません。CLI はリリースのアーカイブから入れるか、`cargo build -p kasu-cli` でビルドしてください。それ以外が必要な場合はソースから実行するか、独自のイメージを作ります([キャッシュ階層](high-availability_ja.md#オプションのキャッシュ階層)を参照)。

### サーバーイメージの起動方法

エントリポイントは `kasu-server`、既定のコマンドは `/data 0.0.0.0:50051 0.0.0.0:50080` です。つまり `kasu-server <kasu-directory> <grpc-address> <http-address>` の形です。イメージ名の後ろに書いた引数は既定のコマンドを置き換えるため、引数を足す場合は 3 つの値を残してください。そのほかの設定はすべて環境変数で行います。詳細は[設定リファレンス](#設定リファレンス)のページを参照してください。

`kasu-ingest` も同じイメージに入っています。エントリポイントを上書きして実行します。例: `docker run --rm --entrypoint kasu-ingest ... --create /source.git /data my-repo`。

### データとユーザー

- `/data` はボリュームです。ローカルストレージでは `meta.db`(SQLite)と `objects/` が入ります。PostgreSQL **と** S3 の両方を設定した場合、サーバーは `/data` を作成も使用もしません。
- 名前付きボリュームはイメージの所有者(UID 10001)で初期化されるので、そのまま使えます。バインドマウントは UID 10001 が書き込める必要があります: `sudo chown 10001:10001 /srv/kasu/data`。
- 設定や資格情報のためにマウントするファイルは、UID 10001 が読めれば十分です。

## すべてのデプロイで守ること

**認証は既定で無効です。** `KASU_TOKEN`、`KASU_TOKEN_FILE`、`KASU_ACCOUNTS_FILE`、`KASU_PROJECTION_ACCOUNTS_FILE` のいずれも設定していないサーバーは、50080 または 50051 に到達できる誰でも、すべてのリポジトリを読み書きできます。手元の PC なら問題ありませんが、それ以外では使えません。そのようなサーバーはループバックだけに公開してください(`ports:` に `127.0.0.1:50080:50080` と書く)。[リポジトリアカウント](../repository-accounts_ja.md)を参照してください。

**秘密情報はファイルで渡します。** 資格情報にはそれぞれ `*_FILE` 形式があり、ファイルは起動時に 1 度だけ読まれます。`KASU_TOKEN_FILE`、`KASU_ACCOUNTS_FILE`(中の `token_file` は JSON ファイルからの相対パスか絶対パス)、`KASU_BINDING_KEY_FILE`、`KASU_POSTGRES_DSN_FILE`、`KASU_WEB_SESSION_KEY_FILE`、`KASU_WEB_DELEGATE_TOKEN_FILE`、`KASU_OBJECT_TOKEN_FILE` です。`docker inspect` に値が出てしまう `-e KASU_TOKEN=...` より、ファイルを使ってください。ファイルを変更した場合は、コンテナを再起動すると反映されます。

トークンは 4096 バイト以下の不透明な文字列です。`openssl rand -hex 32` で生成してください。鍵(`session.key`、`binding.key`)は 32 バイト以上が必要です。

**TLS は組み込まれていません。** `kasu-server` と `kasu-web` は平文の HTTP と gRPC を話します。前段で TLS を終端し(例では [Caddy](https://caddyserver.com/) と nginx を使います)、公開するのはプロキシのポートだけにしてください。クライアントは `https://` の `KASU_SERVER` URL で gRPC ポートに接続します(CLI と Web UI は WebPKI のルートで証明書を検証します)。そのため証明書は公的な CA が発行したものにするか、クライアントの信頼パスに自前の CA を入れる必要があります。PostgreSQL への接続は既定で TLS です。[PostgreSQL と S3](../production-storage_ja.md)を参照してください。

**サーバーから見えるのはプロキシのアドレスで、クライアントのアドレスではありません。** admission 制御はソケットのピアアドレスで判定し、`Forwarded` / `X-Forwarded-For` は無視します。そのためリバースプロキシの背後にあるクライアントは、すべて 1 つの IP 単位クォータを共有します。`KASU_SERVER_LIMITS_FILE` で `client_rate_per_second`、`client_burst`、`per_identity_active` を引き上げてください([サーバー制限](../server-runtime_ja.md))。Web UI は例外で、ブラウザのアドレスをヘッダーで転送し、サーバーは検証済みの delegate からのヘッダーだけを信頼します。

**delegate はネットワークに固定します。** サーバーは Web UI の delegate トークンを `trusted_peers`(CIDR)からの接続でしか受け付けません。Docker では 2 つのコンテナが共有するネットワークのサブネットがそれにあたるため、例では Compose のネットワークに固定のサブネットを割り当てています。`kasu-web` とサーバーの間にロードバランサーを置く場合、サーバーから見えるのはロードバランサーのアドレスです。

**ヘルスチェック。** イメージには `bash` はありますが `curl` はありません。`kasu-web` は `GET /-/healthz` と `GET /-/readyz`(後者は `kasu-server` に到達できるかも確認)を提供します。`kasu-server` には認証なしのヘルスチェック用エンドポイントがなく、`GET /metrics` にはオペレーター PAT が必要です。そのため例では TCP ポートで確認します。どちらも bash の `/dev/tcp` で書いています。

```sh
# kasu-server: HTTP ポートが接続を受け付けるか
bash -c 'exec 3<>/dev/tcp/127.0.0.1/50080'
# kasu-web: ready か(サーバーに到達できるか)
bash -c 'exec 3<>/dev/tcp/127.0.0.1/50090 && printf "GET /-/readyz HTTP/1.0\r\n\r\n" >&3 && head -n1 <&3 | grep -q " 200 "'
```

**ログとトレース** は stderr に出力され(`docker logs`)、`RUST_LOG` で絞り込めます。`OTEL_TRACES_EXPORTER=otlp` と `OTEL_EXPORTER_OTLP_ENDPOINT` を設定するとスパンをエクスポートします([トレース](../telemetry_ja.md))。メトリクスは、サーバーの `GET /metrics`(オペレーター PAT が必要)と、Web UI の `GET /-/metrics`(`KASU_WEB_METRICS_TOKEN_FILE`)が Prometheus テキスト形式で提供します。

**グレースフルシャットダウン。** `kasu-server` と `kasu-web` が処理するのは SIGINT だけです。`docker stop` は既定で SIGTERM を送るため、猶予時間の後に強制終了されます。`stop_signal: SIGINT`(Compose)または `--stop-signal SIGINT`(`docker run`)を設定して、Webhook 配信などのバックグラウンド処理を正常に停止させてください。例はすべてこの設定です。

## 運用

- **アップグレード** はイメージタグを変更してコンテナを作り直します。先にバックアップを取ってください。新しいバージョンが PostgreSQL のスキーマバージョンを上げる場合、古いサーバーは新しいスキーマを受け付けないので、最初の新サーバーを起動する前に古いサーバーをすべて停止します。[PostgreSQL の保存契約](../production-storage_ja.md#postgresql-の保存契約)を参照してください。つまりローリングアップグレードが安全なのは、スキーマバージョンが同じリリース間だけです。リリースノートで確認してください。
- **バックアップ** は正本のデータに対して行います。ローカルストレージなら `/data` ボリューム、共有ストレージなら PostgreSQL データベースと S3 のバケット/プレフィックスを一緒に取ります。kasu は保存したオブジェクトを削除せず、データベースの行は S3 のオブジェクトを参照するため、復元は両方を同じ時点にそろえてください。Web UI にはステートがありません。
- **設定変更**: `KASU_REPOSITORIES_FILE` は SIGHUP で、また 30 秒ごとに再読み込みされます(`docker kill -s HUP <container>`)。アカウント、トークン、書き込みポリシーは起動時にしか読まれないため、サーバーを再起動してください。
- **リポジトリのインポート** は、サーバーと同じストレージに対して `kasu-ingest` で行います。各ガイドにコマンドがあります。

## 設定リファレンス

| トピック | ページ |
|---|---|
| オペレータートークン、アカウント、delegate | [リポジトリアカウント](../repository-accounts_ja.md) |
| 複数リポジトリ、`KASU_REPOSITORIES_FILE`、Admin API | [複数リポジトリ](../multi-repository_ja.md) |
| PostgreSQL と S3 の変数 | [PostgreSQL / S3 と複数サーバー](../production-storage_ja.md) |
| 同時実行数とレート制限、メトリクス | [サーバー制限とメトリクス](../server-runtime_ja.md) |
| Web UI の変数と `web.json` | [Web UI](../web-ui_ja.md) |
| Argo CD / Flux への Webhook | [Webhook](../webhooks_ja.md) |
| Git push のポリシー | [Git push](../git-push_ja.md) |
| Git 履歴のインポート | [Git インポート](../git-import_ja.md) |
| トレース | [OpenTelemetry](../telemetry_ja.md) |
| キャッシュと daemon の階層 | [キャッシュ階層](../cache-hierarchy_ja.md)、[ディスクキャッシュ](../disk-cache_ja.md) |
