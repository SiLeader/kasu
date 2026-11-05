# 認証情報ヘルパー 実装計画

`kasu` CLI が PAT（Personal Access Token）を記憶し、毎回 `--token` / `KASU_TOKEN` を指定しなくても
サーバーへ認証できるようにするための仕様と実装計画である。保存先は Git の credential helper や
Docker の credential helper と同じく **差し替え可能なヘルパー（プラグイン）** とし、OS の秘密情報ストアを優先して使う。

## 背景

- 現在の CLI は PAT を `--token` または環境変数 `KASU_TOKEN` からのみ受け取り、working copy にも保存しない
  （`crates/kasu-cli/src/main.rs` の `Cli::token`、[native-cli](../../native-cli_ja.md)）。
- 対話利用では毎回の環境変数設定が煩雑で、シェル履歴や `.bashrc` に平文の PAT が残りやすい。
- 一方で CI・bot では従来どおり環境変数で渡す運用を壊してはならない。

## 確定した決定事項

| 項目 | 決定 |
|---|---|
| 方式 | プラグイン式の credential helper。組み込みヘルパーと外部実行ファイル（`kasu-credential-<name>`）を同じ抽象で扱う |
| 組み込みヘルパー（macOS） | `osxkeychain`（macOS キーチェーン。git / docker の osxkeychain と同じ仕組み） |
| 組み込みヘルパー（Linux） | `secretservice`（freedesktop Secret Service API。GNOME Keyring / KWallet 等）、`pass`（password-store） |
| 組み込みヘルパー（OS 共通） | `file`（平文ファイル保存） |
| CLI | `kasu login` と `kasu logout` を追加する |
| 既存の指定方法 | `--token` / `KASU_TOKEN` は残し、ヘルパーより常に優先する |

## 本書で置いた既定（変更可能）

| 項目 | 既定 | 章 |
|---|---|---|
| 新クレート | `crates/kasu-credential`（プロトコル・trait・組み込みヘルパー）。`kasu-cli` が依存する | [01](01-architecture.md#クレート構成) |
| 認証情報のキー | 正規化したサーバー URL（`scheme://host:port`、unix ソケットは絶対パス） | [01](01-architecture.md#認証情報のキー) |
| ヘルパーの既定 | macOS は `osxkeychain`、Linux は `secretservice`。利用不可なら平文へ黙って落とさずエラーにする | [01](01-architecture.md#ヘルパーの選択) |
| 設定ファイル | `$XDG_CONFIG_HOME/kasu/config.json`（未設定なら `~/.config/kasu/config.json`） | [01](01-architecture.md#設定ファイル) |
| 外部プラグインのプロトコル | 標準入出力の JSON。`get` / `store` / `erase` / `list` | [01](01-architecture.md#ヘルパープロトコル) |
| ログイン時の検証 | `Identity.WhoAmI` で PAT を検証してから保存し、アカウント名を一緒に保存する | [03](03-cli.md#kasu-login) |
| PAT の入力 | TTY ではエコーなしのプロンプト、非 TTY では `--token-stdin`。コマンドライン引数では受け取らない | [03](03-cli.md#kasu-login) |

## ドキュメント構成

| ファイル | 内容 |
|---|---|
| [01-architecture.md](01-architecture.md) | クレート構成、認証情報のキー、トークン解決順序、ヘルパーの選択、設定ファイル、ヘルパープロトコル |
| [02-helpers.md](02-helpers.md) | 組み込みヘルパー（osxkeychain / secretservice / pass / file）の保存形式と実装 |
| [03-cli.md](03-cli.md) | `kasu login` / `kasu logout` の仕様、既存コマンドへの組み込み、エラー表示 |
| [04-roadmap.md](04-roadmap.md) | フェーズ分割、完了条件、テスト戦略、セキュリティ、リスク |

## ゴール

1. `kasu login <server>` を一度実行すれば、以降の `clone` / `checkout` / `commit` / `land` / `review` / `repo` 等が
   トークン指定なしで動作する。
2. macOS ではキーチェーン、Linux では Secret Service または pass に PAT を保存し、平文では保存しない（`file` を明示選択した場合を除く）。
3. ヘルパーは外部実行ファイルとして追加でき、組み込みヘルパーと同じプロトコル・同じ CLI で扱える。
4. `--token` / `KASU_TOKEN` を使う既存の CI・スクリプトの挙動を一切変えない。

## 非ゴール

- Windows 資格情報マネージャー（CLI 自体が Unix 専用のため対象外）。
- OAuth / デバイスフロー等による PAT の自動発行。PAT の発行・失効はサーバー側の既存運用（`KASU_ACCOUNTS_FILE`）のまま。
- Git の credential helper としての動作（`git-credential-kasu`）。Git 互換 HTTP の認証連携は将来検討とする（[04](04-roadmap.md#将来の拡張)）。
- サーバー側の変更。既存の `Identity.WhoAmI` のみを利用する。
- 常駐プロセスを使う一時保存（git-credential-cache 相当の `memory` ヘルパー）。常駐プロセスを持たない方針のため組み込みでは提供しない。
  必要なら外部ヘルパーとして実装できる。
