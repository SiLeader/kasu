# 02. 組み込みヘルパー

| 名前 | 対応 OS | 保存先 | 永続 | 暗号化 | 既定 |
|---|---|---|---|---|---|
| `osxkeychain` | macOS | ログインキーチェーン | ○ | ○（OS） | macOS |
| `secretservice` | Linux | Secret Service の既定コレクション | ○ | ○（OS） | Linux |
| `pass` | Linux（macOS でも動作） | `~/.password-store/kasu/...` | ○ | ○（GPG） | — |
| `file` | 共通 | `~/.config/kasu/credentials.json` | ○ | ×（平文） | — |

対応 OS 以外で名前を指定した場合は `NotAvailable` を返す（ビルド対象から外し、名前だけ認識する）。

## osxkeychain

git の `git-credential-osxkeychain`、docker の `docker-credential-osxkeychain` と同じく Security.framework のキーチェーン項目を使う。

- 依存: `security-framework`（MIT / Apache-2.0）。`[target.'cfg(target_os = "macos")'.dependencies]` に限定する。
- 項目の種類は **generic password**（`kSecClassGenericPassword`）。

| 属性 | 値 |
|---|---|
| service | `kasu:` + ServerKey（例 `kasu:https://kasu.example.com:443`） |
| account | アカウント名（検証省略時は `kasu`） |
| label | `kasu (https://kasu.example.com:443)` |
| data | PAT（UTF-8） |

- `get`: `ItemSearchOptions` で class=generic password、service 一致、`load_attributes` / `load_data` を指定し 1 件取得する。
  `errSecItemNotFound` は `None`。
- `store`: 同じ service の既存項目を削除してから追加する（アカウント名が変わった場合に古い項目が残らないようにする）。
  削除と追加の間に失敗した場合はエラーを返し、再実行で回復できる。
- `erase`: service 一致の項目をすべて削除する。
- `list`: service が `kasu:` で始まる項目の属性のみ取得する（data は読まない。プロンプトを避けるため）。
- アクセス制御は既定（項目を作成したアプリケーション = `kasu` バイナリ）とする。バイナリ更新後の初回アクセスでは
  OS が許可ダイアログを出す（git と同じ挙動）。これを文書に明記する。
- ロック中・ユーザーが拒否した場合（`errSecUserCanceled` / `errSecAuthFailed`）はヘルパーエラーとして扱う。
- SSH 接続中などで GUI がない場合に備え、ロックされていれば `security unlock-keychain` を案内する。

## secretservice

freedesktop.org Secret Service API（D-Bus `org.freedesktop.secrets`）を使う。GNOME Keyring / KeePassXC / KWallet（対応版）で動作する。

- 依存: `secret-service` クレート（MIT / Apache-2.0）を tokio ランタイム・pure Rust 暗号の feature で使う（libdbus の C ライブラリは不要、zbus ベース）。
  `[target.'cfg(target_os = "linux")'.dependencies]` に限定する。導入前に `cargo deny` のライセンス・重複を確認する。
- セッションは暗号化（DH ネゴシエーション）を使い、平文セッションは使わない。
- 保存先は `default` エイリアスのコレクション。ロックされていれば `unlock` を要求する（Secret Service 側がプロンプトを出す）。

| 属性 | 値 |
|---|---|
| `xdg:schema` | `dev.kasu.Credential` |
| `kasu.server` | ServerKey |
| `kasu.account` | アカウント名（検証省略時は空文字列） |
| label | `kasu credentials for https://kasu.example.com:443` |
| secret | PAT（content type `text/plain`） |

- `get`: `xdg:schema` と `kasu.server` で検索。複数見つかった場合は最新の `modified` を使い、警告を出す。
- `store`: `replace = true` で作成し、同一属性の項目を置き換える。属性のうち `kasu.account` が異なる古い項目は事前に削除する。
- `erase` / `list`: 同じ属性検索で行う。`list` は秘密値を読まない。
- `DBUS_SESSION_BUS_ADDRESS` が未設定、またはサービスが起動・アクティベートできない場合は `NotAvailable`。
  ヘッドレス環境（SSH 先のサーバー等）では `pass`（または `file`）を案内する。
- D-Bus 呼び出しには 10 秒のタイムアウトを置く（unlock プロンプト待ちは除き、プロトコルのタイムアウトに従う）。

## pass

[pass](https://www.passwordstore.org/)（GPG で暗号化したファイルを並べるパスワードストア）を `pass` コマンド経由で使う。
docker-credential-pass と同じ方式である。

- 依存クレートなし。`pass` 実行ファイルを `tokio::process::Command` で起動する。`PASSWORD_STORE_DIR` 等の環境変数は継承する。
- エントリ名:

| ServerKey | エントリ |
|---|---|
| `https://kasu.example.com:443` | `kasu/https/kasu.example.com:443` |
| `http://[::1]:50051` | `kasu/http/[::1]:50051` |
| `unix:///run/kasu/node.sock` | `kasu/unix/<base64url(パス)>`（`/` や制御文字を含むため） |

  host:port 部に `/`・`..`・制御文字が含まれないことは ServerKey の正規化で保証する。

- 内容（pass の慣習: 1 行目がパスワード、以降が `key: value`）:

```
<PAT>
account: alice
server: https://kasu.example.com:443
```

- `get`: `pass show <entry>`。エントリが存在しない（`<store>/<entry>.gpg` がない）場合は `pass` を起動せず `None`
  （存在確認はファイルの有無で行い、GPG の復号プロンプトを不要に出さない）。
- `store`: `pass insert --multiline --force <entry>` に標準入力で内容を渡す。
- `erase`: `pass rm --force <entry>`。
- `list`: `<store>/kasu/` 以下の `.gpg` を列挙し、エントリ名から ServerKey を復元する（復号しないので account は `None`）。
- `pass` が見つからない、またはストアが未初期化（`.gpg-id` がない）場合は `NotAvailable` とし、`pass init <gpg-id>` を案内する。
- pinentry が端末を使えるよう、TTY 上では `GPG_TTY` が未設定なら `ttyname(0)` を設定して起動する。

## file

平文で JSON ファイルに保存する。OS のストアが使えない環境・コンテナ・検証用の最終手段。

- パス: `$KASU_CREDENTIALS_FILE`、未設定なら設定ディレクトリの `credentials.json`（[01](01-architecture.md#設定ファイル)）。

```json
{
  "version": 1,
  "credentials": {
    "https://kasu.example.com:443": { "account": "alice", "secret": "kasu_pat_..." }
  }
}
```

- ディレクトリは 0700、ファイルは `O_CREAT | O_EXCL` 相当で 0600 として作る（umask に依存しない）。
- 書き込みは同じディレクトリの一時ファイル（0600）に書いて `fsync` → `rename` → ディレクトリ `fsync`。`flock` で排他する。
- 読み込み時、所有者が自分でない、または group / other に権限がある場合は警告を出す（OpenSSH の秘密鍵と同様の趣旨。拒否はしない）。
- symlink は追わない（`O_NOFOLLOW`）。
- `kasu login --helper file` の成功時に「PAT は平文で <path> に保存されました」と必ず表示する。
