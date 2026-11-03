# 開発用デプロイ

[English](development.md) | 日本語

kasu の試用、kasu を相手にした開発、CI のフィクスチャのための、使い捨ての環境です。ローカルストレージのサーバーを 1 台、**認証なし**で動かし、必要に応じて Web UI を加えます。自分のマシンの外には公開しないでください。複数人で使うなら[1 ノード構成](single-node_ja.md)を使います。

イメージと共通の決まりごとについては、先に [Docker による kasu のデプロイ](README_ja.md)を読んでください。

## `docker run` で素早く始める

既存の Git リポジトリをインポートしてから、サーバーを起動します。`/path/to/source.git` は UID 10001 が読める必要があります(例: `chmod -R a+rX`)。

```sh
docker volume create kasu-data

docker run --rm \
  -v kasu-data:/data \
  -v /path/to/source.git:/source.git:ro \
  --entrypoint kasu-ingest \
  ghcr.io/sileader/kasu/server:0.1.0 \
  --create /source.git /data my-repo

docker run -d --name kasu --stop-signal SIGINT \
  -p 127.0.0.1:50051:50051 -p 127.0.0.1:50080:50080 \
  -v kasu-data:/data \
  ghcr.io/sileader/kasu/server:0.1.0
```

`-p 127.0.0.1:...` は、認証のないサーバーをネットワークに出さないための指定です。Git で読み取ってみます。

```sh
git ls-remote http://127.0.0.1:50080/my-repo.git
git clone --depth=1 http://127.0.0.1:50080/my-repo.git
```

ローカルでもトークンを必須にして、本番に近づけるには、2 つ目のコマンドに `-e KASU_TOKEN=dev-token` を加え、Git からトークンを送ります。

```sh
git -c http.extraHeader='Authorization: Bearer dev-token' clone http://127.0.0.1:50080/my-repo.git
```

停止してすべて削除するには次のようにします。

```sh
docker rm -f kasu
docker volume rm kasu-data
```

## Compose: サーバーと Web UI

Web UI は別プロセスで、ステートを持ちません。認証なしのサーバーに対しては delegate トークンが不要で(訪問者全員がサーバーの開放されたアクセスを共有します)、そのためこの構成はとても短く書けます。作業用ディレクトリに `compose.yaml` として保存してください。

```yaml
name: kasu-dev

services:
  server:
    image: ghcr.io/sileader/kasu/server:0.1.0
    # pull せずにチェックアウトからビルドする場合:
    # build: { context: /path/to/kasu, dockerfile: docker/Dockerfile, target: server }
    stop_signal: SIGINT
    ports:
      - "127.0.0.1:50051:50051"   # gRPC: ネイティブ CLI
      - "127.0.0.1:50080:50080"   # HTTP: JSON API と Git
    volumes:
      - data:/data
    healthcheck:
      test: ["CMD", "bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/50080"]
      interval: 5s
      timeout: 3s
      retries: 10

  web:
    image: ghcr.io/sileader/kasu/web:0.1.0
    stop_signal: SIGINT
    depends_on:
      server:
        condition: service_healthy
    environment:
      KASU_WEB_SERVER: http://server:50051
      KASU_WEB_PUBLIC_URL: http://127.0.0.1:50090
      KASU_WEB_SESSION_KEY_FILE: /run/secrets/web-session-key
    secrets:
      - web-session-key
    ports:
      - "127.0.0.1:50090:50090"

  # 1 回だけ実行するインポーター: `docker compose run --rm ingest ...`(`up` では起動しません)
  ingest:
    image: ghcr.io/sileader/kasu/server:0.1.0
    profiles: ["tools"]
    entrypoint: ["kasu-ingest"]
    volumes:
      - data:/data

volumes:
  data:

secrets:
  web-session-key:
    file: ./web-session.key
```

セッション鍵を作り(開発用なので誰でも読める権限で構いません)、インポートして起動します。

```sh
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > web-session.key
chmod 644 web-session.key

docker compose run --rm \
  -v /path/to/source.git:/source.git:ro \
  ingest --create /source.git /data my-repo

docker compose up -d
```

`ingest` はサーバーと同じ `data` ボリュームを使うため、最初の `up` の前に実行してください(または先に `docker compose stop server` で止めます)。リポジトリを追加するときも同じ方法で、`team-a/infra` のような名前を使えます([複数リポジトリ](../multi-repository_ja.md))。

Web UI は `http://127.0.0.1:50090/`、Git は `http://127.0.0.1:50080/my-repo.git` で使えます。Web UI は、delegate トークンが未設定であることと、公開 URL が `http://` のため Cookie が `Secure` でないことを警告としてログに出しますが、どちらもこの環境では想定どおりです。

### ネイティブ CLI を使う

`kasu` CLI を[リリースのアーカイブ](../release_ja.md)から入れる(macOS または Linux)か、`cargo build -p kasu-cli --locked` でビルドします。その後次のようにします。

```sh
export KASU_SERVER=http://127.0.0.1:50051
export KASU_REPO=my-repo
kasu checkout main work
kasu -C work status
```

編集、コミット、land については[ネイティブ CLI ガイド](../native-cli_ja.md)を参照してください。

### リセット

```sh
docker compose down -v     # データボリュームも削除されます
```

## 本番用ストレージバックエンドをローカルで試す

サーバーイメージは PostgreSQL と S3 を使えます。リポジトリ自身のテスト用ハーネス `tools/storage/compose.yml` は、固定したバージョンの PostgreSQL 18 と SeaweedFS(S3)のコンテナを、ローカルホストのポート 55432 と 58333 で、公開されているテスト用資格情報とともに起動します。`python3 tools/storage/run.py` がこれを動かします。複数のサーバーを 1 台のマシンで試したい場合は、[高可用性構成](high-availability_ja.md#1-台のマシンでのリハーサル)の Compose ファイルを使ってください。

## トラブルシューティング

| 症状 | 原因 |
|---|---|
| バインドマウントした `/data` への書き込みで `Permission denied` | ディレクトリが UID 10001 から書き込めません: `sudo chown 10001:10001 <dir>` |
| `kasu-ingest` がソースを開けない | リポジトリのディレクトリそのもの(ベアリポジトリ、または作業ツリーの `.git`)をマウントし、UID 10001 から読めるようにしてください |
| Web UI が "upstream" エラーを表示する | `KASU_WEB_SERVER` はサーバーの gRPC URL(ポート 50051)です。HTTP ポートではありません |
| `docker compose up` が `web` で止まる | サーバーのヘルスチェックが失敗しています。`docker compose logs server` を確認してください |
| `kasu-ingest` が "is not registered" と言う | 新しい名前を登録するには `--create` を付けます。付けない場合、インポートできるのはレジストリが空のときだけです。`--create` を付けて既存の名前を指定すると、そのリポジトリへ再度インポートします |
