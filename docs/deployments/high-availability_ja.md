# 高可用性デプロイ

[English](high-availability.md) | 日本語

複数の `kasu-server` ノードをロードバランサーの背後に置き、1 つの PostgreSQL データベースと 1 つの S3 互換バケットを共有する構成です。どのノードでもどのリクエストでも処理できるため、ほかのノードが処理を続けている間に、1 つのノードが故障したり、アップグレードされたりできます。このガイドが扱うのはコンテナまでです。PostgreSQL、オブジェクトストア、ロードバランサー自体の運用は利用者が行います。

先に [Docker による kasu のデプロイ](README_ja.md)を読んでください。このガイドが再利用するアカウントと資格情報のファイルについては[1 ノード構成](single-node_ja.md)も参照してください。

## この構成で得られるものと、得られないもの

- **サーバーはステートレスです。** すべての状態は PostgreSQL(メタデータ、ref、ログ、Land の冪等性台帳、Webhook のカーソル)と S3(不変オブジェクト)にあります。両方を設定した場合、サーバーの `/data` ディレクトリは作成も使用もされません([保存契約](../production-storage_ja.md))。
- **書き込みは競合しません。** ref の更新は PostgreSQL 上の compare-and-swap トランザクションなので、2 台のサーバーが同じブランチを同時に更新しようとしても勝者は 1 つだけです。[Land](../land_ja.md) の結果は共有されるため、レスポンスが失われた Land は、同じキーでどのノードにも再送できます。
- **可用性は構成要素の中で最も低いものに等しくなります。** kasu は PostgreSQL も S3 も複製しません。高可用な PostgreSQL(マネージドサービス、Patroni など)と耐久性のある S3 を使ってください。データベースが止まれば、すべてのサーバーが利用できなくなります。
- **検証範囲。** 共有ストレージのテストは、2 つのサーバープロセスと実際の `kasu-ingest` を、PostgreSQL 18 と SeaweedFS に対して実行します([本番ストレージ](../production-storage_ja.md#実通信の検証))。実際の AWS IAM/KMS やネットワーク障害、地理的に分散した構成、容量は対象外です。自分の環境で負荷試験と障害のリハーサルを行ってください。
- **制限はプロセスごとです。** admission 制御とレート制限は、クラスター全体ではなく各サーバープロセスの中で適用されます。サーバーが N 台なら、実質の上限は設定値のおよそ N 倍です([サーバー制限](../server-runtime_ja.md))。
- **サーバープロセスごとに PostgreSQL 接続は 1 本です。** PostgreSQL ストアは同期接続を 1 本使い回し、プロセス内では直列化されます。障害後は再接続しますが、失敗した SQL 文を自分で再試行することはありません([PostgreSQL の保存契約](../production-storage_ja.md#postgresql-の保存契約))。スケールはサーバーの追加で行います。`max_connections` は、サーバー数と自前のツールの分を見込んで決めてください。
- **Webhook は at-least-once です。** すべてのサーバーが Webhook ディスパッチャーを動かし、カーソルは PostgreSQL にあります。フェイルオーバー後に、同一の本文で配信が繰り返されることがあるため、受信側は重複を許容する必要があります(Argo CD と Flux の Webhook はリフレッシュを起こすだけです)。

## トポロジー

```text
                    ┌──────────────── load balancer pair (TLS, VIP or cloud LB) ─────────────────┐
 Git / JSON ── 443 ─┤ git.example.com:443    → node*:50080                                       │
 kasu CLI ── 50051 ─┤ git.example.com:50051  → node*:50051   (HTTP/2, gRPC)                      │
 browsers ─── 443 ──┤ kasu.example.com:443   → node*:50090                                       │
                    └────────────────────────────────────────────────────────────────────────────┘
                         │                         │                         │
                 ┌───────┴───────┐         ┌───────┴───────┐         ┌───────┴───────┐
                 │ node1         │         │ node2         │         │ node3         │
                 │ server + web  │         │ server + web  │         │ server + web  │
                 └───────┬───────┘         └───────┬───────┘         └───────┬───────┘
                         └──────────────┬──────────┴──────────────┬──────────┘
                                        ▼                         ▼
                        PostgreSQL (HA, TLS)          S3 bucket (conditional writes)
```

各ノードで `server` と `web` のコンテナを 1 つずつ動かし、Web UI は同じノードのサーバーと通信します。そのサーバーが故障すると、そのノードの Web UI は readiness チェックに失敗し、ロードバランサーはそのノードを使わなくなります。可用性のための最小構成は 2 ノードです。3 ノードなら、あるノードのメンテナンス中にもう 1 つ故障しても耐えられます。

## 前提条件

| 構成要素 | 要件 |
|---|---|
| PostgreSQL | すべてのノードから TLS で到達できること。`kasu` スキーマを作成できる専用のデータベースとロール。18 で検証済み |
| S3 | 事前に作成したバケット。[条件付き書き込み](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)(`If-None-Match: *`)と read-after-write 一貫性をサポートしていること。kasu は無条件の上書きにフォールバックしないためです。バケットとプレフィックスに対する `GetObject` と `PutObject` を許可する資格情報 |
| ロードバランサー | TLS を終端し、50080 には HTTP/1.1、50051 には **HTTP/2(gRPC)** を転送できること。長時間のストリーミングレスポンス(Git パック、Objects)に対応していること |
| ホスト | 各ノードに Docker と Compose プラグイン。時刻同期。同一の設定ファイルとシークレットファイルをすべてのノードへ配布する手段 |
| ネットワーク | ノードから PostgreSQL と S3 に到達できること。ロードバランサーからノードの 50051、50080、50090 に到達できること。それ以外は公開しないこと |

## すべてのノードで同一にするもの

サーバー同士は共有ストアを通じる以外には調整しません。そのため次の項目はすべてのノードで同じ値でなければならず、食い違うと特定のノードでだけユーザーが拒否される形で現れます。

| 項目 | 理由 |
|---|---|
| PostgreSQL のデータベース、S3 のバケット、`KASU_S3_PREFIX` | 共有される状態そのものです |
| オペレータートークン、`accounts.json`、それが参照する PAT ファイル | 資格情報は起動時にローカルのファイルと照合されます |
| `KASU_BINDING_KEY_FILE` | Web セッションはこの鍵で PAT に結び付けられます。ノードごとに鍵が違うと、セッションが 1 つのノードでしか使えません |
| `KASU_WEB_SESSION_KEY_FILE`(Web) | セッション Cookie と CSRF トークンはどのレプリカでも検証されます |
| `KASU_WRITE_POLICY_FILE`、`KASU_SERVER_LIMITS_FILE`、`KASU_REPOSITORIES_FILE` | ポリシーとクォータです |
| `KASU_OBJECT_TOKEN_FILE`、`KASU_OBJECT_REPOSITORIES` | [キャッシュ階層](#オプションのキャッシュ階層)を使う場合のみ |

資格情報と設定ファイルは起動時にしか読まれません。変更するには、すべてのノードのファイルを更新し、サーバーを 1 台ずつ再起動します。例外は `KASU_REPOSITORIES_FILE` で、SIGHUP と定期的な間隔で再読み込みされ、レジストリは PostgreSQL にあるため、2 つのノードが同じファイルを適用しても同じ状態に収束します。

## ノードの設定

すべてのノードで同じ Compose プロジェクトを動かします。違うのは `NODE_IP`(ロードバランサーがそのノードに到達するアドレス)だけです。[1 ノード構成](single-node_ja.md#ディレクトリ構成)のファイル構成を使い、次の変更を加えます。

- `secrets/` に `postgres-dsn` と `pg-ca.pem` を追加します。
- S3 の資格情報は `s3.env`(権限 0600、所有者 root)に置くか、インスタンスロールを使います。
- Docker のサブネットはどのノードでも同じなので、`accounts.json` の `trusted_peers` はどのノードでも同じものを使えます。

`secrets/postgres-dsn`(1 行。64 KiB 以下。パスワードも含むため、ファイルを保護してください):

```text
host=pg.internal.example.com port=5432 dbname=kasu user=kasu password=CHANGE-ME sslmode=require
```

PostgreSQL への TLS は、WebPKI のルートとホスト名で検証されます。プライベート CA の場合は、その証明書をマウントして `KASU_POSTGRES_CA_FILE` を設定します。`sslmode=prefer` は `require` に引き上げられます。平文にするには 2 つの明示的なオプトインが必要で、[ローカルのリハーサル](#1-台のマシンでのリハーサル)専用です。

`s3.env`:

```text
AWS_ACCESS_KEY_ID=...
AWS_SECRET_ACCESS_KEY=...
```

それ以外の資格情報は、標準の AWS プロバイダー(セッショントークン、web identity、ECS/EC2 のロールプロバイダー)から取得されます。IMDSv2 を使う EC2 では、コンテナがインスタンスロールに到達するためにメタデータのホップ制限を 2 にする必要があります。付与する権限は `arn:aws:s3:::BUCKET/PREFIX/*` に対する `s3:GetObject` と `s3:PutObject` だけにしてください。kasu はオブジェクトを削除せず、その権限も与えるべきではありません。S3 互換サービスでは `KASU_S3_ENDPOINT` を設定し、必要なら `KASU_S3_PATH_STYLE=true` も設定します。

`compose.yaml`(全ノード共通。`NODE_IP=10.0.1.11 docker compose up -d` のように起動します):

```yaml
name: kasu

services:
  server:
    image: ghcr.io/sileader/kasu/server:0.1.0
    restart: unless-stopped
    stop_signal: SIGINT
    ports:
      - "${NODE_IP:?set NODE_IP to this node's private address}:50051:50051"
      - "${NODE_IP}:50080:50080"
    environment:
      KASU_POSTGRES_DSN_FILE: /run/secrets/postgres-dsn
      KASU_POSTGRES_CA_FILE: /etc/kasu/pg-ca.pem        # プライベート CA の場合のみ
      KASU_S3_BUCKET: kasu-prod
      KASU_S3_REGION: ap-northeast-1
      KASU_S3_PREFIX: kasu/objects/v1
      KASU_TOKEN_FILE: /run/secrets/operator-pat
      KASU_ACCOUNTS_FILE: /etc/kasu/accounts.json
      KASU_BINDING_KEY_FILE: /run/secrets/binding-key
      KASU_SERVER_LIMITS_FILE: /etc/kasu/limits.json
      KASU_PUBLIC_URL: https://git.example.com
      RUST_LOG: info
    env_file: ./secrets/s3.env
    volumes:
      - ./config/accounts.json:/etc/kasu/accounts.json:ro
      - ./config/limits.json:/etc/kasu/limits.json:ro
      - ./secrets/pg-ca.pem:/etc/kasu/pg-ca.pem:ro
    secrets: [postgres-dsn, operator-pat, alice-pat, argocd-pat, kasu-web-pat, binding-key]
    networks: [kasu]
    healthcheck:
      test: ["CMD", "bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/50080"]
      interval: 10s
      timeout: 3s
      retries: 5

  web:
    image: ghcr.io/sileader/kasu/web:0.1.0
    restart: unless-stopped
    stop_signal: SIGINT
    ports:
      - "${NODE_IP}:50090:50090"
    depends_on:
      server:
        condition: service_healthy
    environment:
      KASU_WEB_SERVER: http://server:50051
      KASU_WEB_PUBLIC_URL: https://kasu.example.com
      KASU_WEB_DELEGATE_TOKEN_FILE: /run/secrets/kasu-web-pat
      KASU_WEB_SESSION_KEY_FILE: /run/secrets/web-session-key
      KASU_WEB_CONFIG_FILE: /etc/kasu-web/web.json
      RUST_LOG: info
    volumes:
      - ./config/web.json:/etc/kasu-web/web.json:ro
    secrets: [kasu-web-pat, web-session-key]
    networks:
      kasu:
        ipv4_address: 172.28.0.20
    healthcheck:
      test: ["CMD", "bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/50090 && printf 'GET /-/readyz HTTP/1.0\\r\\n\\r\\n' >&3 && head -n1 <&3 | grep -q ' 200 '"]
      interval: 10s
      timeout: 3s
      retries: 5

networks:
  kasu:
    ipam:
      config:
        - subnet: 172.28.0.0/24

secrets:
  postgres-dsn: { file: ./secrets/postgres-dsn }
  operator-pat: { file: ./secrets/operator-pat }
  alice-pat: { file: ./secrets/alice-pat }
  argocd-pat: { file: ./secrets/argocd-pat }
  kasu-web-pat: { file: ./secrets/kasu-web-pat }
  binding-key: { file: ./secrets/binding-key }
  web-session-key: { file: ./secrets/web-session-key }
```

`config/accounts.json` は[1 ノード構成](single-node_ja.md#設定ファイル)のものと同じです(Web UI は自分のノードのサーバーへ直接接続するので、delegate の `trusted_peers` は `172.28.0.20/32` です)。`config/web.json` には Caddy がないので、`trusted_forwarders` にはロードバランサーのアドレスを設定します。そうしないと、すべてのブラウザがロードバランサーから来たように見え、ログインのレート制限が共有されてしまいます。シークレットのファイルと `limits.json` は、すべてのノードに同一のものが必要です。すべてのクライアントがロードバランサーのアドレスから来るので、`limits.json` には 1 ノード構成のガイドと同様に引き上げたクォータが必要です。

サーバーと Web UI は `NODE_IP` にだけ公開します。TLS がないので、パブリックなインターフェースには公開しないでください。

### 設定の配布

`config/` と `secrets/` は構成管理(Ansible や、ファイルを生成するシークレット管理など)で保管し、イメージタグを変更する前にすべてのノードへ適用します。シークレットや `accounts.json` の変更を反映するには、ローリング再起動を行います。

## ロードバランサー

gRPC ポートに HTTP/2 を話せる L7 ロードバランサーなら使えます。次の nginx の設定(`http2 on` のために nginx 1.25.1 以降)は出発点です。2 台のホストで浮動アドレス(keepalived/VRRP)とともに動かすか、代わりにクラウドのロードバランサーを使ってください。

```nginx
events {}

http {
    upstream kasu_http {
        server 10.0.1.11:50080 max_fails=2 fail_timeout=5s;
        server 10.0.1.12:50080 max_fails=2 fail_timeout=5s;
        server 10.0.1.13:50080 max_fails=2 fail_timeout=5s;
        keepalive 32;
    }
    upstream kasu_grpc {
        server 10.0.1.11:50051 max_fails=2 fail_timeout=5s;
        server 10.0.1.12:50051 max_fails=2 fail_timeout=5s;
        server 10.0.1.13:50051 max_fails=2 fail_timeout=5s;
        keepalive 32;
    }
    upstream kasu_web {
        server 10.0.1.11:50090 max_fails=2 fail_timeout=5s;
        server 10.0.1.12:50090 max_fails=2 fail_timeout=5s;
        server 10.0.1.13:50090 max_fails=2 fail_timeout=5s;
        keepalive 16;
    }

    # Git Smart HTTP と JSON API
    server {
        listen 443 ssl;
        http2 on;
        server_name git.example.com;
        ssl_certificate     /etc/nginx/tls/git.example.com.pem;
        ssl_certificate_key /etc/nginx/tls/git.example.com.key;

        client_max_body_size 0;
        proxy_request_buffering off;
        proxy_buffering off;
        location / {
            proxy_pass http://kasu_http;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_set_header Host $host;
            proxy_connect_timeout 2s;
            proxy_read_timeout 600s;
            proxy_send_timeout 600s;
            proxy_next_upstream error timeout;   # 接続失敗のみ。送信を始めたリクエストは再送しない
        }
    }

    # ネイティブ CLI 用の gRPC: KASU_SERVER=https://git.example.com:50051
    server {
        listen 50051 ssl;
        http2 on;
        server_name git.example.com;
        ssl_certificate     /etc/nginx/tls/git.example.com.pem;
        ssl_certificate_key /etc/nginx/tls/git.example.com.key;

        client_max_body_size 0;
        location / {
            grpc_pass grpc://kasu_grpc;
            grpc_connect_timeout 2s;
            grpc_read_timeout 600s;
            grpc_send_timeout 600s;
            grpc_next_upstream error timeout;
        }
    }

    # Web UI
    server {
        listen 443 ssl;
        http2 on;
        server_name kasu.example.com;
        ssl_certificate     /etc/nginx/tls/kasu.example.com.pem;
        ssl_certificate_key /etc/nginx/tls/kasu.example.com.key;

        location / {
            proxy_pass http://kasu_web;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_set_header Host $host;
            proxy_set_header X-Forwarded-For $remote_addr;   # 上書きする: Web UI は右端のエントリを信頼する
            proxy_connect_timeout 2s;
        }
    }
}
```

nginx をコンテナで動かしても構いません。例: `docker run -d --restart unless-stopped --network host -v /etc/nginx-kasu/nginx.conf:/etc/nginx/nginx.conf:ro -v /etc/nginx-kasu/tls:/etc/nginx/tls:ro nginx:1.27`。

補足:

- **ヘルスチェック。** オープンソース版の nginx は障害をパッシブにしか検出しません(`max_fails`)。アクティブなチェックには HAProxy、Envoy、クラウドのロードバランサーを使い、サーバーは 50080/50051 への TCP チェック、Web UI は 50090 の `GET /-/readyz`(HTTP 200)でチェックします。Web UI が ready になるのは、自分のノードのサーバーに到達できる間だけです。
- サーバーに**スティッキーネスは不要**です。Web UI のセッションは署名付き Cookie で、どのノードでも有効です。
- **リトライ。** 別のノードで再試行するのは接続レベルの失敗だけにしてください。すでに送信したリクエストをロードバランサーに再送させないでください。書き込みは盲目的に繰り返すと安全でない場合があります。クライアントは、自身の冪等性の仕組み(後述)で正しく繰り返せます。
- ロードバランサーの**アイドルタイムアウト**は、想定する最長の Git パックや Objects の転送より長くします。
- **メトリクス**(`/metrics`)はプロセスごとです。ロードバランサー経由ではなく、各ノードのアドレスのポート 50080 を、オペレーター PAT で直接スクレイプしてください。
- **アドレス。** サーバーから見るとすべてのクライアントがロードバランサーのアドレスなので、IP ごとのクォータはすべてのユーザーで共有されます。負荷試験の後に `limits.json` を調整してください([サーバー制限](../server-runtime_ja.md))。

## 初回起動とリポジトリのインポート

1. 最初のノードを単独で `docker compose up -d` で起動します。起動時に、`kasu` スキーマが advisory lock のもとで PostgreSQL に初期化されるので、全ノードを同時に起動しても安全ですが、1 ノードだけなら共有設定の誤りによるエラーを読みやすくなります。
2. `docker compose logs server` を確認します。PostgreSQL や S3 を設定した場合に kasu がローカルストレージへフォールバックすることはないので、DSN やバケットが間違っていれば、起動がエラーで失敗します。
3. ほかのノードを起動します。
4. リポジトリを作成またはインポートします。共有ストレージなら、サーバーを動かしたまま、任意のノードまたは同じ変数を持つ管理用ホストから実行できます。

```sh
docker compose run --rm --no-deps \
  -v /path/to/infra.git:/source.git:ro \
  --entrypoint kasu-ingest server \
  --create /source.git /data team-a/infra
```

`kasu-ingest` はサーバーと同じ `KASU_POSTGRES_*` と `KASU_S3_*` の変数を使い(`server` サービスが供給します)、共有ストアに書き込みます。`/data` 引数はコマンドラインの形式上必要ですが使われません。リポジトリは、すべてのノードですぐに見えるようになります。[Git インポート](../git-import_ja.md)を参照してください。

ロードバランサー経由と、各ノードへ直接の両方で読み取って確認します。

```sh
export KASU_TOKEN="$(sudo cat secrets/operator-pat)"
for h in https://git.example.com http://10.0.1.11:50080 http://10.0.1.12:50080; do
  git -c http.extraHeader="Authorization: Bearer $KASU_TOKEN" ls-remote "$h/team-a/infra.git" | head -1
done
```

## ローリングアップグレード

新しいリリースが PostgreSQL のスキーマバージョンを変更するかどうかで、アップグレードは 2 種類に分かれます。リリースノートに書かれています。変更する場合、古いバージョンのサーバーは新しいスキーマを拒否するので、バージョンを混在させることはできません。

**互換リリース(スキーマバージョンが同じ)。** 1 ノードずつ行います。

1. ノードをロードバランサーから外します(または先に Web UI を止めて、ヘルスチェックに外させます)。
2. そのノードの `compose.yaml` のタグを変更し、`docker compose pull && docker compose up -d` を実行します。
3. `docker compose ps` が `healthy` を示すまで待ち、戻します。
4. 次のノードで繰り返します。その間、古いサーバーと新しいサーバーが並んで動きます。

**スキーマを変更するリリース。** 短い停止を計画してください。すべてのサーバーと Web UI を止め、PostgreSQL のバックアップを取り、1 ノードを新しいバージョンで起動し(ロックのもとでスキーマをアップグレードします。何も削除はしません)、確認してから、残りを起動します。古いサーバーを動かしたままにしないでください。新しいスキーマを拒否します。

どちらの場合も、先に PostgreSQL と S3 のバックアップを取ってください。

## 障害時の挙動

| 障害 | 挙動 | クライアントの対応 |
|---|---|---|
| サーバーのコンテナやノードが停止する | ロードバランサーが迂回します。そのノードで処理中のリクエストは失敗します | リトライします。読み取りは安全です。ネイティブの書き込みは、再試行の前に ref(compare-and-swap)を確認します。[Land](../land_ja.md) は同じ冪等性キーで再送します。`git push` はもう一度実行します |
| PostgreSQL がフェイルオーバーする | プライマリが使えない間、リクエストは失敗します。各サーバーは次のリクエストで再接続し、失敗した SQL 文は内部で再試行されません | 同上 |
| S3 が使えない | オブジェクトのデータが必要なリクエストは失敗します(1 操作あたり全体で 15 秒のデッドライン、内部リトライは最大 2 回)。ローカルへのフォールバックは一切ありません | 後でリトライします |
| リクエストが処理中にキャンセルされる | ref とログは 1 つのトランザクションで更新され、ロールバックされます。参照されない不変オブジェクトが S3 に残ることがあります | リトライします |
| Web UI のノードが停止する | ほかのノードが同じセッション Cookie を受け付けます | リロードします |
| ロードバランサーのノードが停止する | 浮動アドレスがペアのノードへ移ります(keepalived/VRRP またはクラウド LB) | 再接続します |

## バックアップと復旧

- **PostgreSQL**: 基盤の継続的アーカイブやスナップショット(ポイントインタイムリカバリ)を使います。ref とすべてのインデックスがここにあります。
- **S3**: 要件に応じてバージョニングやレプリケーションを有効にします。kasu 自身はオブジェクトを削除しないので、kasu のプレフィックスにライフサイクルルールをかけないでください。
- **整合性**: S3 のオブジェクトは、それを指す ref より先に書き込まれます。時刻 *T* に復元したデータベースには、少なくとも *T* 時点のバケットが必要です。バケットにそれより新しいオブジェクトが余分にあっても害はありません。データベースより前の時点へバケットを戻してはいけません。
- **設定とシークレット**: 構成管理のリポジトリやシークレットマネージャーに保管します。

完全な復旧を、検証用の環境へ少なくとも 1 度はリハーサルしてください。

## オプションのキャッシュ階層

ほかのリージョンのクライアントや大規模な CI では、[`kasu-cache`](../cache-hierarchy_ja.md)(リージョンのメモリ LRU)と `kasu-daemon`(Unix ソケットを持つノードごとのディスクキャッシュ)が、読み取り専用のオブジェクト取得をクライアントの近くで処理します。どちらも公開イメージには含まれません。ソースからビルドする(`cargo build --release -p kasu-cache -p kasu-daemon`)か、自前のイメージにまとめてください。CLI は Unix ソケット経由で daemon に接続するため、daemon はチェックアウトを行うマシン上で直接動かす想定です。

この階層を導入する場合は、すべてのサーバーに同じ `KASU_OBJECT_TOKEN_FILE` を設定します。この変数を設定したサーバーは、通常のユーザー PAT を持つ Objects リクエストを拒否するので、オブジェクトを取得するすべてのクライアントは、hop トークンを持つ daemon かキャッシュを経由する必要があります。サーバー側だけでなくクライアントの経路が変わるため、慎重に展開してください。

## 1 台のマシンでのリハーサル

PostgreSQL や S3 のアカウントなしでこのトポロジーを試すには、1 台のホストで、ローカルの PostgreSQL と SeaweedFS(S3 互換)とともにサーバーを 2 台動かします。リポジトリの[ストレージテスト](../production-storage_ja.md#実通信の検証)が使うものと同じ、固定された 2 つのイメージです。あくまでリハーサルです。平文の接続、公開されている固定のテスト用資格情報、誰でも知っているトークンを使うので、本番のひな形にはなりません。

`compose.yaml`:

```yaml
name: kasu-ha

x-server: &server
  image: ghcr.io/sileader/kasu/server:0.1.0
  stop_signal: SIGINT
  depends_on:
    postgres: { condition: service_healthy }
  environment:
    KASU_TOKEN: dev-token
    KASU_POSTGRES_DSN_FILE: /etc/kasu/postgres-dsn
    KASU_POSTGRES_ALLOW_PLAINTEXT: "true"     # DSN の sslmode=disable と組み合わせる
    KASU_S3_BUCKET: kasu-rehearsal
    KASU_S3_REGION: us-east-1
    KASU_S3_ENDPOINT: http://s3:8333
    KASU_S3_PATH_STYLE: "true"
    KASU_S3_ALLOW_PLAINTEXT: "true"
    AWS_ACCESS_KEY_ID: kasu-test
    AWS_SECRET_ACCESS_KEY: kasu-local-test-secret
  configs:
    - source: postgres-dsn
      target: /etc/kasu/postgres-dsn

services:
  postgres:
    image: postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280
    environment:
      POSTGRES_USER: kasu
      POSTGRES_PASSWORD: kasu-local-test
      POSTGRES_DB: kasu
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U kasu -d kasu"]
      interval: 1s
      timeout: 3s
      retries: 30

  s3:
    image: chrislusf/seaweedfs@sha256:fc9f76fa993ad69966ffeb2f65d0318fcae39c6f8e20cf68ef7b3a5cb97769e5
    command: ["server", "-s3", "-ip=127.0.0.1", "-ip.bind=0.0.0.0", "-master.volumeSizeLimitMB=128", "-master.telemetry=false", "-volume.max=16", "-s3.config=/config/s3.json"]
    ports:
      - "127.0.0.1:58333:8333"
    configs:
      - source: s3-identities
        target: /config/s3.json

  server1:
    <<: *server
    ports: ["127.0.0.1:50051:50051", "127.0.0.1:50080:50080"]
  server2:
    <<: *server
    ports: ["127.0.0.1:51051:50051", "127.0.0.1:51080:50080"]

  lb:
    image: nginx:1.27
    depends_on: [server1, server2]
    ports:
      - "127.0.0.1:8080:8080"     # ロードバランサー経由の HTTP/Git
      - "127.0.0.1:8051:8051"     # ロードバランサー経由の gRPC
    configs:
      - source: nginx
        target: /etc/nginx/nginx.conf

configs:
  postgres-dsn:
    content: "host=postgres port=5432 dbname=kasu user=kasu password=kasu-local-test sslmode=disable"
  s3-identities:
    content: |
      {"identities":[{"name":"kasu-local-test","credentials":[{"accessKey":"kasu-test","secretKey":"kasu-local-test-secret"}],"actions":["Admin","Read","Write","List","Tagging"]}]}
  nginx:
    content: |
      events {}
      http {
        upstream http { server server1:50080 max_fails=1 fail_timeout=10s; server server2:50080 max_fails=1 fail_timeout=10s; }
        upstream grpc { server server1:50051 max_fails=1 fail_timeout=10s; server server2:50051 max_fails=1 fail_timeout=10s; }
        server {
          listen 8080;
          client_max_body_size 0;
          proxy_request_buffering off;
          proxy_buffering off;
          proxy_connect_timeout 2s;
          location / { proxy_pass http://http; proxy_http_version 1.1; proxy_set_header Connection ""; }
        }
        server {
          listen 8051;
          http2 on;
          grpc_connect_timeout 2s;
          location / { grpc_pass grpc://grpc; }
        }
      }
```

ストアを起動し、リポジトリのテストハーネスと同じ方法でバケットを作成してから、サーバーを起動します。

```sh
docker compose up -d --wait postgres s3

until curl -sf --max-time 2 --aws-sigv4 'aws:amz:us-east-1:s3' \
      --user kasu-test:kasu-local-test-secret -X PUT http://127.0.0.1:58333/kasu-rehearsal; do sleep 1; done
docker compose exec -T s3 sh -c \
  'printf "fs.configure -locationPrefix=/buckets/ -volumeGrowthCount=1 -apply\n" | weed shell -master=127.0.0.1:9333 -filer=127.0.0.1:8888'

docker compose up -d
```

(2 つ目のコマンドは、バケットのために SeaweedFS が拡張するボリューム数を制限します。ここで使っている小さな `-volume.max` では必要です。)両方のサーバーが動いている間に、どちらか一方を通してリポジトリをインポートし、ロードバランサーと各サーバー経由で読み取ります。

```sh
docker compose run --rm --no-deps \
  -v /path/to/source.git:/source.git:ro \
  --entrypoint kasu-ingest server1 \
  --create /source.git /data my-repo

for p in 8080 50080 51080; do
  git -c http.extraHeader='Authorization: Bearer dev-token' ls-remote http://127.0.0.1:$p/my-repo.git | head -1
done
```

次に障害を試します。`docker compose stop server1` を実行し、ポート 8080 経由で読み取りを続けます(nginx は接続タイムアウトの 2 秒後に `server1` をあきらめ、`server2` で再試行し、次の 10 秒間は `server1` を使いません)。`docker compose start server1` で復帰し、修復すべき状態はありません。後片付けは `docker compose down -v` です。

## 他のスケジューラー

同じイメージと環境変数は、Docker Swarm、Nomad、Kubernetes でも使えます。共有する PostgreSQL と S3 の設定でサーバーイメージのレプリカを N 個動かし、上記のファイルをシークレットとしてマウントし、ヘルスチェックは TCP ポートで行い、停止シグナルを SIGINT にします。このガイドが扱うのは Compose だけです。
