# ネイティブ CLI と working copy

`kasu-cli` は `kasu` コマンドを提供します。macOS／Linux の Unix filesystem を対象とします。

```sh
RUSTC_WRAPPER= cargo build -p kasu-cli --locked
KASU_BIN="$(pwd)/target/debug/kasu"
export KASU_SERVER=http://127.0.0.1:50051
export KASU_REPO=my-repo
export KASU_TOKEN=dev-token
export KASU_AUTHOR_NAME='Your Name'
export KASU_AUTHOR_EMAIL='you@example.com'
"$KASU_BIN" checkout main work --sparse app --sparse lib
cd work
"$KASU_BIN" status
"$KASU_BIN" diff
"$KASU_BIN" commit -m 'Update configuration'
"$KASU_BIN" rebase --onto main
"$KASU_BIN" land
```

バイナリを PATH に追加すると、以降は `kasu` として実行できます。
接続先は gRPC URL です。HTTPS は WebPKI trust roots で検証します。
トークンは環境変数または `--token` で渡し、working copy には保存しません。
名前・メールは `--author-name`／`--author-email` でも指定できます。
`-C <dir>` は実行する working copy を指定し、その配下からも実行できます。
パス引数はリポジトリの root からの相対パスです。

## コマンド

| コマンド | 動作 |
|---|---|
| `checkout <ref または Changeset ID> <新しい directory>` | 基底を解決し、必要な Tree／Blob だけ取得して作成 |
| `status`／`diff` | 起動時に実ファイルを比較。通常ファイル、実行ビット、symlink、削除・追加を検出 |
| `status --watch` | notify の FSEvents／inotify で変更を監視し、全状態比較を再実行 |
| `commit -m <message>` | Blob の存在確認／ストリーム upload と、サーバーでの Changeset 作成 |
| `rebase --onto <ref または ID>` | draft があればサーバーで rebase。なければ基底を更新 |
| `land [--ref <branch>]` | 永続キーを保存して Land。成功後に返された Changeset へ更新 |
| `sparse add <path>`／`sparse remove <path>` | include／exclude を更新して実ファイルを更新 |
| `log [--path <path>] [--limit N]` | サーバー上の履歴を問い合わせ |
| `blame [--follow-moves] [--follow-copies] <path>` | rename 前のパスまで追跡した Blame を取得。移動、別ファイルからの全体／行断片 copy を明示的に追跡（[規則](blame-lineage.md)） |
| `show [ref または ID] [path]` | Changeset の JSON、または指定ファイルの本文を取得 |

`--json` は status／diff／log／Land／ファイル show の構造化出力を指定します。
パスのバイト列は JSON の数値配列で保持します。通常の status 出力は制御文字と
非 ASCII バイトを escape して表示します。

未着地の commit は1つの draft として更新します。2回目以降の commit は、
元の親に対する累積差分をサーバーで問い合わせ、今回の変更と合わせた新 Changeset を作成します。
先の commit の変更を保持しつつ、追加の取り消しにも対応します。ローカル履歴は計算・保存しません。
作成した過去の Changeset はサーバーの不変ストアに残ります。

Changeset ID を指定した `rebase --onto` は、既存の Land 先を維持します。
ID から新規に `checkout`／`mount` した場合は Land 先を持たないため、
`land --ref <branch>` で明示してください。ref を指定した rebase は、その ref を Land 先にします。

## スパースと保存形式

`--sparse` を省略すると全体を選択します。繰り返し指定すると、それらのパスだけを選択します。
`sparse remove app/tests` は子 subtree を除外し、後から追加されたファイルにも適用します。
その後 `sparse add app/tests/small` とすると、より具体的な include が除外を上書きします。
`.` は root 全体です。

`.kasu/state.json` に現在の基底、draft 状態、ref、プロファイル、パスごとの entry と
mtime／size／inode を保存します。検出漏れを避けるため、現実装の status は実内容も hash します。
キャッシュ `.kasu/objects` は [Object RPC](object-api.md) の zstd frame を変換せず保存し、
受信時にチャンク順序・終端・展開上限・BLAKE3 を検証します。
プロファイル外の Tree／Blob はクライアントから要求しません。

グローバル `--objects <URL>`／`KASU_OBJECTS_ENDPOINT` は Object RPC の接続先を指定します。
checkout では選択した URL を保存し、以降は保存済み URL を今回の実行だけ上書きできます。
`unix:///absolute/path/node.sock` で [daemon／cache 階層](cache-hierarchy.md) に接続できます。
指定を省略した新規 checkout は server への直接接続を使います。

symlink はリンク先をたどらず、リンク先バイト列を保存します。
既存 Gitlink は `<path>/.kasu-gitlink` に40桁 SHA を持つ directory として表し、
SHA の編集を commit できます。submodule 本文の取得は行いません。
macOS の filesystem が表現できない非 UTF-8 名はエラーになります。
RPC と Linux working copy ではバイト列を保持します。

## ファイル保護・競合・再送

checkout は既存の destination を上書きせず、一時 directory の完成後に公開します。
更新は排他ロックを取得し、未 commit の変更や未追跡ファイルとの衝突を拒否します。
親 directory は descriptor から `NOFOLLOW` でたどり、symlink を通じた書き込みを防ぎます。
`.kasu` と大文字・小文字違いの root パスは予約します。

複数ファイルの更新は、staging／backup と fsync 済みジャーナルを使います。
途中終了時は次の起動で旧状態へ戻し、状態ファイルの保存後なら新状態を確定します。
中断後にユーザーが編集したファイルは上書きせず、元の backup とジャーナルを残してエラーにします。
その場合は `.kasu/transaction/backup/` と現在のファイルを保全してから競合を解消してください。

Rebase／Land の競合は `.kasu/conflicts.json` に保存し、現在のファイルと基底を保持します。
内容を修正して commit し、rebase／land を再実行できます。
Land の応答喪失時はキーを保持し、次の `land` で同じ要求を再送します。
`Retry` も保存成功後の応答喪失を含み得るため、同じキーを保持して結果を確認します。
結果が不明なまま別の commit／rebase を作ることは拒否します。
応答待ちの間に編集した場合は、その編集を別の場所へ保全して working copy を clean に戻し、
元の Land を再送してから編集を戻してください。

## 検証

```sh
RUSTC_WRAPPER= cargo test -p kasu-cli --locked
docker build -f tools/cli/Dockerfile -t kasu:cli-test .
```

2026-10-23 に macOS arm64 と固定 Rust 1.90 の Linux arm64 コンテナで検証しました。
実 CLI／gRPC で対象 Tree／Blob の集合を照合し、除外後に追加された subtree を要求しないこと、
Blob upload、累積 commit、file／directory 変換、rebase、Land、競合・再送、
通知、Gitlink、非 UTF-8 パス、未追跡ファイルと symlink 親の保護を検査します。
中断時の rollback／確定後の cleanup と、その間のユーザー編集も検証します。

Linux では [仮想 checkout](virtual-checkout.md) の `kasu mount` も利用できます。
ファイル本文の遅延取得と OverlayFS の copy-on-write を使い、通常の CLI コマンドで編集・着地できます。

## 読み取り範囲のあるアカウント

[read_directories](directory-read-acl.md) を持つ PAT は、sparse 指定なしでも server が許可範囲だけの Tree を返します。
Metadata と Objects の endpoint を権威 server に向けて使用してください。
commit／Land は元の Changeset を親とし、範囲外のファイルを server 上に保持します。
許可外のファイル追加は拒否します。認証主体・読み取り範囲を変える場合は新しい working copy を用意してください。
既にダウンロードしたローカル内容を権限変更で消去する機能はありません。
