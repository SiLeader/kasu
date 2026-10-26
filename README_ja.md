# kasu

[English](README.md) | 日本語

kasu は、Anthropic の VCS **Scion** で説明された仕組みを Rust で実装した、サーバー中心のバージョン管理システムです。既存の GitOps と相互運用するため、読み取り専用の Git アクセス機能を備え、Argo CD や Flux などから Git のプロトコルでリポジトリを利用できます。

サーバー側での操作、差分を記録する Changeset、派生データ、スパースな working copy、階層型オブジェクトキャッシュという Scion の設計に沿っています。元の設計については、Anthropic の記事 [Redesigning source control at Anthropic](https://www.anthropic.com/research/redesigning-source-control) を参照してください。

## 名前の由来と読み方

Git には「愚か者」「使えないやつ」といった意味があります。それにならい、日本語で似た意味を持つ **カス** を名前にしました。「カス」は本来、残りかすなどを指す一般的な語ですが、人に対して使う場合は「使えないやつ」といった侮蔑的な意味になり、スラングのような雰囲気も持ちます。**kasu の読み方は、日本語のまま「カス」です。**

## 仕組み

- **サーバー側でのバージョン管理**：Changeset の作成、merge、rebase、diff、blame、履歴の問い合わせをサーバーで行います。ネイティブクライアントはオンラインで動作し、必要なオブジェクトを取得します。
- **差分に基づく保存**：不変の Changeset に第一親からのファイル変更を記録します。オブジェクトは BLAKE3 の内容アドレスで識別し、ref の更新は Changeset の作成とは分離した compare-and-swap で行います。
- **派生データ（Derived Data）**：Tree、履歴索引、blame、Git 射影などを増分計算し、種類ごとに事前計算と遅延計算を使い分けます。
- **スパースな working copy**：ネイティブ CLI は選択した Tree／Blob を取得します。Linux では FUSE と OverlayFS による仮想 checkout も利用できます。
- **階層型のオブジェクト取得**：CLI からノード内のディスク daemon、共有メモリキャッシュへ、同じ Objects RPC で接続できます。
- **ストレージの選択**：開発用の SQLite／ローカルオブジェクトストアと、複数サーバーで共有できる PostgreSQL／S3 互換ストレージに対応します。

Metadata と書き込み操作は gRPC／HTTP JSON API として公開しているため、自動化ツールは working copy を作成せずにリポジトリを問い合わせ、変更できます。

## GitOps との相互運用

Git アクセスは kasu のネイティブモデルを Git として見せる射影（projection）です。別の Git リポジトリを保持せず、ゲートウェイが Git オブジェクトを生成し、pack をストリーム配信します。

- Smart HTTP による `git clone`、`git fetch`、`git ls-remote`。protocol v0／v1／v2 に対応。
- shallow clone、増分 fetch、deepen／unshallow、`blob:none` の partial clone。
- 指定ディレクトリをリポジトリの root として公開する、読み取り専用のパス射影。
- 対応する入力の元オブジェクトと SHA-1 OID を保持する Git 取り込み。
- Git、go-git、Argo CD、Flux との結合試験。検証範囲は [Git 読み取り](docs/git-read.md) と [GitOps E2E 記録](docs/gitops-e2e.md) を参照。

画像更新などの GitOps の書き戻し用途には、追加で [ポリシーに基づく Git push](docs/git-push.md) にも対応しています。読み取り専用のパス射影は書き込みを受け付けません。アカウント権限と書き込みポリシーは [リポジトリアカウント](docs/repository-accounts.md) を参照してください。

## ローカルで試す

Rust stable、C コンパイラ（SQLite／zstd のビルド用）、`protoc`、Git が必要です。`protoc` は Ubuntu では `protobuf-compiler`、macOS では Homebrew の `protobuf` で用意できます。ネイティブ CLI は macOS／Linux を対象とします。

このリポジトリの root で、完全な履歴を持つ既存の Git リポジトリを取り込み、サーバーを起動します。

```sh
cargo run -p kasu-ingest -- /path/to/source.git /path/to/kasu-data my-repo
KASU_TOKEN=dev-token cargo run -p kasu-server -- /path/to/kasu-data my-repo
```

既定の待ち受け先は gRPC が `127.0.0.1:50051`、HTTP が `127.0.0.1:50080` です。別のターミナルから Git で読み取れます。

```sh
git -c http.extraHeader='Authorization: Bearer dev-token' ls-remote http://127.0.0.1:50080/my-repo.git
git -c http.extraHeader='Authorization: Bearer dev-token' clone --depth=1 http://127.0.0.1:50080/my-repo.git git-work
```

ネイティブの working copy を使う場合は CLI をビルドし、同じサーバーへ接続します。`main` は取り込んだリポジトリにあるブランチ名に置き換えてください。

```sh
cargo build -p kasu-cli --locked
export KASU_SERVER=http://127.0.0.1:50051
export KASU_REPO=my-repo
export KASU_TOKEN=dev-token
./target/debug/kasu checkout main native-work
./target/debug/kasu -C native-work status
```

編集、commit、rebase、Land の使い方は [ネイティブ CLI](docs/native-cli.md)、取り込み可能な入力と制限は [Git 取り込み](docs/git-import.md) を参照してください。`dev-token` はローカル用の認証情報の例です。

## ドキュメントと実装状況

ネイティブ CLI、サーバー API、Git ゲートウェイ、キャッシュ階層、PostgreSQL／S3 バックエンドを実装しています。完了項目、検証範囲、残作業は [実装状況](docs/implementation-status.md)、実装箇所と検証記録の対応は [計画・実装・検証の対応表](docs/plan-audit.json) を参照してください。詳細ドキュメントは現在、主に日本語で記述しています。

| 項目 | ドキュメント |
|---|---|
| アーキテクチャ・データモデル | [設計・実装計画](docs/plan/README.md) |
| working copy | [ネイティブ CLI](docs/native-cli.md)、[仮想 checkout](docs/virtual-checkout.md) |
| ネイティブ API | [Metadata](docs/metadata-api.md)、[書き込み](docs/write-api.md)、[Objects](docs/object-api.md)、[Land](docs/land.md) |
| Git 互換機能 | [取り込み](docs/git-import.md)、[読み取り](docs/git-read.md)、[push](docs/git-push.md)、[SSH](docs/git-ssh.md)、[外部 LFS](docs/git-lfs.md) |
| アクセス制御・射影 | [アカウント](docs/repository-accounts.md)、[ディレクトリ読み取り](docs/directory-read-acl.md)、[Git 射影](docs/git-projection.md)、[射影アカウント](docs/projection-accounts.md) |
| ストレージ・キャッシュ | [PostgreSQL／S3](docs/production-storage.md)、[キャッシュ階層](docs/cache-hierarchy.md)、[ディスクキャッシュ](docs/disk-cache.md) |
| 運用・自動化 | [サーバー制限・メトリクス](docs/server-runtime.md)、[tracing](docs/telemetry.md)、[Webhook](docs/webhooks.md)、[CI bot](docs/automation.md) |
| 検証 | [GitOps E2E](docs/gitops-e2e.md)、[シミュレーション](docs/simulations/README.md)、[ベンチマーク](docs/benchmarks/README.md)、[KV 評価](docs/benchmarks/kv-evaluation.md) |

## 開発

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo deny check
```

`cargo deny` には `cargo-deny` が必要です。外部サービスや fixture が必要なテストは、各ガイドの手順で実行します。

10 万 Changeset の履歴ベンチマークは、明示的に有効化した場合のみ実行し、計測時間を CSV で出力します。

```sh
KASU_BENCH_HISTORY=1 cargo bench -p kasu-ops --bench history
```

## ライセンス

[Apache License 2.0](LICENSE)。
