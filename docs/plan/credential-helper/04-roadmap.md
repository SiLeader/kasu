# 04. ロードマップ・テスト戦略・リスク

各フェーズは単独でマージ可能な状態を完了条件とする。`--token` / `KASU_TOKEN` だけを使う既存の利用者・CI の挙動は
全フェーズを通して変えない。サーバーの変更はない。

## マイルストーン

### C0: 基盤と file ヘルパー（0.5 週）
- `crates/kasu-credential` を作成。`ServerKey`、`Credential`、`Store` trait、エラー型、`config.json` の読み書き。
- `file` ヘルパー（権限・原子的書き込み・flock）。
- 依存追加: `zeroize`（MIT / Apache-2.0）。`rustix` の feature 追加（`termios`、必要なら `process`）。
- 完了条件:
  - ServerKey の正規化が冪等で、同一サーバーを表す URL 表記揺れが同じキーになること（プロパティテスト）。
  - `file` の作成直後のファイル権限が umask によらず 0600 であること。並行する 16 プロセスの `store` で JSON が壊れないこと。

### C1: CLI 統合と login / logout（1 週）
- `kasu login` / `kasu logout`、隠しサブコマンド `credential`。
- `main.rs` のトークン解決を `token_for` に置き換え、`UNAUTHENTICATED` の案内メッセージを追加。
- 完了条件:
  - in-process の kasu-server（accounts 設定あり）に対し、`login --token-stdin --helper file` → `clone` → `commit` → `land` が
    `KASU_TOKEN` なしで成功すること。
  - `KASU_TOKEN` 設定時にヘルパーが一切呼ばれないこと（呼ばれると失敗するテスト用外部ヘルパーで検証）。
  - 無効な PAT では保存されないこと、`--no-verify` では保存されること。
  - `logout` が冪等で、2 回目も終了コード 0 であること。

### C2: osxkeychain（0.5 週）
- `security-framework` による実装（macOS のみビルド）。
- 完了条件:
  - CI の `native-cli-macos` ジョブで、一時キーチェーン（`security create-keychain` → 検索リストに追加 → 既定に設定）を作り、
    store / get / erase / list の往復とアカウント名変更時の置き換えを検証すること。
  - `list` がキーチェーンのデータ（秘密値）を読まないこと。

### C3: secretservice と pass（1 週）
- `secret-service` クレートによる実装（Linux のみビルド）、`pass` コマンド経由の実装。
- 完了条件:
  - Linux CI で `dbus-run-session` + `gnome-keyring-daemon --unlock`（空パスワード）を起動し、secretservice の往復を検証すること。
  - Linux CI でテスト用 GPG 鍵（パスフレーズなし）と一時 `PASSWORD_STORE_DIR` を用意し、pass の往復と unix ソケットキーの
    エントリ名エンコードを検証すること。
  - D-Bus セッションなし・pass 未初期化で `NotAvailable` と案内メッセージになること。
  - 上記は外部プロセスを要するため `#[ignore]` とし、専用 CI ジョブで `--ignored` 実行する（既存の外部サービス系テストと同じ扱い）。

### C4: 外部ヘルパーとドキュメント（0.5 週）
- `kasu-credential-<name>` の探索・起動・タイムアウト・応答上限。
- テスト用の外部ヘルパー（`tests/` 内でビルドする小さなバイナリ）で、組み込みヘルパーとの互換テスト。
- 完了条件:
  - 不正 JSON・非 0 終了・タイムアウト・64 KiB 超過がすべてエラーになり、秘密値がエラーメッセージに含まれないこと。
  - `kasu credential file <op>` と外部ヘルパーが同じ入力に同じ応答を返すこと。

## ドキュメント

- 新規: `docs/credential-helpers.md` / `docs/credential-helpers_ja.md`（使い方、各ヘルパーの保存先とセットアップ、
  外部ヘルパーのプロトコル仕様、トラブルシューティング）。言語切替行を先頭に置く。
- 更新: `docs/native-cli.md` / `_ja`（「トークンは環境変数または `--token` で渡し」の記述をヘルパー対応に改め、
  `login` / `logout` をコマンド表に追加）、`README.md` のクイックスタート（`kasu login` の例）、
  `docs/implementation-status.md` / `_ja`。
- 本計画（`docs/plan/credential-helper/`）は日本語のみ。

## テスト戦略

| 種別 | 内容 |
|---|---|
| 単体 / プロパティ | ServerKey 正規化（冪等・表記揺れ・userinfo 拒否・unix パス）、pass エントリ名の往復、プロトコル型の serde 往復、config の version 処理 |
| 結合（ヘルパー） | 各ヘルパーで store → get → list → erase → get(None) の共通テストスイートを `Store` trait に対して書き、全ヘルパーで同じテストを回す |
| 結合（CLI） | in-process kasu-server に対して `assert_cmd` 相当（既存テストのバイナリ起動方式に合わせる）で login / 各コマンド / logout。`KASU_CONFIG_DIR` を一時ディレクトリにして利用者環境を汚さない |
| セキュリティ | 秘密値が stdout・stderr・tracing 出力・エラーメッセージ・`Debug` 出力・コマンドライン引数に現れないことを、既知の PAT 文字列の検索で確認 |
| プラットフォーム | macOS ジョブ（osxkeychain・file）、Linux ジョブ（secretservice・pass・file） |
| ファジング | 外部ヘルパー応答の JSON パーサと config.json パーサを `fuzz/` に追加 |

テストでは利用者の実キーチェーン・実 Secret Service・`~/.password-store` に絶対に触れないよう、
`KASU_CONFIG_DIR`・`KASU_CREDENTIALS_FILE`・`PASSWORD_STORE_DIR`・一時キーチェーン・`dbus-run-session` で必ず隔離する。

## セキュリティ上の要点

- 秘密値はコマンドライン引数・環境変数（ヘルパー起動時）・ログに出さない。`Zeroizing` で保持する。
- 平文保存は `file` を明示選択した場合のみ。既定ヘルパーが使えなくても平文に落とさない。
- `pass` のエントリ名・外部ヘルパー名は検証済みの文字種に限定し、パストラバーサル・コマンド注入を防ぐ。
- ヘルパー由来のトークンも既存どおり `kasu_object::bearer` で検証し、HTTP（非 TLS）のリモートへ送る場合は現行と同じ扱いとする
  （本計画で警告を追加するかは C1 で判断: `http://` かつ非ループバックへのヘルパー由来トークン送信時に警告する案）。

## 主要リスクと対策

| リスク | 影響 | 対策 |
|---|---|---|
| ヘッドレス Linux で secretservice が使えない | 既定の login が失敗する | 明確なエラーと pass / file の案内。SSH 先向けの手順を文書化 |
| キーチェーン・Secret Service のプロンプトが CI で止まる | ジョブがハング | `KASU_TOKEN` 設定時はヘルパーを呼ばない、`KASU_CREDENTIAL_HELPER=none`、ヘルパー呼び出しのタイムアウト |
| バイナリ更新ごとの macOS キーチェーン許可ダイアログ | 利用者の手間 | git と同じ既知の挙動として文書化。署名済みリリースバイナリでは同一署名なら再確認されにくい旨を記載 |
| `secret-service` の依存ツリーが大きい（zbus 等） | ビルド時間・供給網 | Linux ターゲット限定、feature 最小化、`cargo deny` で確認。重すぎる場合は D-Bus を直接扱う薄い実装に切り替える判断を C3 開始時に行う |
| 古いサーバーに `Identity.WhoAmI` がない | login の検証ができない | `--no-verify` を案内。検証なしでもアカウント名なしで保存可能 |
| 外部ヘルパーの品質 | 認証情報の漏えい・ハング | プロトコルにタイムアウト・応答上限、組み込み名の優先、プロトコル仕様と互換テストの公開 |

## 将来の拡張

- `kasu auth status`（保存済みサーバー・アカウント・ヘルパーの一覧と WhoAmI による有効性確認）。
- Git 互換 HTTP 向けに `git-credential-kasu` を提供し、`git clone https://kasu.example.com/...` で同じ PAT を使えるようにする。
- PAT の有効期限がサーバーで導入された場合の期限表示と再ログイン案内。
- Docker credential helper（`docker-credential-*`）をアダプター経由で流用するオプション。
