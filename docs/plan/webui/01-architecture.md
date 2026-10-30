# 01. アーキテクチャ

## プロセス構成

```
  ブラウザ ──HTTPS──▶ [リバースプロキシ（任意: TLS 終端 / SSO）]
                              │  信頼ヘッダー（プロキシ認証時のみ）
                              ▼
                     kasu-web（ステートレス, N 台）
                      ├─ HTTP ルーター（axum）
                      ├─ 認証: PAT セッション / プロキシヘッダー / 匿名
                      ├─ 表示: テンプレート・ハイライト・Markdown・diff 整形
                      └─ gRPC クライアント（委譲資格情報 + 代理対象アカウント）
                              │  Metadata / Writer / Admin / Identity / Review
                              ▼
                     kasu-server（権威。認可・ポリシー・レビュー判定はすべてここ）
```

- kasu-web は永続状態を持たない。セッションは署名付き Cookie、レビュー・コメントは kasu-server に保存する。
  任意台数を並べてロードバランサーで振り分けられる（セッション鍵は全台で共有）。
- kasu-web はセキュリティ境界ではない。表示の出し分け（ボタンの有無など）は利便のためで、
  すべての認可は kasu-server が代理対象アカウントの権限で判定する。
- Git Smart HTTP は従来どおり kasu-server が提供する。kasu-web は clone URL を表示するだけで、Git 通信を中継しない。
- Objects RPC は使わない。ファイル内容は `ReadFile`、サイズは `GetBlobMetadata` で取得する。

## クレート構成

```
crates/kasu-web/
  Cargo.toml
  src/
    main.rs          # 起動引数・設定読み込み・listener
    lib.rs           # router 構築（テストから in-process で使う）
    config.rs        # 環境変数 / 設定ファイル
    upstream.rs      # gRPC クライアント。委譲メタデータの付与、エラー写像、タイムアウト
    auth/            # session.rs（Cookie）, proxy.rs, anonymous.rs, csrf.rs
    rev.rs           # URL の rev / path エンコードと解決
    routes/          # repo_list, tree, blob, raw, blame, log, changeset, compare, refs,
                     # edit, admin, review, login, static
    render/          # highlight.rs, markdown.rs, diff.rs, text.rs（非 UTF-8 表示）, time.rs
    i18n.rs          # en / ja のメッセージカタログ（コンパイル時埋め込み）
  templates/         # askama テンプレート（*.html）
  assets/            # app.css, app.js（自前・依存なし）, favicon.svg
  tests/             # in-process kasu-server + kasu-web の結合テスト
```

主要依存（候補）: `axum`, `tonic`, `kasu-proto`, `askama`（コンパイル時テンプレート）, `syntect`（`default-fancy`、C 依存なし）,
`pulldown-cmark`, `ammonia`, `hmac` / `sha2` / `blake3`, `percent-encoding`, `jiff`。
テスト専用に `scraper`（HTML 検査）。いずれも `deny.toml` の許可ライセンス（MIT / Apache-2.0 等）であることを
導入時に `cargo deny check` で確認する。

kasu-web は `kasu-core` の値型（`RepoName`、`RepoPath` 等の検証）と `kasu-proto` だけに依存し、
`kasu-store` / `kasu-server` には依存しない（独立プロセスの境界をコンパイル時に保証する）。

## kasu-server との通信

- tonic のチャネルを 1 本共有し（HTTP/2 多重化）、`KASU_WEB_SERVER` に接続する。`https://` なら rustls で TLS。
- 全要求に委譲メタデータを付ける（詳細は [02](02-auth.md#委譲資格情報)）。

| メタデータ | 内容 |
|---|---|
| `authorization` | `Bearer <delegate token>` |
| `kasu-act-as` | 代理対象アカウント名（匿名時は設定した匿名アカウント） |
| `kasu-act-binding` | PAT セッションの場合の結び付け値（PAT 更新による失効検出） |
| `kasu-act-user` | 監査用の利用者表示名（プロキシ認証の利用者 ID 等、最大 256 bytes） |
| `kasu-client-addr` | ブラウザの接続元アドレス（admission の IP 単位制限に使う） |
| `traceparent` | W3C Trace Context（既存 telemetry と連結） |

- 要求ごとの期限: 読み取り 10 秒、書き込み 30 秒（Land / Submit は API 側の試行予算に従う）。
- 自動再送は読み取りの `UNAVAILABLE` のみ 1 回。副作用のある要求は再送せず、冪等キー（Land / Submit / CreateReview）で
  利用者の再送信を安全にする。

### エラーの写像

| gRPC | HTTP | 画面 |
|---|---|---|
| `UNAUTHENTICATED` | 302 → ログイン（匿名閲覧時は 401 画面） | セッション切れの案内 |
| `NOT_FOUND`, `PERMISSION_DENIED`（読み取り） | 404 | 共通の「見つからない」画面。不可視と不存在を区別しない |
| `PERMISSION_DENIED`（書き込み） | 403 | 拒否理由（サーバーのメッセージ）を表示 |
| `INVALID_ARGUMENT` | 400 | 入力エラー |
| `FAILED_PRECONDITION` | 409 | ReadOnly / Archived、CAS 不一致、承認不足など |
| `RESOURCE_EXHAUSTED` | 429 | `Retry-After` を引き継ぐ |
| その他 | 502 | 詳細は tracing のみに記録 |

404 画面は本文・ヘッダー・応答サイズを要因によらず同一にする（存在オラクルを作らない）。

## リクエスト処理の流れ（例: ファイル表示）

1. URL を生のパスのまま解析し、`repo` / `rev` / `path` を復号する（[03](03-screens.md#url-体系)）。
2. 認証方式に応じて代理対象アカウントを決める。
3. `rev` を Changeset ID に解決する（ID 形式ならそのまま、ref なら `ResolveRef`）。
4. 親ディレクトリを `ListDir` し、対象 entry の blob ID を得る。`GetBlobMetadata` でサイズを確認し、
   表示上限以下なら `ReadFile`、超過なら「大きすぎるファイル」表示と raw リンクだけを返す。
5. テキスト判定 → ハイライト（上限内）→ テンプレート描画。
6. ref で指定された表示には解決後の Changeset ID を併記し、パーマリンク（ID 指定 URL）を出す。

## レンダリング

- テンプレートは askama でコンパイル時に検査し、HTML エスケープを既定にする。生の HTML 挿入は
  サニタイズ済み Markdown とハイライト出力（エスケープ済みテキスト + class 付き span）のみ。
- ハイライトは CPU を使うため `spawn_blocking` で実行し、同時実行数をセマフォで制限する。
  クラス名による出力にして CSS をアセットに置く（インライン style を使わず CSP を厳格に保つ）。
- diff は `Diff(content=false)` でファイル一覧を取得し、表示するファイルだけを `paths` 指定で `content=true` 取得する。
  巨大な diff を 1 要求で取得しない。
- 非 UTF-8 のパス・メッセージ・内容は損失のある表示（U+FFFD ではなく `\xNN` の目印付き）とし、
  リンク・フォーム値は常に元のバイト列から生成する。

### 表示上限（設定可能な既定値）

| 対象 | 既定 | 超過時 |
|---|---:|---|
| ファイル表示 | 1 MiB | raw ダウンロードのみ |
| ハイライト | 512 KiB または 20,000 行 | プレーンテキスト表示 |
| blame | 20,000 行 | 「大きすぎる」表示 |
| diff のファイル数（1 ページ） | 100 | ページ分割 |
| diff の 1 ファイルの変更行 | 2,000 | 折りたたみ、個別読み込みリンク |
| ディレクトリ entry | 5,000 | 先頭のみ表示し件数を明示 |
| Web 編集・アップロード | 10 MiB / ファイル、合計 32 MiB | 400 |

## キャッシュ

- HTTP: Changeset ID で指定した表示は `Cache-Control: private, max-age=3600`、ref で指定した表示・レビュー・管理画面は
  `private, no-cache`。どちらも `private` とする（アカウントごとに `read_filtered` の結果が異なるため共有キャッシュ不可）。
  静的アセットはハッシュ付き URL で `public, max-age=31536000, immutable`。
- プロセス内: 不変な応答（ID 指定の `ListDir` / `ReadFile` / `GetChangeset` / ハイライト結果）を LRU に保持してよい。
  キーには **代理対象アカウントと匿名かどうか** を必ず含める（アカウント間の漏えい防止）。容量は設定で上限を置く。
  ref 解決結果・権限照会結果はキャッシュしない（または 1 秒未満）。

## 設定

設定は環境変数（`KASU_WEB_*`）と、構造化設定用の JSON ファイル `KASU_WEB_CONFIG_FILE` で与える。
既存の設定ファイルと同様に、未知フィールド・読めない秘密ファイル・範囲外の値は起動エラーとし、
相対パスは設定ファイルの場所を基準にする。

| 設定 | 意味 |
|---|---|
| `KASU_WEB_LISTEN` | 待ち受けアドレス（既定 `127.0.0.1:50090`） |
| `KASU_WEB_SERVER` | kasu-server の gRPC URL |
| `KASU_WEB_DELEGATE_TOKEN_FILE` | 委譲資格情報 |
| `KASU_WEB_SESSION_KEY_FILE` | セッション Cookie・CSRF トークンの HMAC 鍵（32 bytes 以上、全台共通） |
| `KASU_WEB_PUBLIC_URL` | 外部公開 URL。リダイレクト・Cookie 属性・絶対 URL 生成に使う |
| `KASU_WEB_CONFIG_FILE` | 下記 JSON |

```json
{
  "auth": {
    "pat_login": true,
    "proxy": {
      "trusted_peers": ["10.0.0.0/8"],
      "user_header": "X-Forwarded-User",
      "email_header": "X-Forwarded-Email",
      "name_header": "X-Forwarded-Preferred-Username",
      "accounts": { "map": { "alice@example.com": "alice" }, "default_from_user": false }
    },
    "anonymous": { "enabled": true }
  },
  "clone_urls": { "http": "https://kasu.example", "ssh": "git@kasu.example:" },
  "default_email_domain": "users.kasu.example",
  "limits": { "file_view_bytes": 1048576, "highlight_bytes": 524288, "upload_bytes": 10485760 },
  "cache": { "bytes": 268435456 }
}
```

- 開発用に、kasu-server が認証なしで動いている場合は `KASU_WEB_DELEGATE_TOKEN_FILE` を省略でき、
  その場合は委譲メタデータを付けずに接続する（全利用者が同一の匿名主体になる）。本番では起動時に警告する。

## 観測性

- 既存の `kasu-telemetry` で tracing / OpenTelemetry を初期化し、upstream 呼び出しに `traceparent` を伝播する。
- `GET /-/metrics`（Prometheus）: 画面種別ごとのレイテンシ、upstream RPC のレイテンシとエラー、ハイライトのキュー長、
  キャッシュヒット率、ログイン成功 / 失敗。リポジトリ名・アカウント名はラベルにしない。
  metrics は `KASU_WEB_METRICS_TOKEN_FILE` の Bearer で保護する（未設定時は無効）。
- `GET /-/healthz`（プロセス生存）、`GET /-/readyz`（kasu-server への疎通）。

## 静的アセット

- `assets/` をバイナリに埋め込み、起動時に BLAKE3 で短いハッシュを計算して `/-/static/<hash>/<file>` で配信する。
- JS は依存なしの ES モジュール 1 つ（目安 20 KiB 以下）。担当は行範囲選択とアンカー更新、diff の行コメントフォーム展開、
  折りたたみ diff の遅延読み込み、コピーボタン、相対時刻、二重送信防止。JS 無効時もフォーム送信とリンクで同じ操作ができる。
- CSS は 1 ファイル。`prefers-color-scheme` によるダークモード、ハイライト用クラス。外部 CDN・Web フォントは使わない。
