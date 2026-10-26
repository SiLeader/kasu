# kasu

サーバー中心の VCS と GitOps 向け Git 互換レイヤーを Rust で実装するプロジェクトです。
仕様・実装計画は [docs/plan](docs/plan/README.md)、現在の実装範囲と未完了項目は
[実装状況](docs/implementation-status.md) と [計画・実装・検証の対応表](docs/plan-audit.json) を参照してください。

ライブラリ基盤、Git 取り込み、Metadata／Git Smart HTTP／Object サーバーと、
[ネイティブ CLI](docs/native-cli.md) の working copy、[メモリ cache／ディスク daemon](docs/cache-hierarchy.md) を実装しています。
[daemon のディスク容量管理](docs/disk-cache.md) は永続 LRU、再起動復旧、snapshot 世代管理に対応します。
[PostgreSQL／S3](docs/production-storage.md) を共有する複数 server にも対応します。
Git は `blob:none` の [partial clone](docs/git-read.md#partial-clone) と後続の promisor fetch に対応しています。
指定ディレクトリを root とする [パス射影](docs/git-projection.md) の読み取り URL も提供します。
[射影専用 PAT](docs/projection-accounts.md) で、許可ディレクトリだけを読めるアカウントを設定できます。
[OpenTelemetry tracing](docs/telemetry.md) で CLI／daemon／cache／server の trace context を引き継ぎます。
[権威サーバーの制限とメトリクス](docs/server-runtime.md) は HTTP／gRPC の同時数・待機数と IP／PAT の割当量を共有します。
[CI bot の参照実装](docs/automation.md) は Metadata だけの test gate／DAG bisect と、再開可能な guarded revert を提供します。

```sh
RUSTC_WRAPPER= cargo run -p kasu-ingest -- /path/to/repo.git /path/to/kasu-data my-repo
KASU_TOKEN=dev-token RUSTC_WRAPPER= cargo run -p kasu-server -- /path/to/kasu-data my-repo
```

[Git 取り込み](docs/git-import.md)、[Metadata API](docs/metadata-api.md)、[Write API](docs/write-api.md)、[Git 読み取り](docs/git-read.md)、[Git 書き込み](docs/git-push.md)、[Git SSH](docs/git-ssh.md)、[アカウント ACL](docs/repository-accounts.md)、[ディレクトリ読み取り](docs/directory-read-acl.md)、[外部 LFS](docs/git-lfs.md)、[Webhook](docs/webhooks.md)、[Object RPC](docs/object-api.md) の対応範囲・使用方法を参照してください。

## 開発

Rust stable、C コンパイラ（SQLite / zstd のビルド用）、`protoc` が必要です。
Ubuntu は `protobuf-compiler`、macOS は Homebrew の `protobuf` で protoc を用意できます。

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTC_WRAPPER= cargo test --workspace --all-targets --all-features --locked
RUSTC_WRAPPER= cargo deny check
```

親ディレクトリから継承する sccache 設定は変更せず、Cargo 実行時に `RUSTC_WRAPPER` を空にします。

10 万 Changeset（1 ファイル更新／Changeset、約 10% が第二親を持つ履歴）の計測:

```sh
KASU_BENCH_HISTORY=1 RUSTC_WRAPPER= cargo bench -p kasu-ops --bench history
```

ベンチマークは明示的な環境変数がある場合のみ実行します。通常の `cargo test --all-targets` は
この負荷計測を実行しません。結果は 1,000 件ごとの平均作成時間（µs）を CSV として標準出力します。

分散 KV 候補の保存契約・性能・障害復旧は [TiKV／PostgreSQL 評価](docs/benchmarks/kv-evaluation.md) に記録しています。
