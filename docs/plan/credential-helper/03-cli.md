# 03. CLI

## 追加するサブコマンド

```
kasu login  [SERVER] [--helper <name>] [--token-stdin] [--no-verify]
kasu logout [SERVER] [--helper <name>] [--all]
```

隠しサブコマンド（`#[command(hide = true)]`）:

```
kasu credential <name> get|store|erase|list    # 組み込みヘルパーをプロトコルで呼ぶ（01 参照）
```

### SERVER の決め方（login / logout 共通）

1. 位置引数 `SERVER`
2. `KASU_SERVER`
3. `-C` 以下に working copy があれば、その `state.server`
4. 既定 `http://127.0.0.1:50051`（既存サブコマンドの `--server` 既定と同じ）

決めたサーバーは [ServerKey](01-architecture.md#認証情報のキー) に正規化して表示する。

## kasu login

```
$ kasu login https://kasu.example.com
PAT for https://kasu.example.com:443: ********
Logged in to https://kasu.example.com:443 as alice (stored in osxkeychain).
```

手順:

1. SERVER とヘルパーを決める（[ヘルパーの選択](01-architecture.md#ヘルパーの選択)）。ヘルパーが `NotAvailable` ならここでエラー。
2. PAT を読む。
   - `--token-stdin`: 標準入力を EOF まで読み、末尾の改行（`\n` / `\r\n`）を 1 つ除く。
   - それ以外で標準入力が TTY: `/dev/tty` にプロンプトを出し、`rustix::termios` でエコーを止めて 1 行読む
     （シグナル・エラー時もエコーを必ず復元する。`kasu-cli` の `rustix` に `termios` feature を追加）。
   - 標準入力が TTY でなく `--token-stdin` もない場合はエラー（パイプからの誤入力を防ぐ）。
   - 空文字列・制御文字・前後の空白を含む PAT は拒否する（`kasu_object::bearer` が受け付けない値を先に弾く）。
   - グローバルの `--token` / `KASU_TOKEN` は login の入力としては **使わない**（コマンドライン引数で PAT を渡す経路を増やさない）。
3. 検証（`--no-verify` がない場合）: その PAT で `Identity.WhoAmI` を呼ぶ。
   - 成功: `Principal.account` をアカウント名として保存する。`anonymous = true` の応答はエラー（PAT が効いていない）。
   - `UNAUTHENTICATED`: 「PAT が無効です」として保存しない。
   - 接続失敗・`UNIMPLEMENTED`（古いサーバー）: 保存せずエラーにし、`--no-verify` を案内する。
4. ヘルパーの `store` を呼ぶ。
5. `config.json` の `servers.<key>.credential_helper` を記録する。
6. 結果を表示する。`file` の場合は平文保存の警告を表示する。
   `KASU_TOKEN` が設定されている場合は「環境変数が優先されるため保存した PAT は使われません」と警告する。

`--json` では `{"server": "...", "account": "alice", "helper": "osxkeychain"}` を出力する（PAT は出さない）。

## kasu logout

```
$ kasu logout https://kasu.example.com
Logged out of https://kasu.example.com:443 (removed from osxkeychain).
```

1. SERVER を決める。ヘルパーは `--helper` → `servers.<key>.credential_helper` → 既定の順。
2. ヘルパーの `erase` を呼ぶ。見つからなければ「保存された認証情報はありません」と表示して **成功（終了コード 0）** とする（冪等）。
3. `config.json` から `servers.<key>` を削除する。
4. サーバー側の PAT は失効しない旨を、初回のみではなく毎回 1 行で表示する（PAT の失効は管理者の作業であるため）。

`--all`: `config.json` の `servers` に記録された全サーバーについて 2・3 を行う。加えて `--helper` 指定時はそのヘルパーの `list` の結果も対象にする
（`list` が `Unsupported` なら記録済みのものだけ）。

## 既存コマンドへの組み込み

現在 `main.rs` は `cli.token.as_deref()` を各所で直接渡している。これを接続先が決まった時点での解決に置き換える。

```rust
/// --token / KASU_TOKEN があればそれを、なければヘルパーから取得する。
async fn token_for(cli: &Cli, server: &str) -> Result<Option<Zeroizing<String>>>;
```

| サブコマンド | 接続先（キー） |
|---|---|
| `mount` / `checkout` / `repo` / `review` | `--server` / `KASU_SERVER` |
| `clone` | 位置引数 `server` |
| `diff` / `commit` / `rebase` / `land` / `log` / `blame` / `show` / `sparse` | working copy の `state.server` |
| `status` / `diff --json` | 接続しないので取得しない |

- `WorkingCopy::remote(token)` / `hydrate_diff(token)` / `Remote::connect` / `Admin::connect` / `Reviews::connect` / `virtual_fs::mount`
  のシグネチャは `Option<&str>` のまま変えず、呼び出し側で解決した値を渡す（影響範囲を `main.rs` に閉じる）。
- `mount` はマウント中に長時間動くため、起動時に 1 回取得した PAT を使い続ける（ログアウトしても既存マウントには反映しない。文書化する）。
- working copy にはこれまでどおり PAT を保存しない。

## エラー表示

| 状況 | 表示 |
|---|---|
| サーバーが `UNAUTHENTICATED`、トークンなし | `kasu: authentication required; run 'kasu login <server>' or set KASU_TOKEN` |
| サーバーが `UNAUTHENTICATED`、ヘルパー由来のトークン | `kasu: stored credentials for <server> were rejected (account alice); run 'kasu login <server>' again` |
| ヘルパーの `get` 失敗 | `kasu: warning: credential helper 'pass' failed: <message>; continuing without credentials` |
| 既定ヘルパーが使えない（login） | `kasu: secretservice is not available (no D-Bus session); use --helper pass or --helper file` |

ヘルパー由来のトークンが拒否されても、**自動では消去しない**（git は消去するが、一時的なサーバー設定ミスで認証情報を失わないため）。

## ヘルプ・補完

- `kasu login --help` に保存先と解決順序を記載する。
- `--helper` の値は組み込み名の補完候補を持つが、任意の名前（外部ヘルパー）を受け付ける。
