# Web UI

[English](web-ui.md) | 日本語

`kasu-web` は kasu のブラウザ向けインターフェースです。リポジトリの閲覧、ファイル編集、リポジトリ管理、Changeset スタックのレビューを提供します。独立したステートレスなプロセスで、**委譲資格情報（delegate）** を使って `kasu-server` と gRPC で通信し、要求ごとにどのアカウントとして振る舞うかを指定します。認可はすべて kasu-server が判定し、Web UI は使えないボタンを隠すだけです。

画面はサーバー側でレンダリングします（askama テンプレート、syntect によるハイライト、サニタイズ済み Markdown）。小さなスクリプトが行範囲選択、コピーボタン、相対時刻、diff の遅延読み込みなどの便利機能を加えますが、すべての操作は JavaScript なしでも完結します。ビルドに Node.js や npm は使いません。

## 起動

```sh
# kasu-server: アカウントと delegate（後述「サーバーの設定」）
KASU_ACCOUNTS_FILE=accounts.json KASU_BINDING_KEY_FILE=binding.key \
  cargo run -p kasu-server -- /path/to/kasu-data

# kasu-web
KASU_WEB_SERVER=http://127.0.0.1:50051 \
KASU_WEB_DELEGATE_TOKEN_FILE=secrets/kasu-web.pat \
KASU_WEB_SESSION_KEY_FILE=secrets/web-session.key \
KASU_WEB_PUBLIC_URL=https://kasu.example \
KASU_WEB_CONFIG_FILE=web.json \
  cargo run -p kasu-web
```

| 変数 | 意味 |
|---|---|
| `KASU_WEB_LISTEN` | 待ち受けアドレス（既定 `127.0.0.1:50090`） |
| `KASU_WEB_SERVER` | kasu-server の gRPC URL。`https://` は WebPKI ルートで TLS 接続 |
| `KASU_WEB_DELEGATE_TOKEN_FILE` | delegate のトークン。認証なしで動くサーバー（開発用）の場合のみ省略でき、その場合は全訪問者がその未認証アクセスを共有する |
| `KASU_WEB_SESSION_KEY_FILE` | セッション Cookie と CSRF トークンの HMAC 鍵（32 bytes 以上、全レプリカで同一） |
| `KASU_WEB_PUBLIC_URL` | 外部公開の `scheme://host[:port]`。`http://` の場合は `Secure` と `__Host-` 接頭辞を外す（開発用、警告をログに出す） |
| `KASU_WEB_METRICS_TOKEN_FILE` | `GET /-/metrics` の Bearer トークン。未設定ならメトリクスは無効 |
| `KASU_WEB_CONFIG_FILE` | 下記の JSON 設定。未知のフィールドは拒否 |

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
    "anonymous": { "enabled": true, "account": "public-reader" },
    "login_attempts_per_minute": 10
  },
  "clone_urls": { "http": "https://kasu.example", "ssh": "git@kasu.example:" },
  "default_email_domain": "users.kasu.example",
  "limits": {
    "file_view_bytes": 1048576, "highlight_bytes": 524288, "highlight_lines": 20000,
    "blame_lines": 20000, "diff_files": 100, "diff_lines": 2000, "dir_entries": 5000,
    "upload_bytes": 10485760, "upload_total_bytes": 33554432, "highlight_concurrency": 4
  },
  "cache": { "bytes": 268435456 },
  "session": { "idle_secs": 7200, "max_secs": 43200 }
}
```

`anonymous.account` にはサーバー側 delegate の `anonymous_account` を指定します。

ヘルスチェック: `GET /-/healthz`（プロセス）、`GET /-/readyz`（kasu-server への疎通）、`GET /-/metrics`（Prometheus。画面種別・ステータスごとの要求数とレイテンシ、上流エラー、ログイン、キャッシュヒット）。リポジトリ名・アカウント名はラベルにしません。

## サーバーの設定

`KASU_ACCOUNTS_FILE` に `delegates` といくつかのアカウント項目が加わります（詳細は [リポジトリアカウント](repository-accounts_ja.md)）。

```json
{
  "accounts": [
    { "name": "alice", "token_file": "alice.pat", "display_name": "Alice", "email": "alice@example.com",
      "create_repositories": ["team-a/*"],
      "grants": [{ "repositories": ["team-a/**"], "write_policy": {}, "admin": true }] },
    { "name": "public-reader", "token_file": "public.pat", "grants": [{ "repositories": ["public/**"] }] }
  ],
  "delegates": [
    { "name": "kasu-web", "token_file": "kasu-web.pat", "act_as": ["*"], "allow_admin": true,
      "allow_unbound": false, "anonymous_account": "public-reader", "trusted_peers": ["10.0.20.0/24"] }
  ]
}
```

サーバー再起動後も Web セッションを維持するには `KASU_BINDING_KEY_FILE`（32 bytes 以上、全サーバーレプリカで共通）を設定します。未設定の場合はプロセスごとにランダムな鍵を使います。

## サインイン

| 方式 | アカウントの決まり方 | 書き込み |
|---|---|---|
| PAT ログイン（`/-/login`） | kasu-server が `Identity.WhoAmI` で PAT を検証。セッションにはアカウントとサーバーが計算した PAT の *binding* を保存し、PAT 自体は保存しない | アカウントの権限内 |
| リバースプロキシ | `trusted_peers` からの `user_header` のみ。`accounts.map`、または `default_from_user` ならそのままアカウント名として使う | アカウントの権限内（サーバー側 delegate に `allow_unbound` が必要） |
| 匿名 | 設定した匿名アカウント | 不可（サーバーが匿名の代理を読み取り専用に固定） |

優先順位はプロキシヘッダー（信頼ピアのみ）→ セッション Cookie → 匿名です。信頼ピア以外からのヘッダーは無視し、ログに記録します。運用者トークン、射影アカウント、Objects hop token ではサインインできません。

セッションは署名付き Cookie（`__Host-kasu_session`、`Secure; HttpOnly; SameSite=Lax`）で、最終利用から 2 時間、サインインから 12 時間で失効します。ログアウトは Cookie を削除します。あるアカウントの全セッションを即時に終わらせるには PAT を差し替えます。binding が一致しなくなり、kasu-server が次の要求でセッションを拒否します。ログイン試行は接続元アドレスごとに制限します。

## URL

最初の `-` セグメントでリポジトリ名（名前に `-` 単独のセグメントは含まれない）と画面を分けます。

```
/                                  リポジトリ一覧         /-/login  /-/logout  /-/preferences  /-/new
/<repo>                            default branch と README
/<repo>/-/tree/<rev>/<path>        ディレクトリ           /<repo>/-/blob/<rev>/<path>    ファイル
/<repo>/-/raw/<rev>/<path>         生データ               /<repo>/-/blame/<rev>/<path>   blame（?moves=1&copies=1）
/<repo>/-/log/<rev>/<path>         履歴                   /<repo>/-/changeset/<id>       Changeset（?parent=n）
/<repo>/-/compare/<base>...<head>  three-dot 比較（`..` は直接比較）
/<repo>/-/refs                     branch と tag          /<repo>/-/git/<oid>            Git OID → Changeset
/<repo>/-/edit|new|upload|delete/<rev>/<path>             Web からの書き込み
/<repo>/-/reviews[/new|/<n>[/files|/changesets]]          レビュー
/<repo>/-/settings                 管理
```

`<rev>` は 1 セグメントで、64 桁 16 進の Changeset ID、完全な ref 名（`refs/...`）、branch 名または tag 名です。`/` を含む予約文字はすべてパーセントエンコードします（`feature%2Fx`）。`<path>` は残り全体をバイト単位でエンコードするため、非 UTF-8 のパスも往復します。ref で表示中の画面は Changeset ID のパーマリンクを示します（`y` キーでも移動）。Changeset ID 指定の画面は `Cache-Control: private, max-age=3600`、それ以外は `private, no-cache` です。

## 閲覧

- ディレクトリ一覧（ディレクトリが先）、そのディレクトリの最新の変更、README。
- 行番号と範囲選択（`#L10-L20`）付きのファイル表示、上限内のハイライト、ソース表示に切り替えられる Markdown、画像（PNG / JPEG / GIF / WebP）のインライン表示、それ以外のサイズ・実行可能ビット・Git blob OID。symlink はリンク先（リポジトリ内ならリンク）、gitlink はサブモジュールの commit を表示します。
- 由来の Changeset ごとにまとめた blame と、由来の親での blame へのリンク。
- cursor によるページングの履歴、マージ表示、インポートされた Changeset の Git commit OID。
- ページ分割したファイル一覧、unified / side-by-side（利用者設定）の diff、前後 3 行の文脈、大きいファイルの折りたたみと遅延読み込みを備えた Changeset 表示と比較。
- `read_directories` を持つアカウントには「ディレクトリ限定ビュー」のバッジを出し、範囲外は表示しません。
- ディレクトリの各 entry の最終変更（先頭 100 件）をページ表示後に読み込みます。JavaScript なしでは `?latest=1` で表示します。
- `/<repo>/-/archive/<rev>/<path>` はディレクトリの `tar.gz`（`?format=zip` で zip）をダウンロードします。サーバーの `kasu.v1.Archive` RPC から呼び出し元の読み取り範囲内でストリーミングします（最大 100,000 entry・2 GiB。gitlink は含めない）。
- 同名の branch・tag がなく、一致する Changeset が 1 つだけなら、短縮 Changeset ID（4〜63 桁の 16 進）を `<rev>` に使えます。

## ブラウザからの書き込み

branch 上でファイルの編集・新規作成・アップロード・削除ができます。フォームは blob をアップロードし、表示中の Changeset を親とする Changeset を作成してから、次のいずれかを行います。

1. branch へ **直接着地**（`Land`。先端が進んでいれば rebase し、競合時は内容を保持したままフォームに戻る）
2. **branch を作成**してそこへ着地
3. **レビューを作成**（保護された branch では既定かつ唯一の選択肢）

改行コード・末尾改行・実行可能ビットは保持し、変更もできます。改名はパスの変更で行います。各フォームはランダムな冪等キーと発行時刻を持つため、二重送信や応答喪失後の再送信でも着地は 1 回です。フォームを開いた後に branch 上で同じファイルが変わった場合は、サーバーが rebase する前に確認を求めます。refs 画面では branch の作成と削除（表示時の version を使う `UpdateRef`）ができます。force 更新と tag の置換は CLI / API で行います。

## 管理

grant に `admin: true` を持つアカウントには `/<repo>/-/settings` を表示します。default branch、レビューポリシー（最初の rule のフォーム）、Webhook、JSON による設定全体、状態（Active / ReadOnly / Archived）、使用量、改名、フォーク、削除（名前の再入力で確認）を扱えます。Webhook endpoint の追加・変更・削除は運用者のみ可能です。リポジトリ管理者は他の設定変更時に既存 endpoint を維持できます。変更はすべて表示時の `config_version` を伴い、他者の変更と競合した場合は上書きせずにエラーと送信内容を表示します。`create_repositories` を持つアカウントは `/-/new` で Webhook endpoint のないリポジトリを作成できます。

## レビュー

モデルと API は [レビュー](review_ja.md) を参照してください。UI は承認数付きの一覧、比較画面からの作成（スタックの開始位置を選ぶ）、概要（説明、満たしていない条件付きの Submit 状態、投票、タイムライン）、ファイル（任意のリビジョン、または 2 つのリビジョン間の差分。行コメントは任意の投票とともに公開するまで下書き）、スタックの Changeset を表示します。Submit（REBASE / SQUASH）、「target に追従」、Close / Reopen はボタンで行い、レビューの先端で編集すると新リビジョンとして追加できます。

## セキュリティ

- すべての応答に `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'; connect-src 'self'`、`X-Content-Type-Options: nosniff`、`Referrer-Policy: same-origin`、クロスオリジン分離ヘッダー、HTTPS 公開時は HSTS を付けます。
- テンプレートは既定でエスケープします。Markdown は pulldown-cmark で HTML 化し、生の HTML はテキストとして表示し、結果を ammonia の許可リスト（`http` / `https` / `mailto` と相対リンクのみ）に通します。
- raw ファイルは `text/plain` か `application/octet-stream` と `Content-Disposition: attachment` で配信し（PNG / JPEG / GIF / WebP のみ inline）、常に `Content-Security-Policy: sandbox` を付けます。SVG・HTML・PDF は inline にしません。
- 状態を変える要求は `POST` のみで、セッション（またはプロキシ利用者、訪問者 Cookie）・フォームの種類・発行時刻に結び付けた CSRF トークンと、同一オリジンの `Origin` / `Referer` 検査を要求します。
- 404 画面は、リポジトリ・ref・パス・Changeset が存在しないのか見えないのかによらず同一です。
- プロセス内キャッシュは、要求が代理したアカウントをキーに必ず含めます。

## 検証（2026-10-30）

- **自動テスト:** in-process の kasu-server と kasu-web による結合テスト（閲覧・書き込み・管理・レビュー・認証・CSRF・XSS ペイロード・キャッシュ分離・アーカイブ）、kasu-server の委譲とレビューのテスト、kasu-sim の Submit 競合シナリオ、URL 往復の proptest、`web_url`・`markdown` の fuzz target とシード再生。ワークスペース全体（311 テスト）が通ります。
- **PostgreSQL 18（実 DB）:** PostgreSQL ストアの全テスト。8 並行の書き込み、レビューと同一トランザクションの ref 更新、冪等な再送、消去を含むレビューストアのテストも含みます。release ビルドの `kasu-server`（PostgreSQL バックエンド）と `kasu-web` で、PAT サインイン、設定画面での `main` の保護、保護 branch の編集からのレビュー作成、別アカウントの承認、Submit までを通し、ref_log に `alice` が `kasu-web:alice` 経由で記録されることを確認しました。
- **JavaScript 無効のブラウザ:** `tools/webui-browser-check/run.py` が release バイナリを起動し、WebKitGTK 2.52・Chrome 156・Firefox 158 をページのスクリプト無効で操作します。各ブラウザで、設定画面での言語選択、匿名閲覧、PAT サインイン、設定画面でのレビューポリシー保存、保護ファイルの編集（レビュー作成）、コメント、Submit、着地した内容と履歴の確認までの 11 段階がすべて成功しました。`--page-js` では 3 つとも「page scripts did not run」で失敗し、検査自体の有効性と、CSP のもとで `app.js` が動くことを確認しています。
- **Fuzzing:** 30 分の libFuzzer キャンペーン（`cargo +nightly fuzz run`）で、`markdown` は 4,954,343 入力、`web_url` は 79,777,670 入力を実行し、問題は見つかりませんでした。それ以前の `web_url` の 2 件の報告はハーネスの誤検出（エンコードされた `/` が空のパス・名前セグメントを作るもので、Web UI は使用前に拒否する）で、性質を有効な名前とパスに限定し、両入力をシードとして残しています。
- **性能**（release ビルド、16 コア、SQLite、100 ファイルにわたる 100,000 Changeset の生成履歴）: 取り込み 157 秒。ウォーム時の画面レイテンシ 4〜83 ms（tree・log・パスの log・ファイル・blame・entry ごとの履歴）。100 人の同時訪問者による 1,000 件の混合要求はエラーなし、1,215 要求/秒（p50 74 ms、p95 100 ms、p99 117 ms）。500 KiB のファイルのハイライトは 0.87 秒（約 13,700 行/秒）、同時実行は最大 4。
- **Derived Data のコールドスタート:** 取り込み直後は、1,000 リビジョンを持つファイルの最初の blame や最初のパス履歴をその場で計算するため、10 秒の要求予算を超えることがあります。サーバーは計算を続けて結果を保存し、その間の画面は `Retry-After` 付きの `503` を返し、entry ごとの履歴は 5 秒以内に終わった分だけを表示します。取り込み直後の実測では、entry ごとの履歴は 5.1 秒で応答し（計算後は 83 ms）、最初の blame は取り込みの約 3 分後に利用可能になりました（0.6 秒、ウォーム時 47 ms）。

## ブラウザのチェックリスト

ロードマップの手動確認は、主要ブラウザでの表示・キーボード操作・JavaScript なしの操作を対象とします。`python3 tools/webui-browser-check/run.py` は、目視以外の下記項目をすべて自動化します（先に `cargo build --release -p kasu-server -p kasu-web -p kasu-ingest`）。`--page-js` は対照実験で、「page scripts did not run」で失敗しなければなりません。

| 項目 | WebKitGTK 2.52 | Chrome 156 | Firefox 158 |
|---|---|---|---|
| 画面の描画（tree・README・ハイライト付きファイル・レビュー） | 合格 | 合格 | 合格 |
| JavaScript なし: 設定・PAT サインイン・リポジトリ設定・編集→レビュー・コメント・Submit・履歴 | 合格 | 合格 | 合格 |
| キーボード: Tab で一覧のファイルへ移動し Enter で開く | 合格 | 合格 | 合格 |
| キーボード: Tab で行番号へ移動し Enter で行を選択（`#L1`、行の強調） | 合格 | 合格 | 合格 |
| キーボード: Tab で diff のファイル一覧へ移動し Enter でその diff へ移動 | 合格 | 合格 | 合格 |
| キーボードフォーカスの可視表示 | 合格（計算値） | 合格（計算値） | 合格（スタイルシートの規則。ヘッドレスのウィンドウはフォーカスを持たない） |
| JavaScript あり: CSP のもとでスクリプトが動く（entry ごとの履歴の遅延読み込み） | 合格 | 合格 | 合格 |
| 文字のコントラスト 4.5:1 以上（WCAG 2.1 AA）。ライト・ダーク両方の全色トークンの組 | 合格（全 24 組。`app.css` から計算） | | |

Safari はサポート対象外です。WebKitGTK は WebKit エンジンとして確認しています。

## 上限と非ゴール

上限は前述の設定の通りで、超えた場合はダウンロードのみの提供、ハイライトの省略、diff のページ分割や折りたたみになります。Web UI は競合解消、コード検索、Issue 管理や通知を提供せず、force push や tag の置換では CLI の代わりになりません。
