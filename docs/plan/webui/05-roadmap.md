# 05. ロードマップ・テスト戦略・リスク

Web UI はメイン計画の M13 として、W0〜W7 の段階で入れる。各段階は単独でリリース可能な状態を完了条件とし、
既存の kasu-server の挙動（GitOps 互換・既存 API）を変えない順序にする。サーバー側の変更（W1・W4・W5）と
kasu-web 側の変更は別コミットにする。

## マイルストーン

### W0: kasu-web の基盤（1 週）
- `crates/kasu-web` の作成、設定読み込み、axum ルーター、askama テンプレート、埋め込みアセット、i18n の骨組み。
- gRPC クライアント（`upstream.rs`）とエラー写像、共通 404 / エラー画面、セキュリティヘッダー、healthz / readyz / metrics。
- 開発モード（kasu-server が認証なしの場合）でリポジトリ一覧だけを表示する。
- 完了条件: in-process の kasu-server と kasu-web を起動する結合テスト基盤。全応答に CSP 等のヘッダーがあること。

### W1: 認証と委譲（1.5 週）
- サーバー: `delegates` 設定、委譲メタデータの検証、匿名代理の読み取り専用固定、`binding`、admission の
  `kasu-client-addr` / `per_delegate_active`、ref_log の `via`、`Identity`（WhoAmI / GetPermissions）、アカウントの
  `display_name` / `email`。
- kasu-web: PAT ログイン / ログアウト、セッション Cookie、プロキシヘッダー認証、匿名閲覧、CSRF。
- 完了条件:
  - delegate 単体・`act_as` 外・`trusted_peers` 外・binding 不一致がすべて拒否されること。
  - PAT 更新で既存セッションが次の要求で失効すること。
  - 匿名代理では、書き込み権限のあるアカウントを匿名アカウントに指定しても全書き込みが拒否されること。
  - 代理要求の認可結果が、同じアカウントの PAT による直接要求と全 RPC で一致すること（プロパティテスト）。

### W2: 閲覧（2 週）
- リポジトリ一覧・トップ、tree、blob、raw、blame、log、changeset、compare、refs、Git OID リダイレクト。
- rev / path の URL エンコードと解決、diff コンポーネント、ハイライト、Markdown、表示上限、HTTP キャッシュ、プロセス内 LRU。
- 完了条件:
  - rev / path（非 UTF-8・`%`・`/` を含む ref 名を含む）の URL 往復一致のプロパティテスト。
  - 不可視リポジトリ・未所属 ID・不存在で 404 応答のバイト列が同一。
  - `read_directories` アカウントで範囲外のパスが全画面に現れないこと。
  - 悪意あるファイル名・コミットメッセージ・ref 名・Markdown・SVG による XSS テストケースがすべて無害化されること。
  - アカウント A のキャッシュ内容がアカウント B に返らないこと。

### W3: Web からの書き込み（1.5 週）
- 編集・新規・アップロード・削除 → `UploadBlobs` / `CreateChangeset` / `Land`。branch 作成・削除。
- `GetPermissions` に基づくボタン表示、冪等キーによる二重送信対策、Conflict 時の再編集。
- 完了条件:
  - 改行コード・末尾改行・実行可能ビット・非 UTF-8 パスが編集で保持されること。
  - 同じフォームの二重送信で Changeset / ref 更新が 1 回だけであること。
  - path ACL 外の変更がサーバーで拒否され、UI に理由が出ること。

### W4: リポジトリ管理（1 週）
- サーバー: grant の `admin`、アカウントの `create_repositories`、`config_json` の管理者への返却、
  `KASU_WEBHOOK_SECRET_DIR` による `secret_file` 制限。
- kasu-web: 作成画面、設定画面（一般・書き込みポリシー・上限・Webhook・状態・改名・フォーク・使用量・削除）。
- 完了条件: 管理権限の各操作の認可マトリクステスト。`config_version` CAS 競合時の再編集。
  `KASU_REPOSITORIES_SOURCE=file` での変更拒否表示。

### W5: レビュー基盤（サーバー、2.5 週）
- `ReviewStore`（SQLite / PostgreSQL）、`kasu.v1.Review`（gRPC + HTTP/JSON）、`review` ポリシー、承認判定、
  指紋、Submit（REBASE / SQUASH、1 トランザクション確定）、RebaseReview、保護 ref の強制（Land / UpdateRef / receive-pack）、
  リポジトリ削除時の範囲削除。
- 完了条件:
  - `review` 設定がないリポジトリで既存テストスイート（GitOps E2E を含む）が無変更で通ること。
  - 保護 ref への直接更新が全経路（gRPC / HTTP JSON / Git HTTP / SSH）で拒否され、bypass アカウントは通ること。
  - Submit と並行する push / Land / 投票取り消し / ポリシー変更の決定的シミュレーション（`kasu-sim`）で、
    承認条件を満たさない状態での着地・二重着地・SUBMITTED と ref の不整合が起きないこと。
  - 冪等キーでの再送、応答喪失、サーバー再起動後の再送で同じ結果になること。

### W6: レビュー UI（2 週）
- 一覧、作成、概要、files（リビジョン指定・リビジョン間比較）、changesets、行コメント（下書き → 公開）、スレッド解決、
  投票、Submit、target への追従、Web 編集からのレビュー作成・リビジョン追加。
- 完了条件: JS 無効でもコメント・投票・Submit ができること。古いリビジョンのコメントの再配置規則のテスト。

### W7: 拡張
- ディレクトリ表示の entry ごとの最終変更（JS による遅延読み込み、件数上限付き。必要なら専用 RPC）。
- tar / zip アーカイブのダウンロード（サーバー側ストリーミング RPC の追加が前提）。
- レビューイベントの Webhook 配信（`review_log` を outbox に）、`kasu review` CLI。
- 短縮 Changeset ID（前方一致索引）、CODEOWNERS 相当のパス別承認者、コード検索の検討。

## ドキュメント

各段階で利用者向け文書を英語・日本語で追加・更新する（`docs/web-ui.md` / `docs/web-ui_ja.md`、
`docs/review.md` / `docs/review_ja.md`）。あわせて `README.md` / `README_ja.md` の表、`docs/repository-accounts*.md`
（delegates・admin・プロフィール）、`docs/multi-repository*.md`（`review` 設定）、`docs/server-runtime*.md`
（delegate の admission）、`docs/implementation-status*.md`、`docs/plan-audit.json` を更新する。
設計計画では `../README.md` の非ゴール、`../01-architecture.md` の構成図、`../03-protocols-and-api.md` の API 表、
`../05-roadmap.md` に M13 を反映する。

## テスト戦略

| 種別 | 内容 |
|---|---|
| 単体 / プロパティ | URL の rev / path 往復、セッション Cookie と CSRF トークンの改ざん検出・期限、指紋の正規化、承認判定（ランダムな投票列・リビジョン列・ポリシー） |
| 結合（HTTP） | in-process の kasu-server + kasu-web に `tower::ServiceExt::oneshot` で要求し、`scraper` で HTML を検査。各画面の正常系・権限別・上限超過 |
| 認可マトリクス | 運用者 / 管理者 / 書き込み / 読み取り / ディレクトリ限定 / 匿名 / プロキシ × 全画面・全 POST の期待結果を表で定義して網羅 |
| 隔離 | 複数リポジトリ・共有 blob で、kasu-web 経由の全画面が他リポジトリ専用オブジェクトを返さず、404 が区別不能 |
| セキュリティ | XSS ペイロード集（パス・ref・メッセージ・Markdown・SVG・HTML ファイル）、CSRF（Origin 欠落・不一致・トークン再利用）、プロキシヘッダーの偽装、raw 配信のヘッダー |
| 決定的シミュレーション | `kasu-sim` に Review（Submit・投票・ポリシー変更・push）の並行シナリオを追加 |
| ファジング | URL パーサ、Markdown → サニタイズ（出力に許可外の要素・属性が残らないこと）を `fuzz/` に追加 |
| 手動確認 | 主要ブラウザ（Chrome / Firefox。Safari はサポート対象外）での表示・キーボード操作・JS 無効時の動作のチェックリスト。ブラウザ自動化は Node 依存を避けるため初期は導入しない |
| 性能 | 10 万 Changeset のリポジトリでの log / blame / tree 表示のレイテンシ、ハイライトのスループット、同時 100 利用者の負荷 |

## 主要リスクと対策

| リスク | 影響 | 対策 |
|---|---|---|
| リポジトリ内容による XSS | セッション乗っ取り・代理操作 | 自動エスケープ、ammonia、厳格な CSP（inline script なし）、raw の attachment / sandbox、任意の raw 別オリジン、ファジング |
| 委譲資格情報の漏えい | `act_as` 範囲の全アカウントになりすまし | `trusted_peers` の必須化推奨、`act_as` の最小化、`allow_admin` の分離、専用 token・定期更新、`via` による監査 |
| ステートレスセッションの即時失効不可 | ログアウト後も Cookie が期限まで有効 | 短い期限、PAT 更新による `binding` 失効、HttpOnly / Secure / SameSite |
| アカウント共有による帰属の曖昧さ | コメント・承認の主体が個人を表さない | 1 利用者 1 アカウントを運用要件として文書化、`kasu-act-user` を監査に記録 |
| 保護 ref の強制が GitOps 書き戻しを止める | Image Automation 等の push が失敗 | 既定は保護なし、`bypass_accounts`、拒否メッセージの明示、E2E で bot の書き戻しを検証 |
| 大規模リポジトリ・巨大 diff | 応答遅延・メモリ逼迫 | 表示上限、diff の段階取得、ハイライトの同時実行制限、ページング |
| アカウント間のキャッシュ漏えい | `read_filtered` の範囲外が見える | キャッシュキーにアカウントを含める、`Cache-Control: private`、隔離テスト |
| trivial rebase 判定の誤り | 意味の変わった変更に承認が残る | 規則を文書化し rule で無効化可能、リビジョン間比較で base の違いを明示 |
| Submit とレビュー状態の不整合 | 着地したのに OPEN、または逆 | ref CAS とレビュー更新を同一 Tx、能力がないバックエンドでは Review API を無効化、シミュレーション |
| 依存クレートの増加 | 供給網・ライセンス | 依存を最小化（JS は自前・npm なし）、`cargo deny` を CI で強制 |
