# 1 ノード構成のデプロイ

[English](single-node.md) | 日本語

1 台のマシンで、1 台の `kasu-server`、Web UI、TLS リバースプロキシを動かし、認証を有効にする構成です。データは Docker ボリューム(SQLite とローカルのオブジェクトファイル)に置きます。小規模なチーム、検証環境、より大きな構成へ段階的に移行する最初の段階に向いています。冗長性はなく、ホストが単一障害点で、アップグレード時にはサーバーが再起動します。それ以上が必要な場合は[高可用性構成](high-availability_ja.md)を参照してください。

イメージ、TLS が組み込まれていないこと、このガイドが前提にしているその他の決まりごとについては、先に [Docker による kasu のデプロイ](README_ja.md)を読んでください。

```text
                    ┌─────────────── Docker network 172.28.0.0/24 ───────────────┐
 Git, JSON ── 443 ──┤               ┌─► server :50080  (Git, JSON APIs)          │
 kasu CLI ── 50051 ─┤ caddy (TLS) ──┼─► server :50051  (gRPC)                    │
 browsers ── 443 ───┤               └─► web    :50090 ── gRPC ──► server :50051  │
                    └────────────────────────────────────────────────────────────┘
                      server data volume: /data
```

例では 3 つの名前を使っています。`git.example.com`(Git、JSON API、50051 番ポートの gRPC)、`kasu.example.com`(Web UI)、Caddy が Let's Encrypt に登録するメールアドレスです。自分の環境の値に置き換えてください。2 つの DNS 名はこのホストに解決される必要があり、証明書の発行のためにポート 80 と 443 に外部から到達できる必要があります。

## ディレクトリ構成

デプロイ用のディレクトリ(例: `/srv/kasu`)を作ります。

```text
/srv/kasu/
├── compose.yaml
├── Caddyfile
├── config/
│   ├── accounts.json
│   ├── limits.json
│   └── web.json
└── secrets/
    ├── operator-pat
    ├── alice-pat
    ├── argocd-pat
    ├── kasu-web-pat
    ├── binding-key
    └── web-session-key
```

## 資格情報

ファイルごとにランダムな値を 1 つ作ります。どれもランダムな文字列で、PAT はユーザーやツールが Bearer トークンまたは Basic パスワードとして提示する値です。

```sh
cd /srv/kasu
mkdir -p config secrets
for n in operator-pat alice-pat argocd-pat kasu-web-pat binding-key web-session-key; do
  openssl rand -hex 32 > "secrets/$n"
done
sudo chown 10001:10001 secrets/*
sudo chmod 0400 secrets/*
```

コンテナは UID 10001 で動作するので、この所有者にするとコンテナ内でファイルが読めます。値を持ち主に渡すときは `sudo cat secrets/alice-pat` で読み出します。

| ファイル | 用途 |
|---|---|
| `operator-pat` | オペレーター資格情報(`KASU_TOKEN_FILE`)。すべてのリポジトリと Admin API を操作できます。管理とブートストラップ用に取っておきます |
| `alice-pat`、`argocd-pat` | [アカウント](../repository-accounts_ja.md)の例。メンテナーと、読み取り専用の GitOps 利用者です |
| `kasu-web-pat` | Web UI の[delegate](../repository-accounts_ja.md)トークン。ほかのどの資格情報とも異なる値にする必要があります |
| `binding-key` | サーバーを再起動しても Web UI のセッションが維持されるようにします(`KASU_BINDING_KEY_FILE`。32 バイト以上) |
| `web-session-key` | Web UI のセッション Cookie と CSRF トークンに署名します(32 バイト以上) |

オペレーター PAT では Web UI にサインインできません。`alice-pat` のようなアカウントの PAT を使います。

## 設定ファイル

`config/accounts.json` は、誰が何をできるかを定義します([リポジトリアカウント](../repository-accounts_ja.md)、[複数リポジトリ](../multi-repository_ja.md))。`token_file` の値は、Compose がシークレットをマウントするコンテナ内のパスです。

```json
{
  "accounts": [
    {
      "name": "alice",
      "token_file": "/run/secrets/alice-pat",
      "display_name": "Alice",
      "email": "alice@example.com",
      "create_repositories": ["team-a/*"],
      "grants": [
        { "repositories": ["**"], "write_policy": {}, "admin": true }
      ]
    },
    {
      "name": "argocd",
      "token_file": "/run/secrets/argocd-pat",
      "grants": [
        { "repositories": ["team-a/*"] }
      ]
    }
  ],
  "delegates": [
    {
      "name": "kasu-web",
      "token_file": "/run/secrets/kasu-web-pat",
      "act_as": ["*"],
      "allow_admin": true,
      "trusted_peers": ["172.28.0.20/32"]
    }
  ]
}
```

`172.28.0.20` は、後述の Compose が Web UI コンテナに割り当てる固定アドレスです。これにより delegate トークンを使えるのはそのコンテナだけになります。アカウントを設定すると、匿名アクセスは拒否されます。

`config/limits.json` はクライアントごとのクォータを引き上げます。サーバーから見えるピアは Caddy だけなので、全ユーザーが 1 つのクライアント IP のバケットを共有します([理由](README_ja.md#すべてのデプロイで守ること))。値は負荷に合わせて調整してください。既定値は[サーバー制限](../server-runtime_ja.md)にあります。

```json
{
  "active": 128,
  "per_identity_active": 32,
  "client_rate_per_second": 2000,
  "client_burst": 4000
}
```

`config/web.json` は [Web UI](../web-ui_ja.md) の設定です。`trusted_forwarders` は Caddy の固定アドレスで、`X-Forwarded-For` でクライアントのアドレスを与えられるのはこのアドレスだけになります。

```json
{
  "auth": { "pat_login": true },
  "trusted_forwarders": ["172.28.0.10/32"],
  "clone_urls": { "http": "https://git.example.com" },
  "default_email_domain": "users.example.com"
}
```

## Compose ファイル

`compose.yaml`:

```yaml
name: kasu

services:
  server:
    image: ghcr.io/sileader/kasu/server:0.1.0
    restart: unless-stopped
    stop_signal: SIGINT
    environment:
      KASU_TOKEN_FILE: /run/secrets/operator-pat
      KASU_ACCOUNTS_FILE: /etc/kasu/accounts.json
      KASU_BINDING_KEY_FILE: /run/secrets/binding-key
      KASU_SERVER_LIMITS_FILE: /etc/kasu/limits.json
      KASU_PUBLIC_URL: https://git.example.com   # Webhook の clone_url の基準
      RUST_LOG: info
    volumes:
      - data:/data
      - ./config/accounts.json:/etc/kasu/accounts.json:ro
      - ./config/limits.json:/etc/kasu/limits.json:ro
    secrets: [operator-pat, alice-pat, argocd-pat, kasu-web-pat, binding-key]
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

  caddy:
    image: caddy:2   # 本番ではバージョンまたはダイジェストを固定してください
    restart: unless-stopped
    depends_on: [server, web]
    ports:
      - "80:80"
      - "443:443"
      - "443:443/udp"
      - "50051:50051"
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy_data:/data
      - caddy_config:/config
    networks:
      kasu:
        ipv4_address: 172.28.0.10

networks:
  kasu:
    ipam:
      config:
        - subnet: 172.28.0.0/24

volumes:
  data:
  caddy_data:
  caddy_config:

secrets:
  operator-pat:
    file: ./secrets/operator-pat
  alice-pat:
    file: ./secrets/alice-pat
  argocd-pat:
    file: ./secrets/argocd-pat
  kasu-web-pat:
    file: ./secrets/kasu-web-pat
  binding-key:
    file: ./secrets/binding-key
  web-session-key:
    file: ./secrets/web-session-key
```

ポートを公開するのは Caddy だけです。サーバーの 50051 と 50080 には Docker ネットワーク内からしか到達できません。サブネット `172.28.0.0/24` がホスト上の何かと衝突する場合は、別のものを選び、`compose.yaml`、`accounts.json`、`web.json` の 3 か所を変更してください。

`Caddyfile`:

```text
{
	email admin@example.com
}

# Git Smart HTTP と JSON API
git.example.com {
	reverse_proxy server:50080 {
		flush_interval -1
	}
}

# ネイティブ CLI 用の gRPC: KASU_SERVER=https://git.example.com:50051
git.example.com:50051 {
	reverse_proxy h2c://server:50051
}

# Web UI
kasu.example.com {
	reverse_proxy web:50090
}
```

サーバーの `/metrics` は `git.example.com` 経由で到達できますが、オペレーター PAT が必要です。インターネットに出したくない場合はプロキシで遮断してください(`respond /metrics 404`)。

## 起動と確認

```sh
docker compose up -d
docker compose ps          # server と web が "healthy" になるはずです
docker compose logs -f server web caddy
```

Admin API でリポジトリを作成し、読み取って確認します。オペレーター PAT は Basic パスワードまたは Bearer トークンとして使えます。

```sh
export KASU_SERVER=https://git.example.com:50051
export KASU_TOKEN="$(sudo cat secrets/operator-pat)"
kasu repo create team-a/infra
kasu repo list

git -c http.extraHeader="Authorization: Bearer $KASU_TOKEN" ls-remote https://git.example.com/team-a/infra.git
```

次に `https://kasu.example.com/` を開き、PAT ログインを選んで `alice-pat` を貼り付けます。

## 既存のリポジトリをインポートする

サーバーと同じボリュームに対してインポーターを実行します。ローカルストレージではサーバーとインポーターが 1 つの SQLite データベースを共有するため、インポート中はサーバーを止めます。停止するのはインポートの間だけです。

```sh
docker compose stop server web
docker compose run --rm --no-deps \
  -v /path/to/infra.git:/source.git:ro \
  --entrypoint kasu-ingest server \
  --create /source.git /data team-a/infra
docker compose up -d
```

`/path/to/infra.git` は UID 10001 から読める必要があります。何が保存されるかは [Git インポート](../git-import_ja.md)を参照してください。その後の新しい作業は、`git push`([書き込みポリシー](../git-push_ja.md)に従います)またはネイティブ CLI で、ダウンタイムなしに取り込めます。

## リポジトリと Webhook を宣言する

リポジトリの設定を手作業で作る代わりにファイルで管理するには、`repositories.json` をマウントし、サーバーに `KASU_REPOSITORIES_FILE=/etc/kasu/repositories.json` を設定します。このファイルは SIGHUP と 30 秒ごとに再読み込みされます。

```sh
docker compose kill -s HUP server
```

ファイルの形式、書き込みポリシー、Argo CD や Flux 向けの Webhook エンドポイントについては、[複数リポジトリ](../multi-repository_ja.md#リポジトリ単位の設定)と [Webhook](../webhooks_ja.md) を参照してください。Webhook のシークレットもファイルなので、ほかのシークレットと同様にマウントし、パスで参照します。

## バックアップ

ローカルストレージでは、すべてが `data` ボリューム(上のプロジェクト名なら `kasu_data`)にあります。書き込み中の SQLite データベースをコピーしてはいけないので、サーバーを止めてバックアップします。

```sh
docker compose stop server web
docker run --rm -v kasu_data:/data:ro -v "$PWD/backups":/backup \
  debian:bookworm-slim tar czf "/backup/kasu-data-$(date +%Y%m%d-%H%M%S).tar.gz" -C /data .
docker compose start server web
```

ボリュームに含まれない `config/` と `secrets/` も保管してください。復元するには、空の `kasu_data` ボリュームを作り直し、アーカイブを所有者 10001 で展開してから、スタックを起動します。頼る前に、復元を 1 度リハーサルしてください。

ダウンタイムなしのバックアップには共有ストレージが必要です。PostgreSQL と S3 を、それぞれのバックアップ手段とともに使ってください。[高可用性構成](high-availability_ja.md#バックアップと復旧)を参照してください。

## アップグレード

1. バックアップを取ります(上記)。
2. `compose.yaml` のタグを新しいバージョンに変更します。先にリリースノートを読んでください。
3. `docker compose pull && docker compose up -d`。サーバーが再起動し、その数秒間、クライアントはリトライするか失敗します。

ロールバックは、バックアップを古いタグとともに復元することを意味します。保存済みデータをダウングレードする手段はありません。

## バリエーション

- **社内 SSO プロキシの背後で認証する。** Web UI は、信頼するリバースプロキシのヘッダーによるサインインに対応しています(`web.json` の `auth.proxy`。delegate には `"allow_unbound": true` が必要)。[Web UI](../web-ui_ja.md#サインイン)を参照してください。
- **公開の読み取り専用ブラウズ。** delegate に `"anonymous_account"` を、`web.json` に `auth.anonymous` を追加します。匿名の訪問者は常に読み取り専用です。
- **ボリュームの代わりに PostgreSQL と S3 を使う。** [PostgreSQL / S3](../production-storage_ja.md)の変数を `server` に設定し、`data` ボリュームを削除します。これは[高可用性構成](high-availability_ja.md)への第一歩です。共有ストレージなら、サーバーを止めずに再起動、入れ替え、インポートができます。
- **Git over SSH。** `kasu-git-ssh` はイメージに入っていません。ホスト上のビルドから動かす方法は [Git SSH](../git-ssh_ja.md) を参照してください。
- **別のプロキシを使う。** TLS を終端し、HTTP をポート 50080 へ、HTTP/2 cleartext(h2c)をポート 50051 へ転送できるプロキシなら、Caddy の代わりに使えます。[高可用性ガイド](high-availability_ja.md#ロードバランサー)に nginx の例があります。
