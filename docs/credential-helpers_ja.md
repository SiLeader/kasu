# 認証情報ヘルパー

[English](credential-helpers.md) | 日本語

`kasu login` は PAT（Personal Access Token）を **認証情報ヘルパー** に保存し、`clone`／`checkout`／`commit`／`land`／`review`／`repo` などを
`--token` や `KASU_TOKEN` なしで認証できるようにします。Git や Docker と同様にヘルパーは差し替え可能で、組み込みヘルパーは OS の
秘密情報ストアを使い、実行ファイル `kasu-credential-<name>` を追加することもできます。
設計は [docs/plan/credential-helper](plan/credential-helper/README.md)（日本語のみ）にあります。

```sh
kasu login https://kasu.example.com          # PAT をエコーなしで入力
printf '%s' "$PAT" | kasu login https://kasu.example.com --token-stdin --helper pass
kasu repo list                                # --token は不要
kasu logout https://kasu.example.com
```

## トークンの解決順序

各コマンドのトークンは次の最初のものです。

1. `--token`
2. `KASU_TOKEN`
3. 接続先サーバーで引いた認証情報ヘルパー
4. なし（未認証接続）

1 または 2 がある場合、ヘルパーは **一切呼ばれません**（CI でキーチェーンのプロンプトや D-Bus 待ちが起きません）。
`KASU_CREDENTIAL_HELPER=none` でヘルパーの参照を無効にできます。ヘルパーが失敗した場合や、送信できない PAT（前後に空白がある等）を返した場合は警告を表示して未認証で続行します。
サーバーが `UNAUTHENTICATED` を返した場合はサーバーのエラーを表示したうえで `kasu login <server>` を案内します。サーバーに拒否された保存済みの認証情報は自動では消去しません。

認証情報は正規化したサーバー URL をキーにします。`https://Kasu.Example.com/` は `https://kasu.example.com:443` になり、パスは捨てられ、
`unix:///run/kasu/node.sock` のような unix ソケット URL はそのまま使います。同じサーバーの全リポジトリで同じ PAT を使います。
保存済みの PAT は保存先のエンドポイントにだけ送ります。Objects エンドポイント（`--objects`、`KASU_OBJECTS_ENDPOINT`）が別のホストやソケットの場合は、
そのエンドポイント用に保存した PAT があればそれを使います（`kasu login <Objects エンドポイント>`）。明示的な `--token`／`KASU_TOKEN` は両方に送ります。
working copy には PAT を保存しません。

## コマンド

| コマンド | 動作 |
|---|---|
| `kasu login [SERVER] [--helper H] [--token-stdin] [--no-verify]` | PAT を読み（端末ではエコーなしのプロンプト、それ以外は `--token-stdin`。コマンドライン引数では受け取らない）、`Identity.WhoAmI` で検証して保存し、使ったヘルパーを `config.json` に記録します。`--helper` なしで再ログインすると記録済みのヘルパーを使い、ヘルパーを変えた場合は以前のヘルパーから PAT を消去します。`--no-verify` は検証せずに保存します。`--token`／`KASU_TOKEN` は入力として使いません。プロンプトで Ctrl-C を押すと何も保存せずに中止します |
| `kasu logout [SERVER] [--helper H] [--all]` | 保存済みの PAT を消去します（冪等。何も保存されていなくても終了コード 0）。`--all` は記録済みの全サーバーが対象です。失敗したヘルパーは報告したうえで残りを続けて処理し、消去できたサーバーだけを `config.json` から外し、終了コードは 0 以外になります。サーバー側の PAT は管理者が失効させるまで有効です |

`SERVER` は `KASU_SERVER`、`-C` 以下の working copy のサーバー、`http://127.0.0.1:50051` の順で決めます。
`--json` では login 後に `{"server", "account", "helper"}` を出力します（PAT は出しません）。

`mount` は起動時に PAT を 1 回だけ読み、以後それを使い続けます。ログアウトしても実行中のマウントには反映されません。

## ヘルパーの選択

`--helper`（login／logout のみ）、`KASU_CREDENTIAL_HELPER`、login 時にそのサーバーへ記録したヘルパー、`config.json` の `credential_helper`、
プラットフォーム既定（macOS は `osxkeychain`、Linux は `secretservice`）の順です。
既定が使えない場合（SSH 先で D-Bus セッションがない等）は **平文へ自動で落とさず** エラーにし、`--helper pass` または `--helper file` を案内します。

| ヘルパー | OS | 保存先 | 暗号化 |
|---|---|---|---|
| `osxkeychain` | macOS | ログインキーチェーン（generic password、service は `kasu:<server>`） | あり |
| `secretservice` | Linux | Secret Service の既定コレクション（GNOME Keyring、KeePassXC、KWallet） | あり |
| `pass` | Linux／macOS | `$PASSWORD_STORE_DIR/kasu/<scheme>/<host:port>.gpg` | あり（GPG） |
| `file` | 共通 | `$KASU_CREDENTIALS_FILE`、なければ `<設定ディレクトリ>/credentials.json`（0600） | **なし** |

- **osxkeychain**: `kasu` バイナリを入れ替えた後は、Git と同様に macOS がアクセス許可を再度確認することがあります。
  SSH 越しでは先に `security unlock-keychain` を実行してください。Linux ではコンパイル・テストできないため、macOS の CI ジョブで検証します。
- **secretservice**: libsecret の `secret-tool`（`libsecret-tools` パッケージ）で実装しており、PAT は標準入力で渡します。
  `secret-tool search` は秘密値も出力するため、`list` は読み取って破棄します。
- **pass**: 初期化済みのストア（`pass init <gpg-id>`）が必要です。エントリは 1 行目が PAT、続けて `account:` と `server:` です。
- **file**: 0600（ディレクトリは 0700）で原子的に書き込み、symlink は辿らず、権限が緩いと警告します。`kasu login` は必ず平文保存の警告を表示します。

## 設定

`$KASU_CONFIG_DIR/config.json`（既定は `$XDG_CONFIG_HOME/kasu/config.json`、なければ `~/.config/kasu/config.json`）。秘密値は含みません。

```json
{
  "version": 1,
  "credential_helper": "pass",
  "helper_timeout_secs": 60,
  "servers": { "https://kasu.example.com:443": { "credential_helper": "pass" } },
  "helpers": { "vault": { "path": "/opt/kasu/kasu-credential-vault" } }
}
```

未知の `version` のファイルは読み取り専用として扱い、`login`／`logout` は書き込みを拒否します。

## 外部ヘルパー

組み込みでない名前は `kasu-credential-<name> <operation>` として実行します（`PATH` 上、または `helpers.<name>.path`）。
名前は `[a-z0-9][a-z0-9-]{0,63}` で、組み込み名が常に優先されます。環境変数は継承し、標準入力に JSON の要求を 1 つ受け取り、標準出力に JSON の応答を 1 つ書きます。
非 0 の終了コード、不正な JSON、64 KiB を超える出力、`helper_timeout_secs`（既定 60 秒）以内に応答がない場合はエラーになり、エラーメッセージから PAT は伏せられます。
秘密値は引数や環境変数では渡しません。

| operation | 要求 | 応答（終了コード 0） |
|---|---|---|
| `get` | `{"version":1,"server":"https://kasu.example.com:443"}` | `{"status":"ok","account":"alice","secret":"..."}` または `{"status":"not_found"}` |
| `store` | `{"version":1,"server":"...","account":"alice","secret":"..."}` | `{"status":"ok"}` |
| `erase` | `{"version":1,"server":"..."}` | `{"status":"ok"}` または `{"status":"not_found"}` |
| `list` | `{"version":1}` | `{"status":"ok","entries":[{"server":"...","account":"alice"}]}` または `{"status":"unsupported"}` |

隠しサブコマンド `kasu credential <組み込み名> get|store|erase|list` は同じプロトコルで組み込みヘルパーを呼べるため、外部実装の比較に使えます。

## トラブルシューティング

- `secretservice is not available (no D-Bus session)`: デスクトップセッションがありません（SSH 先で典型的）。`--helper pass` か `--helper file` を使うか、
  `config.json` の `credential_helper` を設定してください。
- `pass is not initialised`: `pass init <gpg-id>` を実行してください。
- `login` 後も未認証になる: `KASU_TOKEN` や `--token` が優先されます。unset してください。
- テストでは実際のストアに触れないよう、`KASU_CONFIG_DIR`、`KASU_CREDENTIALS_FILE`、`PASSWORD_STORE_DIR` を設定し、
  `dbus-run-session` と一時的な `XDG_DATA_HOME` を使ってください。
