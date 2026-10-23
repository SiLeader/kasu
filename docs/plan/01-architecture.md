# 01. アーキテクチャ

## コンポーネント

```
                ┌─────────────────────────────── 権威層 ───────────────────────────────┐
                │  kasu-server (stateless, N 台)                                       │
                │   ├─ Metadata API (changeset, ref, diff, blame, history, merge-base)  │
                │   ├─ Write API (create changeset, rebase, merge, ref CAS)             │
                │   ├─ Object API (GetTree / GetBlob)  ← 全階層共通 RPC                 │
                │   ├─ Derived Data Engine                                              │
                │   └─ Git Gateway (Smart HTTP: upload-pack / receive-pack)             │
                │        │                                                              │
                │   MetaStore (KV/SQL: changeset, ref, derived)  BlobStore (S3 互換)     │
                └────────┬─────────────────────────────────────────────────────────────┘
                         │ 同一 Object RPC
                ┌────────┴────────┐
                │ kasu-cache      │  リージョン単位、メモリ常駐、ステートレス
                └────────┬────────┘
                         │ 同一 Object RPC
                ┌────────┴────────┐
                │ kasu-daemon     │  ノード単位、ディスクオブジェクトストア、スナップショットから起動
                └────────┬────────┘
                         │ Unix socket
                ┌────────┴────────┐
                │ kasu (CLI)      │  working copy 管理・FS 監視・UX
                └─────────────────┘

  外部:  git / go-git / ArgoCD / Flux ──HTTPS──▶ Git Gateway
         CI / bisect / revert bot ──gRPC/HTTP──▶ Metadata API（チェックアウト不要）
```

### kasu-server
- ステートレス。全データは MetaStore / BlobStore にあり、ノードローカルは LRU キャッシュのみ。
- content-addressed データはノード間協調なしにキャッシュ可能。可変状態は ref のみ。
- Derived Data を要求時に計算し、MetaStore に書き戻す（冪等）。

### kasu-cache
- `GetTree` / `GetBlob` のみを提供する読み取りキャッシュ。ミス時は上位（kasu-server）へ。
- ディスク格納形式＝daemon の格納形式でストリームし、変換コストをゼロにする。
- 障害ドメイン分離のため、階層はスキップ不可（設定で強制）。上位への同時リクエスト数を制限（singleflight + レート制限）。

### kasu-daemon
- ノード上の全ジョブで共有するオブジェクトストア。定期スナップショット（tarball / ディスクイメージ）から起動して初回ミスを削減。
- CLI とは Unix ドメインソケット上の gRPC で通信。

### kasu (CLI)
- working copy のレイアウト、スパースプロファイル、FS 変更監視（`notify` クレート）。
- commit 時は変更ファイルの blob をアップロードし、サーバーに Changeset 作成を依頼する。ローカルでの履歴計算はしない。

## ストレージ抽象

| trait | 開発用実装 | 本番用実装（候補） |
|---|---|---|
| `MetaStore` | SQLite | PostgreSQL → FoundationDB / TiKV / Spanner |
| `BlobStore` | ローカル FS | S3 / GCS / MinIO |
| `RefStore` | SQLite（ref + ref_log） | PostgreSQL → 分散 KV（単一パーティション Tx） |

要件:
- 不変レコード（Changeset / Blob / Derived Data）は **冪等 put-if-absent のみ** に依存し、トランザクション・ロック不要。
- `RefStore` のみ「ref CAS と ref_log 追記の原子性」を必須とし、「複数 ref トランザクション」は任意能力とする
  （能力モデルは 02 の「RefStore の能力モデル」参照。Git ゲートウェイは能力に応じて `atomic` 等の広告を切り替える）。

## Rust ワークスペース構成

現在の単一クレート `kasu` を Cargo workspace に移行する。

```
crates/
  kasu-core      # ID/ハッシュ, Changeset, Tree, Blob, パス型, エラー型
  kasu-store     # MetaStore / BlobStore / RefStore trait と実装 (sqlite, fs, s3, postgres)
  kasu-derived   # Derived Data フレームワークと各種派生データ
  kasu-ops       # サーバー側演算: diff, 3-way merge, rebase, blame, merge-base
  kasu-proto     # protobuf 定義 (tonic-build)
  kasu-server    # 権威サーバー (gRPC + HTTP)
  kasu-cache     # リージョンキャッシュ
  kasu-daemon    # ノードデーモン
  kasu-cli       # `kasu` コマンド
  kasu-git       # Git オブジェクト変換, pack 生成/解析, Smart HTTP ゲートウェイ
  kasu-ingest    # 既存 Git リポジトリからの取り込み
  kasu-sim       # 決定的シミュレーションテスト基盤
```

主要依存クレート（候補）: `tokio`, `tonic`/`prost`, `axum`, `gix`（gitoxide: オブジェクト/パック/プロトコル）,
`blake3`, `sha1`, `zstd`, `rusqlite`/`sqlx`, `object_store`（S3/GCS 抽象）, `notify`, `fuser`（後半）, `tracing`, `proptest`.

## 横断的関心事

- **認証/認可**: リポジトリ単位 + ディレクトリ単位 ACL。トークン（PAT / OIDC）。詳細は 04 の制約を参照。
- **観測性**: `tracing` + OpenTelemetry。各階層のキャッシュヒット率、Derived Data の遅延、ref CAS 競合率をメトリクス化。
- **レート制限**: クライアント/トークン単位、上位階層への fan-in 制限。
