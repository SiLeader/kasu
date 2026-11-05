# 01. アーキテクチャ

## 全体像

```
kasu (kasu-cli)
  ├─ トークン解決: --token > KASU_TOKEN > credential helper > なし
  └─ kasu-credential
       ├─ Store trait（get / store / erase / list）
       ├─ 組み込み: osxkeychain | secretservice | pass | file
       └─ 外部:     PATH 上の kasu-credential-<name>（JSON over stdio）
```

- CLI は「どのサーバーに接続するか」が決まった時点で、そのサーバーの認証情報をヘルパーから取得する。
- 組み込みヘルパーはプロセス内で実行する（外部コマンド起動のオーバーヘッドと PATH 汚染を避ける）。
  ただし `pass` は `pass` コマンドを呼び出す（[02](02-helpers.md)）。常駐プロセスは一切持たない。
- 外部ヘルパーは Docker の `docker-credential-<name>` と同じ発想で、`kasu-credential-<name>` を PATH から探して起動する。

## クレート構成

新クレート `crates/kasu-credential` を追加する。CLI 以外（将来の `kasu-web` の開発用ツール等）からも使えるよう、
`kasu-cli` には置かない。

```
crates/kasu-credential/
  src/
    lib.rs          // 公開 API: Credential, ServerKey, Store trait, Error, resolve_helper()
    key.rs          // サーバー URL の正規化（ServerKey）
    config.rs       // config.json の読み書き
    protocol.rs     // 外部ヘルパーの JSON プロトコル（要求・応答の型）
    external.rs     // kasu-credential-<name> の起動
    file.rs         // file ヘルパー
    pass.rs         // pass ヘルパー（Linux / macOS どちらでもビルドするが既定は Linux のみ）
    #[cfg(target_os = "macos")]  osxkeychain.rs
    #[cfg(target_os = "linux")]  secretservice.rs
  tests/
    file.rs  external.rs  pass.rs  osxkeychain.rs  secretservice.rs
```

### 主要な型

```rust
/// 正規化済みのサーバー識別子。認証情報の検索キー。
pub struct ServerKey(String);

pub struct Credential {
    pub server: ServerKey,
    /// WhoAmI で得たアカウント名。検証を省略した場合は None。
    pub account: Option<String>,
    pub secret: Zeroizing<String>,
}

// async fn in trait を使う（async-trait クレートは使わない）。
// ヘルパーは名前で選ぶため、動的ディスパッチ用に enum Helper { OsxKeychain, SecretService, Pass, File, External(..) } で包む。
pub trait Store {
    fn name(&self) -> &str;
    async fn get(&self, server: &ServerKey) -> Result<Option<Credential>>;
    async fn store(&self, credential: &Credential) -> Result<()>;
    /// 存在しなければ Ok(false)。
    async fn erase(&self, server: &ServerKey) -> Result<bool>;
    /// 秘密値を含まない一覧。対応しないヘルパーは Error::Unsupported。
    async fn list(&self) -> Result<Vec<(ServerKey, Option<String>)>>;
}
```

- 秘密値は `zeroize::Zeroizing` で保持し、`Debug` 実装では伏せ字にする。`tracing` のフィールドに含めない。
- 同期 API しかない OS ライブラリ（Security.framework）は `tokio::task::spawn_blocking` で呼ぶ。
- エラーは `thiserror` で `NotAvailable`（ヘルパー自体が使えない: D-Bus なし、pass 未初期化等）、`Unsupported`、
  `Helper { name, message }`、`Io`、`Config` に分類し、CLI は `NotAvailable` に対して代替ヘルパーを案内する。

## 認証情報のキー

PAT はサーバー（認証主体の発行元）単位で保存する。リポジトリ単位にはしない（同一サーバーの全リポジトリで同じ PAT を使うため）。

| 入力 | 正規化後のキー |
|---|---|
| `https://Kasu.Example.com/` | `https://kasu.example.com:443` |
| `http://127.0.0.1:50051` | `http://127.0.0.1:50051` |
| `https://[::1]:8443/path` | `https://[::1]:8443`（パスは捨てる） |
| `unix:///run/kasu/node.sock` | `unix:///run/kasu/node.sock`（パスはそのまま、正規化しない） |

- scheme と host を小文字化し、既定ポートを明示する。userinfo を含む URL は既存の `Remote::connect` と同じく拒否する。
- Objects エンドポイント（`--objects` / `KASU_OBJECTS_ENDPOINT`）が別ホストでも、送るトークンは **メタデータサーバーのキー** で引いたものとする
  （現在も同じトークンを両方へ送っているため挙動は変わらない）。
- daemon / cache 経由（`unix://` や cache の URL）で接続する場合、そのエンドポイント自体のキーで保存する。
  利用者は `kasu login unix:///run/kasu/node.sock` のように接続先 URL で一度ログインする。

## トークン解決順序

1. `--token`（コマンドライン）
2. `KASU_TOKEN`（環境変数。clap の `env` により 1 と同じフィールドに入る）
3. 接続先サーバーのキーで credential helper から取得
4. なし（現在の未認証接続と同じ）

- 1・2 がある場合はヘルパーを **呼び出さない**（CI でキーチェーンのプロンプトや D-Bus 待ちが起きないようにする）。
- `KASU_CREDENTIAL_HELPER=none` でヘルパー参照自体を無効化できる。
- ヘルパーの `get` は「見つからない」と「エラー」を区別する。見つからない場合は未認証で続行し、
  エラー（キーチェーンのロック解除拒否、pass の復号失敗等）は警告を表示して未認証で続行する。
  サーバーが `UNAUTHENTICATED` を返した場合は「`kasu login <server>` を実行してください」と案内する（[03](03-cli.md#エラー表示)）。
- 取得は 1 コマンドにつき 1 回までとし、取得結果をプロセス内でキャッシュする。

## ヘルパーの選択

`get` / `logout` で使うヘルパーは次の順で決める。

1. `KASU_CREDENTIAL_HELPER`（`none` / ヘルパー名）
2. `config.json` の `servers.<key>.credential_helper`（`kasu login` が記録したもの）
3. `config.json` の `credential_helper`（利用者が設定した既定）
4. プラットフォーム既定: macOS は `osxkeychain`、Linux は `secretservice`

`kasu login` は `--helper` があればそれを使い、なければ 1・3・4 の順で決める。

- Linux で `secretservice` が使えない（`DBUS_SESSION_BUS_ADDRESS` 未設定、Secret Service が応答しない）場合、
  **平文の `file` へ自動で落とさない**。`--helper pass` / `--helper file` を案内してエラーにする
  （Docker が黙って平文保存する問題を避ける）。
- 一度 `login` に成功したヘルパーは `servers.<key>` に記録し、以後の `get` はそのヘルパーだけを見る
  （複数ヘルパーを順に探して余計なプロンプトを出さない）。

## 設定ファイル

パス: `$KASU_CONFIG_DIR/config.json`。`KASU_CONFIG_DIR` 未設定時は `$XDG_CONFIG_HOME/kasu`、それも未設定なら `~/.config/kasu`
（macOS も同じ。gh / docker と同様に CLI 設定は `~/.config` に置く）。新たな依存を増やさないため JSON（`serde_json`）とする。

```json
{
  "version": 1,
  "credential_helper": "osxkeychain",
  "servers": {
    "https://kasu.example.com:443": { "credential_helper": "osxkeychain" },
    "http://127.0.0.1:50051": { "credential_helper": "file" }
  }
}
```

- **秘密値は書かない**（`file` ヘルパーの保存先は別ファイル）。そのため権限は 0644 でよいが、ディレクトリは 0700 で作る。
- 書き込みは一時ファイル＋`rename` で原子的に行い、`flock`（`rustix::fs::flock`）で並行する `kasu login` と排他する。
- 未知のフィールドは保持しない（`version` を見て将来の移行を行う）。`version` が未知なら読み取り専用で扱い、書き込みを拒否する。

## ヘルパープロトコル

外部ヘルパー `kasu-credential-<name>` の契約。組み込みヘルパーの単体テストにも同じ要求・応答型を使う。

### 起動

```
kasu-credential-<name> <operation>
```

- `<name>` は `[a-z0-9][a-z0-9-]{0,63}`。組み込み名（`osxkeychain` 等）と同名の外部ヘルパーは無視し、組み込みを優先する。
- 実行ファイルは `PATH` から探す（絶対パス指定は `config.json` の `helpers.<name>.path` で可能にする）。
- 環境変数は継承する（pinentry・D-Bus 等のため）。stderr はそのまま利用者の端末へ流す。
- 標準入力に JSON を 1 つ書いて閉じ、標準出力から JSON を 1 つ読む。タイムアウトは既定 60 秒
  （キーチェーン等の対話プロンプトに配慮。`config.json` の `helper_timeout_secs` で変更可）。

### 要求と応答

| operation | 標準入力 | 標準出力（終了コード 0） |
|---|---|---|
| `get` | `{"version":1,"server":"https://kasu.example.com:443"}` | `{"status":"ok","account":"alice","secret":"..."}` または `{"status":"not_found"}` |
| `store` | `{"version":1,"server":"...","account":"alice","secret":"..."}` | `{"status":"ok"}` |
| `erase` | `{"version":1,"server":"..."}` | `{"status":"ok"}` または `{"status":"not_found"}` |
| `list` | `{"version":1}` | `{"status":"ok","entries":[{"server":"...","account":"alice"}]}` または `{"status":"unsupported"}` |

- 終了コードが 0 以外ならエラーとし、stderr の内容をエラーメッセージに含める。標準出力が JSON として解釈できない場合もエラー。
- 応答の大きさは 64 KiB を上限とする。`secret` は空文字列を拒否する。
- `version` が未知の要求を受けたヘルパーは非 0 で終了する。
- 秘密値は引数・環境変数では渡さない（`ps` から見えるため）。

### 組み込みヘルパーの公開

デバッグとヘルパー実装者向けに、組み込みヘルパーを同じプロトコルで呼べる隠しサブコマンドを用意する。

```
kasu credential <name> get|store|erase|list   # stdin/stdout は上記プロトコル
```

外部ヘルパーの互換テストはこのサブコマンドと外部実装の出力を比較して行う。
