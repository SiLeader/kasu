# Git over SSH

`kasu-git-ssh` は OpenSSH の forced command として起動する Git transport です。
公開鍵認証と暗号化は OpenSSH に委ね、Git の advertisement／negotiation／pack と
native ストレージへの読み書きを Kasu が処理します。Git の外部プロセスや bare repository は使いません。

## 起動と資格情報

```sh
RUSTC_WRAPPER= cargo build -p kasu-server --bin kasu-git-ssh --release --locked
```

専用 OS ユーザー `git` の `authorized_keys` に、公開鍵ごとの固定コマンドを登録します。
以下のパスは設置先に置き換えてください。引数をクライアント入力から組み立てないでください。

```text
restrict,command="/opt/kasu/kasu-git-ssh /srv/kasu demo /etc/kasu/operator.pat" ssh-ed25519 AAAA... operator
restrict,command="/opt/kasu/kasu-git-ssh /srv/kasu demo /etc/kasu/deploy-reader.pat" ssh-ed25519 AAAA... deploy-reader
```

`sshd_config` の該当ユーザーに、管理者が用意した設定だけを渡します。

```text
Match User git
    AuthenticationMethods publickey
    PasswordAuthentication no
    KbdInteractiveAuthentication no
    PermitTTY no
    DisableForwarding yes
    AcceptEnv GIT_PROTOCOL
    SetEnv KASU_TOKEN_FILE=/etc/kasu/operator.pat KASU_PROJECTION_ACCOUNTS_FILE=/etc/kasu/accounts.json KASU_WRITE_POLICY_FILE=/etc/kasu/write-policy.json
```

`KASU_TOKEN_FILE` は既存の `KASU_TOKEN` のファイル版で、通常の `kasu-server` でも使えます。
両方の指定は起動エラーです。PAT は秘密ファイルに保存し、[射影アカウント](projection-accounts.md) の
`token_file` と forced command の最後の引数を対応させます。最後の引数は接続者の資格情報、
環境変数側のファイルは backend が照合する運用者資格情報です。
クライアントから `KASU_*` を受け取る `AcceptEnv` は設定しないでください。

```sh
git clone ssh://git@example.test/demo.git
git clone git@example.test:demo.git
git clone --depth=1 ssh://git@example.test/demo/-/path/deploy.git
git clone --filter=blob:none ssh://git@example.test/demo.git
git push origin main
```

SSH ホスト鍵はクライアントの `known_hosts` で照合します。通常の repo は運用者 PAT または [repository account](repository-accounts.md)、
射影は許可ディレクトリに対応する PAT で認可します。匿名 SSH は拒否します。
`git-upload-pack`／`git-receive-pack` と1個の repository 引数だけを解釈し、
`SSH_ORIGINAL_COMMAND` を shell で実行しません。パスの apostrophe と非 UTF-8 bytes を保持し、
射影の `path-hex` 表記も HTTP と同じ解釈になります。

## 動作と制約

protocol v0／v1／v2 の stateful negotiation、shallow／deepen／unshallow、
partial clone の blob 取得、通常の push、atomic push、ref 削除に対応します。
pack は zlib の境界と object 数から終端を検出するため、受信側は SSH の EOF を待ちません。
空 pack と、ref 削除だけで pack がない場合も扱います。
受信 pack の検証・接続性・ref CAS・write policy・変更ログ・webhook outbox は HTTP と共通です。
射影 URL の push は拒否します。

共有の backend 設定を読み、SQLite／filesystem または既存の production storage に接続します。
SSH worker 自体は webhook dispatcher を起動しません。永続 outbox の配送には
同じストレージを使う通常の `kasu-server` を稼働させます。

制御 pkt-line の総量は1セッション16 MiB、コマンドは4096 bytes が上限です。
pack には既存 receive limits（既定128 MiB、10万 object、object 64 MiB、展開総量512 MiB、
delta depth 64）を適用します。stdout の書き込みで backpressure を伝えます。
`KASU_SSH_TIMEOUT_SECS` は1〜3600秒、既定300秒のセッション全体期限です。
期限切れは終了コード124、その他の失敗は1になり、診断は stderr に出します。

worker は接続ごとの別プロセスです。HTTP／gRPC サーバーの共有 admission budget とは合算されません。
SSH の接続数・プロセス数は OpenSSH と OS の制限で管理します。
鍵の運用・失効、実環境の sshd 設定、複数ノードの共有 quota は未評価です。

## 検証

macOS Git 2.54.0 の stdio 接続と、Linux Git 2.39.5／OpenSSH 9.2p1 の
実公開鍵接続で、protocol 0／1／2 の full／depth1 clone、shallow の増分 fetch、
deepen／unshallow、push、空 pack／ref 削除、atomic branch+tag push、partial clone、
射影 clone とアカウント認可を検証しました。各 clone の strict fsck と native file 内容も照合しています。
Linux の go-git v5.19.3 も実 SSH・固定 known_hosts で full／depth1 clone→commit→push が通過しました。

境界単位テストは1 byteずつの pkt-line／pack 入力、切れた入力、後続メッセージの非消費を検証します。
実プロセスで stdin を開いたまま待機させ、設定期限で終了することも確認しています。
[通信 fixture と結果](../crates/kasu-git/tests/fixtures/clients/ssh/) を保存しています。

```sh
RUSTC_WRAPPER= cargo test -p kasu-server --lib git_ssh --locked
RUSTC_WRAPPER= cargo test -p kasu-server --test git_ssh --locked
docker build -f tools/ssh/Dockerfile -t kasu:ssh-test .
docker run --rm --network none kasu:ssh-test
```

Docker の root／空 password はテスト用コンテナ内だけの設定で、sshd は公開鍵のみ、接続先は loopback です。
CI の [Git SSH workflow](../.github/workflows/ssh.yml) は同じコンテナを実行します。
GitHub 上での実行結果は未確認です。暗号・鍵管理のセキュリティ監査は今回の検証範囲に含めていません。

仕様の参照先は Git 公式の [pack protocol](https://git-scm.com/docs/gitprotocol-pack) と
[protocol v2](https://git-scm.com/docs/gitprotocol-v2) です。
EOF と独立した受信、shallow-info の送信順序、複数回の negotiation は SSH の stateful 接続に合わせています。
