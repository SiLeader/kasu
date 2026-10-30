# 02. 認証・認可・セキュリティ

## 主体モデル

- 認可の主体は既存の **リポジトリアカウント**（`KASU_ACCOUNTS_FILE` の `accounts[].name`）である。Web UI 固有の
  ユーザー DB は持たない。利用者 1 人に 1 アカウントを割り当てる運用を前提とする（コメント・承認・ref_log の帰属のため）。
- kasu-web は自分の権限では何も読めない **委譲主体（delegate）** として kasu-server に接続し、
  要求ごとに「どのアカウントとして振る舞うか」を指定する。kasu-server はその代理を設定で許された範囲でのみ認める。
- 運用者 PAT（`KASU_TOKEN`）と射影専用アカウント、Objects hop token は Web UI ではログインできない。
  運用作業は CLI / API で行い、Web で管理を行う利用者には後述の管理権限を付与する。

| 認証方式 | 利用者の識別 | 代理対象アカウント | 書き込み |
|---|---|---|---|
| PAT ログイン | 入力された PAT を kasu-server が検証 | PAT の持ち主 | 可（アカウントの権限内） |
| リバースプロキシ | 信頼ピアからのヘッダー | 対応表で決定 | 可（アカウントの権限内） |
| 匿名閲覧 | なし | 設定した匿名アカウント | 不可（サーバーが強制） |

複数方式を同時に有効にできる。優先順位はプロキシヘッダー（信頼ピアからの場合のみ）→ セッション Cookie → 匿名。

## 委譲資格情報

### kasu-server の設定

`KASU_ACCOUNTS_FILE` に `delegates` を追加する（アカウントと同じファイル・同じ PAT 重複禁止規則）。

```json
{
  "accounts": [ ... ],
  "delegates": [
    {
      "name": "kasu-web",
      "token_file": "secrets/kasu-web.pat",
      "act_as": ["*"],
      "allow_admin": true,
      "allow_unbound": true,
      "anonymous_account": "public-reader",
      "trusted_peers": ["10.0.20.0/24"]
    }
  ]
}
```

| フィールド | 意味 |
|---|---|
| `act_as` | 代理できるアカウント名の glob。運用者・射影アカウントは対象外 |
| `allow_admin` | 代理時に Admin API と管理権限（後述）を使えるか。false なら代理要求の Admin 系は拒否 |
| `allow_unbound` | `kasu-act-binding` なしの代理（プロキシ認証）を許すか。PAT ログインのみなら false にする |
| `anonymous_account` | 匿名閲覧で代理するアカウント。`act_as` に含まれなくてよい |
| `trusted_peers` | この delegate の接続を受け付ける送信元 CIDR。空なら制限なし（非推奨、起動時に警告） |

### 要求の検証（kasu-server）

1. `authorization` が delegate の token で、送信元が `trusted_peers` 内であることを確認する。
2. `kasu-act-as` が必須。欠落・未知アカウント・`act_as` 外は `UNAUTHENTICATED`。
   delegate 単体（代理なし）ではどのリポジトリも読めない。
3. `kasu-act-binding` があれば、そのアカウントの現在の PAT から計算した値と一致を確認する（不一致は `UNAUTHENTICATED`）。
   ない場合は `allow_unbound` が必要。
4. 匿名アカウントとしての代理は **読み取り専用に固定** する（アカウントの `write_policy` にかかわらず Writer / Review の
   書き込み / Admin を `PERMISSION_DENIED`）。
5. 以後の認可・ポリシー・Land の冪等名前空間・ref_log の actor は、代理対象アカウントとして直接 PAT で来た場合と同一にする。
   監査用に delegate 名と `kasu-act-user` を tracing と ref_log の新しい補助列 `via` に記録する。

### admission

- 既存の admission は接続元 IP → 認証主体の順に消費する。delegate の接続元（kasu-web）は全利用者で共有されるため、
  `trusted_peers` から来た delegate 要求は peer IP の bucket を消費せず、検証後に `kasu-client-addr` の IP bucket と
  代理対象アカウントの bucket を消費する。`kasu-client-addr` は delegate からの要求でのみ解釈する。
- delegate 自体にも主体として同時実行上限を置く（`KASU_SERVER_LIMITS_FILE` に `per_delegate_active` を追加）。

## Identity API（新規）

```proto
service Identity {
  rpc WhoAmI(WhoAmIRequest) returns (Principal);
  rpc GetPermissions(PermissionsRequest) returns (Permissions);
}
message WhoAmIRequest {}
message Principal {
  string account = 1; bool anonymous = 2;
  bytes display_name = 3; bytes email = 4;
  bytes binding = 5;                       // PAT で直接呼んだ場合のみ。セッションに保存する
  repeated string create_repositories = 6; // 作成を許可された名前 glob
}
message PermissionsRequest { string repo = 1; repeated bytes refs = 2; }
message RefPermission { bytes name = 1; bool update = 2; bool create = 3; bool delete = 4; bool force = 5; bool review_required = 6; bool review_bypass = 7; }
message Permissions {
  bool read_filtered = 1; repeated bytes read_directories = 2;
  bool write = 3; repeated bytes allowed_path_prefixes = 4;
  repeated RefPermission refs = 5;          // 要求された ref ごとの実効権限（全体 ∧ リポジトリ ∧ アカウント）
  bool admin = 6; bool review = 7;
}
```

- `GetPermissions` は不可視リポジトリに対して `NOT_FOUND` を返す（存在の秘匿を維持）。
- 結果は UI のボタン表示にのみ使い、実際の判定は各 RPC が行う。
- アカウント設定に表示用の任意フィールド `display_name` / `email` を追加する（Web 編集時の署名の既定値）。

## PAT ログインとセッション

1. `GET /-/login` でフォームを表示。`POST /-/login` で PAT を受け取る（CSRF トークン付き）。
2. kasu-web はその PAT を Bearer にして `Identity.WhoAmI` を **直接**（委譲なしで）呼ぶ。
   成功すれば account と `binding` を得る。PAT は以後保存しない（メモリからも即破棄、ログに出さない）。
3. セッション Cookie を発行する。

```
__Host-kasu_session = base64url( version | account | binding | issued_at | expires_at | session_id ) . HMAC-SHA256(session_key, ...)
属性: Secure; HttpOnly; SameSite=Lax; Path=/
```

- `binding` は kasu-server が「サーバー秘密鍵による PAT の鍵付きハッシュ」として計算する。kasu-web は PAT も鍵も持たない。
  PAT を更新・削除するとサーバー側の検証で一致しなくなり、既存セッションは次の要求で無効になる。
- 有効期限: 最終利用から 2 時間のスライディング、発行から最長 12 時間（設定可能）。
- ログアウトは Cookie の削除のみ（ステートレスのため、盗まれた Cookie は期限まで有効。PAT 更新で即時失効できる）。
- ログイン試行には kasu-web 側でも IP 単位のレート制限（既定 10 回 / 分）をかける。
- `KASU_WEB_PUBLIC_URL` が `http://` の場合は `__Host-` と `Secure` を外す開発モードとし、起動時に警告する。

## リバースプロキシ認証

- 送信元が `auth.proxy.trusted_peers` のときだけ `user_header` を信頼する。それ以外の接続にヘッダーがあれば無視し、
  付いていたこと自体を警告ログに残す。
- 利用者 → アカウントの対応: `accounts.map` の完全一致を優先し、`default_from_user=true` なら利用者名をそのまま
  アカウント名として使う。対応がなければ匿名扱い（匿名無効なら 403）。
- セッション Cookie は発行しない（毎要求ヘッダーで識別）。CSRF トークンは利用者 ID に結び付けて検証する。
- `email_header` / `name_header` は Web 編集時の署名の既定値として使う（アカウント設定の値より優先）。

## 匿名閲覧

- `auth.anonymous.enabled=true` かつ kasu-server 側の delegate に `anonymous_account` がある場合に有効。
  見えるリポジトリは匿名アカウントの grant で決まる（専用の読み取り専用アカウントを用意する）。
- 匿名時は書き込み・レビュー投稿・管理の画面とリンクを出さず、該当 URL は 401 → ログインへ誘導する。

## 管理権限の拡張

現状 Admin API は運用者 PAT 専用である。Web から管理を委ねるため、アカウント設定に次を追加する。

```json
{
  "name": "team-a-lead",
  "token_file": "secrets/team-a-lead.pat",
  "create_repositories": ["team-a/*"],
  "grants": [
    { "repositories": ["team-a/**"], "read": true, "write_policy": {}, "admin": true }
  ]
}
```

| 操作 | 必要な権限 |
|---|---|
| `CreateRepository` | 名前が `create_repositories` に一致 |
| `GetRepository` / `ListRepositories` / `GetRepositoryUsage` | 既存どおり（可視なもの） |
| `UpdateRepository`（設定・状態） | 対象の grant に `admin: true` |
| `RenameRepository` | 対象に `admin` かつ新名が `create_repositories` に一致 |
| `DeleteRepository` | 対象に `admin` |
| `ForkRepository` | 元を読める、かつ新名が `create_repositories` に一致 |

- 現在 `config_json` は運用者にだけ返している。`admin` を持つアカウントにも返すよう変更する。
- 管理権限はリポジトリ設定（`config_json`）経由でもサーバー全体の policy を拡大できない（既存の積の規則）。
  ただし `webhooks[].secret_file` はサーバー上の任意ファイルを指せるため、非運用者が設定する場合は
  サーバー設定 `KASU_WEBHOOK_SECRET_DIR` 配下の相対パスに限定する。
- `KASU_REPOSITORIES_SOURCE` がファイルを正とする構成では、Web からの設定変更は既存どおり `FAILED_PRECONDITION` になる。
  UI はその旨を表示する。

## CSRF

- 状態を変える要求はすべて `POST`（`GET` で副作用を起こさない）。
- 各フォームに CSRF トークンを埋め込む: `HMAC(session_key, session_id or proxy_user | form_kind | issued_at)`。
  有効期限 2 時間。不一致は 403。
- `Origin`（なければ `Referer`）が `KASU_WEB_PUBLIC_URL` と一致することも要求する。
- Land / Submit / CreateReview / 書き込みフォームには別途ランダムな冪等キーを hidden で埋め込み、二重送信を同一結果にする。

## セキュリティヘッダー

全応答に付与する。

```
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:;
                         form-action 'self'; frame-ancestors 'none'; base-uri 'none'; connect-src 'self'
X-Content-Type-Options: nosniff
Referrer-Policy: same-origin
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Resource-Policy: same-origin
Strict-Transport-Security: max-age=31536000   （HTTPS 公開時）
```

## リポジトリ内容の安全な表示

リポジトリの内容・パス・コミットメッセージ・ref 名・レビュー本文はすべて攻撃者が制御し得る入力として扱う。

- HTML 表示ではテンプレートの自動エスケープを必須とし、属性値・URL も文脈に応じてエスケープする。
- Markdown（README・レビュー本文・コメント）は `pulldown-cmark` で HTML 化し、`ammonia` の許可リストでサニタイズする。
  `<script>` / イベント属性 / `style` / `iframe` / `form` は除去、リンクは `http` / `https` / `mailto` と相対のみ。
  相対リンクは同じ rev の blob / tree URL、相対画像は raw URL に書き換える。外部画像は CSP で読み込まれない（リンクとして表示）。
- raw 配信（`/-/raw/`）:
  - 既定は `Content-Type: text/plain; charset=utf-8`（テキスト）または `application/octet-stream`（バイナリ）、
    `Content-Disposition: attachment`。
  - 画像（PNG / JPEG / GIF / WebP）のみ実際の型で `inline` 配信する（UI の `<img>` 表示用）。SVG・HTML・PDF は常に attachment。
  - raw 応答には `Content-Security-Policy: sandbox; default-src 'none'` を付ける。
  - 任意: `KASU_WEB_RAW_ORIGIN` に別オリジン（例 `https://raw.kasu.example`）を設定すると raw をそこから配信し、
    Cookie を共有しない。raw オリジンでは URL に短期の署名付き読み取りトークンを付ける。

## 監査

- ログイン成功 / 失敗、プロキシヘッダーの拒否、全ての書き込み・管理・レビュー操作を tracing に記録する
  （アカウント、`kasu-act-user`、接続元、対象リポジトリ、結果）。PAT・Cookie・CSRF トークンは記録しない。
- 永続的な監査は kasu-server 側（ref_log の `via`、repo_log、review_log）を正とする。
