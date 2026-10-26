# Native のディレクトリ読み取り ACL

repository account に `read_directories` を設定すると、native Metadata／Objects は
許可パスの読み取りビューを返します。CLI はそのビューから checkout でき、許可範囲の
commit／Land は元の Changeset を親にするため、範囲外のファイルを保持できます。

```json
{
  "accounts": [
    {
      "name": "deploy-user",
      "token_file": "secrets/deploy-user.pat",
      "read_directories": ["deploy", "charts/team-a"],
      "write_policy": {
        "allowed_path_prefixes": ["deploy"],
        "allowed_ref_patterns": ["refs/heads/main"],
        "allow_create": false
      }
    }
  ]
}
```

この JSON を [repository account 設定](repository-accounts.md) の `KASU_ACCOUNTS_FILE` で読み込みます。
読み取りだけなら `write_policy` を省略します。
`read_directories` と `read_directories_hex` を両方省略または `null` にすると、従来どおり repo 全体を読み取れます。
指定した場合は合計1〜128件、各パス4096 bytes までです。空配列は起動エラーです。
`read_directories_hex: ["ff"]` は非 UTF-8 の `0xff` パスを許可します。
重複と既に許可した親配下のルールを除き、正規化したパス集合で権限を識別します。
`app` の許可では `application` は読めません。

## Metadata の見え方

Changeset の ID・親・author／committer・message、ref、履歴の存在、ancestry／merge-base は
リポジトリ共通のメタデータとして公開します。コミットメッセージ等に人が書いた文字列を検閲する機能ではありません。
ファイル内容、変更一覧に含まれる範囲外のパス、tree の範囲外エントリ、raw Git provenance は公開しません。

| RPC | 制限されたアカウントの応答 |
|---|---|
| GetChangeset／Log | 許可パスだけの changes、raw_git_commit は省略、read_filtered=true |
| GetRootTree | 許可ファイルとその祖先 directory だけの canonical TreeId、read_filtered=true |
| ListDir | 読み取りビューから列挙。許可パスまでの祖先は移動可能で、範囲外の兄弟は返さない |
| ReadFile | 許可パスの内容だけ。範囲外は PermissionDenied |
| Diff | paths の省略時は許可パスで絞り込み、範囲外の本文は diff しない |
| Log(path)／Diff(paths) | 明示した各パスが許可範囲内でなければ PermissionDenied |
| Blame | 対象パスと返す全 origin が許可範囲内。copy／move の origin が範囲外なら要求全体を拒否 |
| GetBlobMetadata | 同じ読み取り範囲で公開された Blob ID だけ |
| ResolveRef／ListRefs／IsAncestor／MergeBase／FindChangesetByGitOid | 元の repo の ID・履歴の関係を維持 |

`GetChangeset.read_filtered` は、その応答が元オブジェクトの完全な表現ではないことを示します。
`id` は元 Changeset の参照であり、フィルター済みの応答から元の canonical bytes を復元・検証してはいけません。
元 Changeset と既存の ID／canonical codec は変更しません。
`RootTreeResponse.id` はビュー自体の正しい内容 ID です。許可パスへの境界 node だけを作り直し、
丸ごと許可された subtree は元の ID を共有します。範囲外だけの tree 更新ではビューの ID は変わりません。

HTTP JSON と gRPC は同じ処理を使います。追加した `read_filtered` は protobuf の追加 field で、
無制限のアカウントと従来の応答では false です。

## Objects RPC と取得の順序

`ResolveRef` → `GetRootTree` → 必要な Tree／Blob、という既存の取得順序を使います。
ReadFile と変更一覧が返す許可済み Blob、許可パス内の rewrite conflict の生成物も、その読み取り範囲へ登録します。
Objects の要求・zstd frame は既存の形式のままです。

権限索引は repo と正規化した読み取りパス集合ごとに分離します。
別の利用者が repo 全体へ登録した Tree／Blob や、既知の元 root ID は、そのままでは取得できません。
同一内容が許可パスにも存在する場合、その内容 ID は取得できます。
バッチは全 ID を確認してから配信するため、拒否した ID を含むバッチから一部の frame は流しません。

索引とビュー root cache は不変の KV として保存します。
親 node の登録を子の検証後、root cache の保存を索引作成後に行い、中断後も再計算できます。
再起動や PAT の変更では同じパス集合を再利用できます。
権限を狭めると別の名前空間になり、古い許可の ID は自動的には再利用されません。
同じルールで過去の許可パスから既に公開された内容は、履歴として引き続き取得できます。
クライアントが既にダウンロードした内容を消去する仕組みではありません。

`KASU_OBJECT_TOKEN_FILE` を使う構成でも、範囲のある account は自分の PAT で server の
Objects RPC に直接接続できます。各要求にはその scope を適用します。
無制限の運用者／repository account PAT で hop token を置き換えることは引き続き拒否します。
hop token は repo 全体を読み取る cache 用資格情報です。

共有 cache／daemon の既存プロトコルは利用者の読み取り範囲を委譲しません。
制限された利用者は Objects endpoint を権威 server に設定してください。
cache 側の資格情報を渡すことは、その cache に設定した権限を渡すことになります。
利用者別 scope を保つ共有 cache、永続索引の GC・容量上限、広大なパス集合の容量評価は残作業です。

## Writer と Git

読み取り範囲のある writer の実効パス範囲は、read_directories、アカウントの write policy、
サーバー全体の write policy のすべてを満たす範囲です。
CreateChangeset の明示的な変更は読み取り範囲内に限定し、Blob の参照にも scope の所属を要求します。
これにより、推測した非公開 Blob ID を許可パスへ移して内容を読む要求を拒否します。
UploadBlobs の既存確認も scope 内だけで行い、未許可の内容には実際の bytes と ID の一致を要求します。

Merge／Rebase／Land の競合が範囲外に生じた場合、範囲外の entry や本文を返さず PermissionDenied になります。
公開時の最終 tree ACL は従来どおり検査します。
Land の冪等結果も読み取り範囲ごとに分離するため、権限を変えて古い競合応答を再取得することはありません。
PAT の変更だけなら、アカウント名とパス集合を保つと既存の結果を再取得できます。

Git の通常 URL は repo 全体を含むため、この account では読み取りも push も拒否します。
許可ディレクトリを root にする [Git パス射影](git-projection.md) の読み取りは利用できます。
射影の push は拒否します。Git OID の内容を部分的に隠して同じ ID で返すことはしません。

```sh
KASU_TOKEN=deploy-user-pat kasu checkout main deploy-work \
  --server http://127.0.0.1:50051 --repo demo
# --sparse を指定しなくても、server が許可範囲だけの tree を返す。
```

## 検証

macOS arm64 と固定 Rust 1.90 Linux arm64 で実 Metadata／Writer／Objects RPC、HTTP JSON、CLI を検証しました。
root／部分祖先／非 UTF-8 のフィルタリング、既知の元 Tree／Blob の拒否、バッチ全件の事前認可、
非公開 Blob のコピー・アップロード確認の拒否、範囲外 conflict、権限変更時の索引分離を含みます。
実 server 2プロセスと PAT 更新・再起動で、ビューと権限索引・Land の結果を再取得しました。
実 CLI は sparse 指定なしで checkout→commit→Land を行い、非公開 Blob の無取得とサーバー上の内容保持を照合しました。

```sh
RUSTC_WRAPPER= cargo test -p kasu-server --test accounts --locked
RUSTC_WRAPPER= cargo test -p kasu-cli --test e2e --locked
docker build -f tools/ssh/Dockerfile -t kasu:read-acl-test .
docker run --rm --network none --entrypoint /out/accounts-test kasu:read-acl-test
docker run --rm --network none --entrypoint /out/cli-e2e-test kasu:read-acl-test --skip virtual_mount
```

[検証ログとソースハッシュ](../crates/kasu-git/tests/fixtures/clients/read-acl/) を保存しています。
[Git SSH CI](../.github/workflows/ssh.yml) にも native CLI のテストを追加しています。GitHub 上での実行結果は未確認です。
セキュリティ監査と、本番での権限設計・鍵管理の評価は今回の範囲に含めていません。
