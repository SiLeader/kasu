# リポジトリアカウントと書き込み ACL

[English](repository-accounts.md) | 日本語

`KASU_ACCOUNTS_FILE` に PAT ごとの書き込み範囲を設定します。
Git Smart HTTP／SSH と native Writer の gRPC／HTTP JSON に共通して適用します。
既定ではアカウントにリポジトリ全体の読み取り権限があります。
`read_directories` で [native の読み取り範囲](directory-read-acl_ja.md) を限定できます。
読み取りをディレクトリに限定する Git 接続には、既存の [射影専用アカウント](projection-accounts_ja.md) を使います。
範囲のあるアカウントは Git の許可されたパス射影を読み取れ、通常の Git URL の read／push は拒否します。

## 設定

```json
{
  "accounts": [
    {
      "name": "deploy-bot",
      "token_file": "secrets/deploy-bot.pat",
      "write_policy": {
        "allowed_ref_patterns": ["refs/heads/main", "refs/heads/automation/*"],
        "allowed_path_prefixes": ["deploy", "charts/team-a"],
        "force_ref_patterns": ["refs/heads/automation/*"],
        "allow_create": false,
        "allow_delete": false
      }
    },
    {
      "name": "repository-reader",
      "token_file": "secrets/repository-reader.pat"
    }
  ]
}
```

```sh
KASU_TOKEN_FILE=/etc/kasu/operator.pat \
KASU_ACCOUNTS_FILE=/etc/kasu/accounts.json \
KASU_WRITE_POLICY_FILE=/etc/kasu/write-policy.json \
  kasu-server /srv/kasu demo
```

`token_file` は JSON の場所からの相対パス、または絶対パスです。
`KASU_TOKEN`／`KASU_TOKEN_FILE` は引き続き運用者の資格情報になります。
運用者 PAT を省略してアカウントだけで稼働することもできます。設定されたアカウントがある場合、匿名アクセスは拒否します。
PAT は Basic の password または Bearer として渡します。Basic の username はアカウント名とは照合しません。

`write_policy` を省略するか `null` にすると読み取り専用です。
明示的な `{}` は既定の WritePolicy、つまり branch／tag の作成・fast-forward、全パスの変更を許可します。
force と削除は既定で許可しません。書き込み範囲を限定する場合は、上記のように policy を明記します。
`allowed_path_prefixes` は UTF-8 文字列と byte 配列の両方を受け付け、たとえば `[255]` は `0xff` のパスです。
空文字列は全パス、空の配列は変更可能なパスなしを表します。
パス境界を照合するため、`deploy` の許可で `deployment` は変更できません。

## リポジトリ単位の grant

複数リポジトリを提供する場合は、アカウントに `grants` を設定します。
`repositories` のリポジトリ名 glob ごとに `write_policy` と `read_directories` を指定し、
最も具体的に一致する 1 件だけを使います。grant のないリポジトリは存在しないものと同じ `NotFound` です。
上記の `grants` なしの形式は、起動引数または `migrate-registry` で登録した legacy リポジトリへの単一 grant として扱います。
照合の規則と例は [マルチリポジトリ](multi-repository_ja.md#認証認可) を参照してください。

## サーバー全体の policy との関係

実効権限は、[サーバー全体の WritePolicy](git-push_ja.md)、[リポジトリの policy](multi-repository_ja.md#リポジトリ単位の設定)、アカウントの policy のすべてが許す範囲です。

`Identity.GetPermissions.allowed_path_prefixes` は、この積集合をさらに `read_directories` で制限した範囲を返します。`paths_restricted` が true のとき、空の prefix 配列は変更可能なパスなしを表し、false のときはパス制限なしです。書き込み自体の可否は引き続き `write` で判定します。
アカウント設定はサーバー全体の制限を拡大しません。

| 操作 | 判定 |
|---|---|
| ref の更新先 | 両 policy がその ref を許可 |
| 新規 ref | 両 policy が作成を許可 |
| ref の削除 | 両 policy が削除を許可し、最終 tree 差分も許可範囲内 |
| force／tag の置換 | 両 policy がその ref の force を許可 |
| 変更パス | 最終 tree の各変更が両 policy のパス範囲内 |
| Git atomic push | 全 ref の判定が通過した場合だけ一括 CAS |
| 読み取り専用 PAT | Metadata／Git fetch は許可、Objects は既存の hop 認可にも従う。全 Writer RPC と push は拒否 |

サーバー全体の force 設定が空の場合、アカウント側に force 設定があっても force はできません。
新しい ref のパス検査は空 tree との比較なので、許可範囲外の既存ファイルを含む branch の新規公開も拒否します。
既存 branch の変更では、その branch の更新前 tree と更新後 tree を比較します。
これにより、既存の範囲外ファイルを保持したまま、許可されたディレクトリだけを更新できます。

native の UploadBlobs／CreateChangeset／Merge／Rebase は、書き込み可能なアカウントなら draft を作成できます。
パス ACL は UpdateRef と Land の公開時、Git は receive-pack の公開時に最終 tree 差分を検査します。
個々の Changeset の変更リストだけでは判定しないため、merge の別親を経由する範囲外の変更も検査対象です。
Land は rebase 後の候補を各試行と確定直前に検査します。
読み取り範囲を省略したアカウントはリポジトリ全体を読み取れるので、draft の作成をパス別に秘匿する機能ではありません。

## Land の再送と ref log

Land の永続的な冪等結果は、設定上のアカウント名とクライアントの冪等キーを組み合わせて識別します。
別のアカウントが同じキーを使っても、それぞれの要求として処理します。
同じアカウント・同じキーで要求内容を変えるとエラーになります。
運用者 PAT の既存のキー・fingerprint 形式は維持します。

PAT の更新や設定内の並べ替えでは、アカウント名を維持すると同じ結果を再取得できます。
アカウント名を変えると別の再送名前空間になります。
設定と秘密ファイルはプロセスの起動時に読みます。変更・失効には全 server／SSH worker への設定反映が必要です。
SSH worker は接続ごとに起動するため、次の接続から更新を読みます。
既に開始した要求はその開始時点の policy を保持します。

確定した native／Git 更新の ref log の actor には設定上のアカウント名を使います。
運用者の従来の `api`／`git-push` actor は維持します。
拒否した要求の永続監査ログや、OIDC の認証・動的な失効は未実装です。

## SSH での対応付け

[Git SSH](git-ssh_ja.md) の `authorized_keys` の forced command の最後の引数を、対象アカウントの PAT ファイルにします。
管理者が設定する sshd の `SetEnv` に `KASU_ACCOUNTS_FILE=/etc/kasu/accounts.json` も含めます。
クライアントからアカウント設定や policy の環境変数を受け付ける必要はありません。

```text
restrict,command="/opt/kasu/kasu-git-ssh /srv/kasu demo /etc/kasu/secrets/deploy-bot.pat" ssh-ed25519 AAAA... deploy-bot
```

## 管理権限・プロフィール・delegate

grant に `"admin": true` を付けると、一致するリポジトリを管理（設定、状態、改名、削除）できます。アカウントの `create_repositories`（名前の glob）は、作成・改名後の名前・フォーク先として使える名前を表します。詳しくは [複数リポジトリ](multi-repository_ja.md#admin-api) を参照してください。
`display_name` と `email` は任意のプロフィール項目（256 bytes 以下、`<`・`>`・改行を含まない）で、[Web UI](web-ui_ja.md) で作るコミットの署名の既定値になります。

同じファイルで **delegate** を宣言します。kasu-web のように、自分では何も読めず、要求ごとにアカウントとして振る舞う主体です。

```json
"delegates": [
  { "name": "kasu-web", "token_file": "secrets/kasu-web.pat", "act_as": ["*"], "allow_admin": true,
    "allow_unbound": false, "anonymous_account": "public-reader", "trusted_peers": ["10.0.20.0/24"] }
]
```

| フィールド | 意味 |
|---|---|
| `act_as` | 代理できるアカウント名の glob。運用者と射影アカウントは対象外 |
| `allow_admin` | 代理要求が Admin API でリポジトリを変更できるか |
| `allow_unbound` | `kasu-act-binding` のない要求（リバースプロキシ認証）を受け付けるか |
| `anonymous_account` | 匿名の訪問者が代理するアカウント。`act_as` に含まれなくてよく、常に読み取り専用 |
| `trusted_peers` | delegate の接続を受け付ける送信元（CIDR）。空なら制限なし（警告をログに出す） |

代理要求は `authorization: Bearer <delegate token>`、`kasu-act-as: <account>` と、任意で `kasu-act-binding`（PAT でサインインしたときに `Identity.WhoAmI` が返した binding の 16 進）、`kasu-act-user`（監査用の名前、256 bytes 以下）、`kasu-client-addr`（ブラウザのアドレス）を持ちます。アカウントの欠落・未知・`act_as` 外、信頼しないピア、アカウントの現在の PAT と一致しない binding は `UNAUTHENTICATED` で拒否します。`kasu-act-as` のない delegate token では何も読めません。それ以外は、匿名アカウントが読み取り専用に固定される点を除き、そのアカウントの直接の要求とまったく同じに認可・帰属（ref log の actor、Land のキー、レビューの作者）・admission されます。ref log の `via` 列に delegate と利用者を記録します。binding は `KASU_BINDING_KEY_FILE`（32 bytes 以上、全レプリカで共通）を鍵とした PAT の鍵付きハッシュで、未設定ならプロセスごとにランダムな鍵を使います。delegate token は他のすべての資格情報と異なる必要があります。

## 設定の上限

- accounts は1〜256件。名前は1〜128 bytes で、設定内・射影アカウント間で重複不可。
- PAT は運用者、他アカウント、射影アカウント、Objects hop token と重複不可。
- JSON は最大1 MiB。未知の field、欠落した秘密ファイル、不正な byte パスは起動失敗。
- 各 policy の各ルール配列は最大128件、各ルールは最大4096 bytes。
- PAT の読み込み上限・末尾改行の扱いは既存の token file と同じ。

## 検証

macOS arm64 の実 gRPC／HTTP JSON、Git 2.54.0 の実 HTTP と SSH stdio、
固定 Rust 1.90 Linux arm64 の実 OpenSSH／Git 2.39.5 で検証しました。
アカウント別の path／ref、両 policy の積、read-only、atomic rollback、非 UTF-8、
64要求の並行 HTTP、native Land の最終差分・再送、2つの実 server でのアカウント順序差、
PAT 更新と再起動後の再送・ref log をテストしました。
テストは認可機能の動作検証で、セキュリティ監査は今回の範囲に含めていません。

```sh
cargo test -p kasu-server --lib accounts --locked
cargo test -p kasu-server --test accounts --test git_ssh --locked
docker build -f tools/ssh/Dockerfile -t kasu:ssh-test .
docker run --rm --network none --entrypoint /out/accounts-test kasu:ssh-test
docker run --rm --network none kasu:ssh-test
```

[検証結果とハッシュ](../crates/kasu-git/tests/fixtures/clients/accounts/) と
[SSH の通信記録](../crates/kasu-git/tests/fixtures/clients/ssh/) を保存しています。
[Git SSH CI](../.github/workflows/ssh.yml) にも同じテストを追加しています。GitHub 上での実行結果は未確認です。
