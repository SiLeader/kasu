# リリースとコンテナイメージ

[English](release.md) | 日本語

リリースは GitHub Actions の `Release` ワークフロー(`.github/workflows/release.yml`)で作成し、コンテナイメージは `Container` ワークフロー(`.github/workflows/container.yml`)でビルドします。

## リリースの手順

1. 新しいバージョンを宣言されているすべての場所に設定し、デフォルトブランチにコミットします。
   - ルート `Cargo.toml` の `[workspace.package]` の `version`
   - すべての path 依存の `version`。`[workspace.dependencies]` の `kasu-*` と、`crates/kasu-cli/Cargo.toml` の `kasu-daemon`
   - `Cargo.lock`(`cargo update --workspace` を実行)
2. ローカルで確認します: `python3 tools/release/check_version.py <version>`。
3. Actions タブで、デフォルトブランチから **Release** を実行します。`version` には先頭の `v` を付けずに `0.2.0` のように指定します。

先頭に `v` が付いている、上記のどこかが一致しない、タグ `v<version>` が既に存在する、デフォルトブランチ以外から起動した、のいずれかの場合は、何もビルドせずに失敗します。
それ以外では次のジョブを実行します。

| ジョブ | 結果 |
|---|---|
| `cli` | `--release --locked` でビルドした `kasu` CLI のアーカイブ。`kasu-v<version>-linux-x86_64.tar.gz`(`ubuntu-latest` での x86_64-unknown-linux-gnu のため glibc 2.39 以降が必要)と `kasu-v<version>-macos-arm64.tar.gz`(aarch64-apple-darwin)。それぞれ `.sha256` ファイルが付き、`kasu`、`LICENSE`、`README.md` を含みます |
| `containers` | server と web のイメージ。タグは `<version>` と `latest` |
| `release` | GitHub リリース `v<version>`。ワークフローを実行したコミットに作成され、アーカイブとチェックサムをアセットとして持ち、ノートは自動生成されます |

`0.2.0-rc.1` のようにサフィックス付きのバージョンはプレリリースになり、イメージにも `latest` を付けません。

## コンテナイメージ

`docker/Dockerfile` はコンポーネントごとにビルドターゲットを持ちます。どちらのイメージもマルチアーキテクチャ(`linux/amd64` と `linux/arm64`、それぞれネイティブでビルド)で、UID 10001 で動作します。

| イメージ | 内容 | ポート |
|---|---|---|
| `ghcr.io/sileader/kasu/server:<version>` | `kasu-server`(`production` feature でビルド。PostgreSQL と S3 に対応)と `kasu-ingest`。データは `/data` に保存 | 50051 (gRPC)、50080 (HTTP) |
| `ghcr.io/sileader/kasu/web:<version>` | `kasu-web`。`KASU_WEB_*` 変数で設定([Web UI](web-ui_ja.md)) | 50090 |

server イメージは `kasu-server /data 0.0.0.0:50051 0.0.0.0:50080` で起動します。イメージ名の後ろに引数を渡すと、この既定値を置き換えます。

```sh
docker run --rm -p 50051:50051 -p 50080:50080 -v kasu-data:/data \
  -e KASU_TOKEN=change-me ghcr.io/sileader/kasu/server:0.1.0
```

### 開発ビルド

**Container** を任意のブランチで手動実行(workflow_dispatch)すると、`<Cargo.toml のバージョン>-<サフィックス>` と `<Cargo.toml のバージョン>-<サフィックス>-<短縮 SHA>`(サフィックスは指定しなければ `dev`)をプッシュします。例: `0.1.0-dev`、`0.1.0-dev-1a2b3c4`。この実行がリリースバージョンや `latest` をプッシュすることはありません。
