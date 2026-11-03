# リリースとコンテナイメージ

[English](release.md) | 日本語

リリースは GitHub Actions の `Release` ワークフロー(`.github/workflows/release.yml`)で作成し、コンテナイメージは `Container` ワークフロー(`.github/workflows/container.yml`)でビルドします。

## リリースの手順

1. Actions タブで、デフォルトブランチから **Release** を実行します。`version` には先頭の `v` を付けずに `0.2.0` のように指定します。
2. リポジトリがそのバージョンをすべての場所で宣言していれば、リリースがビルド・公開されます(下記)。
3. そうでなければ何もリリースせず、バージョンを設定するプルリクエスト `release/set-version-<version>` を作成して正常終了します。これをマージしてから、もう一度 **Release** を実行してください。

バージョンは次の場所で宣言されます。`python3 tools/release/check_version.py <version>` でローカルでも確認でき、`tools/release/set_version.py <version>` で書き換えられます。
- ルート `Cargo.toml` の `[workspace.package]` の `version`と、独自の `version` を持つメンバーの `version`
- すべての path 依存の `version`。`[workspace.dependencies]` の `kasu-*` と、`crates/kasu-cli/Cargo.toml` の `kasu-daemon`
- `Cargo.lock` 内のワークスペースメンバー

プルリクエストはワークフローの `GITHUB_TOKEN` で作成するため、リポジトリ設定 *Allow GitHub Actions to create and approve pull requests*(Settings、Actions、General)を有効にしておく必要があります。また、そのプルリクエストでは CI が自動では起動しません(`GITHUB_TOKEN` が起こしたイベントではワークフローが実行されない)。閉じて開き直すと CI が起動します。
先頭に `v` が付いているなどセマンティックバージョンでない、タグ `v<version>` が既に存在する、デフォルトブランチ以外から起動した、スクリプトが書き換えられないバージョン宣言が残っている、のいずれかの場合は、プルリクエストを作らずに失敗します。

バージョンが一致している場合は、次のジョブを実行します。

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

server イメージは `kasu-server /data 0.0.0.0:50051 0.0.0.0:50080` で起動します。イメージ名の後ろに引数を渡すと、この既定値を置き換えます。デプロイのシナリオ(開発、1 ノード、高可用性)については [Docker による kasu のデプロイ](deployments/README_ja.md)を参照してください。

```sh
docker run --rm -p 50051:50051 -p 50080:50080 -v kasu-data:/data \
  -e KASU_TOKEN=change-me ghcr.io/sileader/kasu/server:0.1.0
```

### 開発ビルド

**Container** を任意のブランチで手動実行(workflow_dispatch)すると、`<Cargo.toml のバージョン>-<サフィックス>` と `<Cargo.toml のバージョン>-<サフィックス>-<短縮 SHA>`(サフィックスは指定しなければ `dev`)をプッシュします。例: `0.1.0-dev`、`0.1.0-dev-1a2b3c4`。この実行がリリースバージョンや `latest` をプッシュすることはありません。
