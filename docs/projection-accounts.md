# パス射影の読み取りアカウント

`KASU_PROJECTION_ACCOUNTS_FILE` で PAT ごとに読み取り可能なディレクトリを設定します。
このアカウントは Git の [パス射影](git-projection.md) 専用です。

```json
{
  "accounts": [
    {
      "name": "deploy-reader",
      "token_file": "secrets/deploy-reader.pat",
      "directories": ["deploy", "charts/team-a"],
      "directories_hex": []
    }
  ]
}
```

`token_file` は設定ファイルからの相対パス、または絶対パスです。末尾の CR／LF を除いた PAT を読みます。
秘密値は JSON 本体に書かず、所有者だけが読めるファイルに保存してください。
`directories_hex` は非 UTF-8 パスのためのバイト列指定です。たとえば `ff` は `0xff` のディレクトリを許可します。

```sh
KASU_PROJECTION_ACCOUNTS_FILE=/path/to/accounts.json \
  cargo run -p kasu-server -- /path/to/kasu-data my-repo

git clone --filter=blob:none \
  http://any-user:reader-pat@127.0.0.1:50080/my-repo/-/path/deploy.git
```

Basic の username は任意です。Bearer も使えます。`name` は設定上の識別名で、Basic username とは照合しません。
従来の `KASU_TOKEN` を併用すると、その PAT は通常の repo／Metadata／Writer API を使う運用者の資格情報になります。
併用しない場合、射影アカウントだけの読み取りサーバーとして起動できます。
この設定があると、`KASU_TOKEN` がなくても匿名アクセスは拒否します。

複数リポジトリでは、`directories` の代わりに
`"projections": [{"repository": "team-a/*", "directories": ["deploy"], "directories_hex": []}]` で
リポジトリ名 glob とディレクトリの組を指定します。`directories` だけの形式は legacy リポジトリの射影です。
一致しないリポジトリは存在しないものと同じ `NotFound` です。

## 許可範囲

| 操作 | 射影専用 PAT |
|---|---|
| 許可ディレクトリそのものの Git 射影 | 読み取り可能 |
| 許可ディレクトリ内の、さらに深いディレクトリの射影 | 読み取り可能 |
| v0／v1／v2、shallow／partial clone、後続 promisor fetch | 同じ範囲で利用可能 |
| 許可パスの親、兄弟、通常の repo URL | 拒否 |
| receive-pack 広告／push | 拒否 |
| Metadata／Writer の gRPC・HTTP JSON | 拒否 |
| Objects gRPC | 拒否。共有 cache 用の hop 資格情報を流用できない |

パス境界で照合するため、`deploy` の許可で `deploy-other` は読めません。
literal と hex の URL は同じバイト列パスへ正規化してから照合します。
複数ディレクトリを許可しても、通常 repo 全体を取得する権限にはなりません。
want の到達可能性と OID 索引も引き続き射影ごとに検査します。

認証は Git の URL 解釈・ref 導出・pack 生成より前、パス認可は ref 導出より前に実行します。
未認証・誤った PAT は 401、認証済みだが範囲外の操作は 403 を返します。
ネイティブ API に渡した射影専用 PAT は、ID の所属や内容を調べる前に拒否します。
専用の Objects hop token が有効な場合、Objects はその hop token を要求します。

## 設定と制約

- account 数は 1〜256、名前は重複不可、1〜128 UTF-8 bytes。
- 各 account は 1〜128 個のディレクトリを持ち、各パスは最大 4096 bytes。
- PAT は最大 4096 bytes。アカウント間、運用者 PAT、Objects hop token との重複を拒否します。
- JSON は最大 1 MiB。未知の field、不正なパス・hex、空の account 一覧、欠落した秘密ファイルは起動失敗にします。
- 設定・秘密ファイルの変更や失効にはサーバー再起動が必要です。複数 server には同じ設定を配布してください。

元の author／committer／message と ref 名は射影でも残ります。メッセージ内の文字列をパスに応じて除去する機能はありません。
[アカウントごとの書き込み ACL](repository-accounts.md) は別の repository account 設定で利用できます。
[ネイティブのディレクトリ読み取り](directory-read-acl.md) は repository account の read_directories で利用できます。
OIDC、動的な失効・拒否監査ログは残作業です。
既存の書き込み用 `allowed_paths` は運用者 PAT にも適用され、最終 tree 差分を検査します。

## 検証

macOS arm64 と固定 Rust 1.90／Linux arm64 で、次のテストが通過しています。

- 実 Git の v0／v1／v2 で、任意の Basic username と専用 PAT による partial clone／後続 blob 取得。
- 許可された descendant の受け入れと、通常 repo／兄弟ディレクトリ／別の hex パス／push の拒否。
- HTTP JSON と実 gRPC の Metadata／Writer／Objects に同じ PAT を渡した場合の拒否。
- 実 `kasu-server` バイナリの相対 token file 読み込み、運用者 PAT なしの匿名拒否、設定不備の起動失敗。
- PAT 重複、Objects hop token の設定順序を変えた衝突、unknown field、invalid path／hex、秘密値を含まないエラー。

[検証記録](../crates/kasu-git/tests/fixtures/clients/projection-auth-summary.json) にソースのハッシュと実行範囲を保存しています。

```sh
cargo test -p kasu-server --lib --test projection_accounts --locked
cargo test -p kasu-server --test git_http scoped_projection --locked
docker build -f tools/git-compat/Dockerfile -t kasu:projection-auth-test .
```
