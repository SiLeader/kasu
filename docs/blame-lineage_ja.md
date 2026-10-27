# Rename／copy の行追跡

[English](blame-lineage.md) | 日本語

Blame は rename 前のパスまで追跡し、行ごとの Changeset・元のパス・元の行番号を返します。
同一ファイル内の移動は `--follow-moves`、別ファイルからの行断片やファイル全体の copy は
`--follow-copies` で追跡します。copy 追跡は移動追跡も有効にします。既定では両方とも false です。

```sh
kasu blame path/to/file
kasu blame --follow-moves path/to/file
kasu blame --follow-copies path/to/copied-file
```

gRPC と HTTP JSON の `/v1/blame` は同じ指定を受け付けます。
protobuf の既存 field 1〜4 は変えず、`follow_moves` を bool の field 5 に追加しています。
`follow_copies` は field 4 です。省略した旧クライアントは従来どおり追跡オプションが false になります。
ReadFile／ListDir と共有する PathRequest ですが、`follow_copies`／`follow_moves` は Blame だけに適用します。

## Derived Data

| 型 | キー・内容 |
|---|---|
| `PathIndex@1` | SubKey=BlobId。第一親との差分で、その内容を持つパスのソート済み集合を維持。`under(prefix)` はパス境界で絞り込み |
| `CopyDetect@1` | SubKey=RepoPath。全親と現在の tree を比較し、親 Changeset・source path・rename/copy・類似度を返す |
| `Blame@2` | SubKey=RepoPath。同じパス、または検出した rename 元の親 Blame を参照 |
| `LineMatches@1` | SubKey=(RepoPath, copies)。親 snapshot から同一パス／rename の一致行と移動／copy の断片を選び、元の親・パス・行を保存 |
| `MovedBlame@1` | SubKey=RepoPath。同一ファイル／rename 元の移動も追跡 |
| `CopyBlame@2` | SubKey=RepoPath。上記に加え、ファイル全体と行断片の copy 元を追跡 |

すべて Lazy です。通常の Changeset 作成で PathIndex／CopyDetect を作ったり、全 blob を索引に追加したりしません。
PathIndex は要求された BlobId の第一親履歴をバックフィルします。変更パスを除去し、同じ BlobId の Set を追加するため、
merge も第一親からの最終差分に従います。複数 blob を要求する大規模履歴でのバックフィル時間・容量は未評価です。

Blame@1／@2 と CopyBlame@1 の不変レコードは残します。
新しい CopyBlame@2／MovedBlame@1 は別キーで、通常の Blame@2 と PathHistory の意味は変わりません。
copy が作られた時点の親 snapshot に source がなければ、さらに古い履歴の同じ内容には結び付けません。
単に過去に同じ blob があっただけの削除後再作成では、古い帰属を復活させません。

## 検出規則

各親に同じパスの file／symlink がある場合、通常の行 diff を使います。なければ次の規則で source を選びます。

1. PathIndex で、現在と同じ BlobId の親パスを検索します。regular file と symlink は別の種類として比較します。
2. 削除 source と追加 destination を、同じ内容・種類のグループ内でパスのバイト順に一対一で割り当てます。
   削除 source が割り当てられた destination は rename、残る完全一致は copy です。
3. 完全一致がない regular file は、削除と追加の候補を行 diff で比較します。
   `一致した行の bytes / max(元 bytes, 新 bytes)` が 50% 以上なら候補にします。
4. 完全一致の組を先に予約し、類似度の高い順、destination／source のバイト順で一対一に割り当てます。
5. rename 元、または copy 追跡指定時の完全一致 source の親 Blame から、変更されていない行の帰属を引き継ぎます。
   merge では全親を調べ、同一 blob の親があればその帰属を採用します。それ以外は親順で一致行を引き継ぎます。

NUL を含む内容と 8 MiB を超える内容は類似度推定の対象外です。完全一致の検出は内容 ID で行えます。
類似度の候補比較は一回 1,000 組までで、超過時は明示的にエラーを返し、不完全な CopyDetect の結果は保存しません。

CopyDetect 自体は引き続き上の v1 規則です。Git の内部の byte block による類似度・曖昧な候補の選択と同一ではありません。

## 行断片の移動／copy

LineMatches はまず従来と同じ行 diff で、同一パス／rename 元の一致行を全親から引き継ぎます。
親の blob と完全一致する場合は、その親の行をすべて引き継ぎ、断片の検索は行いません。
それ以外の未割り当て行について、次の順で検索します。

1. 同一パス／rename 元の regular file から、場所によらず完全一致する連続行を探します。
   ASCII の英数字が合計20文字以上ある断片を移動として扱います。
2. copy 有効時は、各親 snapshot の他の regular file から未割り当ての連続行を探します。
   新規ファイルと既存ファイルの両方が対象で、断片には合計40文字以上の ASCII 英数字が必要です。
3. 英数字数が多い断片、行数が多い断片、親順、パスのバイト順、元の行、移動先の行の順で選びます。
   選択済みの断片と重なる候補は残る連続部分ごとに再採点します。
4. 選ばれた元ファイルの親 Blame だけを追加依存にします。元行の帰属がさらに古い移動／copy を参照していれば、
   その Changeset・パス・行番号まで引き継ぎます。新しい行は現在の Changeset に帰属します。

親の時点で存在しないファイルを、さらに古い snapshot から探すことはありません。
symlink と NUL を含む内容は断片検索から除外します。従来の完全一致 copy はこれらも追跡できます。
断片検索は8 MiB以下・10万行以下、他ファイルの候補は全親の合計1,000個までです。
行位置の索引作成・候補探索・一致行の延長を合計100万ステップまでに制限します。
行数・候補数・比較量の超過は明示的なエラーで、部分的な LineMatches／Blame を保存しません。
断片を探す必要のない root Changeset や親と完全一致するファイルには探索上限を適用しません。

[Git blame の `-M`／`-C`](https://git-scm.com/docs/git-blame) と同じ用途ですが、
候補探索、採点の境界、短い完全一致 copy、同点候補の選択まで同一にする仕様ではありません。
`--follow-copies` は変更されていない親ファイルも検索し、Git の `-C` 1回より広い範囲を対象にします。

## 依存スケジューラ

型の通常依存を解決した後、`resolved_dependencies` を評価する段階を追加しました。
CopyDetect は RootTree を確認して PathIndex のキーを選び、Blame は CopyDetect を確認して親の別パスを選びます。
LineMatches も内容の一致を先に選び、実際に寄与する元の親 Blame だけを追加依存にします。
その依存も同じ反復スケジューラで解決します。計算中に Engine を再帰的に呼び出す方式にはしません。
通常依存と追加依存の双方で循環を検出し、最終値の保存には既存の singleflight／put-if-absent を使います。

## 検証

macOS arm64 の Git 2.54.0 と、固定 Rust 1.90／Linux arm64 の Git 2.39.5 で、
純粋な rename、編集した rename、copy、copy 後の編集、行ブロックの移動、既存／新規ファイルへの断片 copy、
断片 copy 後の編集について、通常／移動／copy の3モード・計24通りの行帰属を比較しています。
既定の結果は通常の `git blame --line-porcelain`、copy 有効時は完全一致 copy の fixture を
`git blame -C1 -C1 --line-porcelain` と比較します。移動 fixture は `-M`、断片 copy fixture は `-M -C -C -C` を使用し、
Changeset に対応する Git OID・元の行番号・元のパスが一致しました。
Backend、実 gRPC、HTTP JSON の結果も一致します。

単体・プロパティ検証では、非 UTF-8 source、binary の完全一致、第二親の rename、削除・再作成、
一対一の競合解決、1,024回の rename、追加依存の循環、比較上限、PathIndex と RootTree の一致を確認しています。
移動した行ブロックのランダムな並べ替えでも全行の元行番号を比較し、第二親の断片 copy、
短い共通行を copy とみなさないこと、候補数／比較量の上限、大きな root の検索省略も検証しています。
実 CLI E2E は `blame --follow-copies`／`--follow-moves` の要求が RPC を通ることも検証します。
実在 OSS での rename／copy parity、大規模な検索性能、上記の Git ヒューリスティクスとの差分は残作業です。

[比較結果とソースのハッシュ](simulations/blame-lineage/summary.json) を保存しています。

```sh
cargo test -p kasu-derived --test lineage --locked
KASU_CAPTURE_DIR=/tmp/kasu-lineage-evidence \
  cargo test -p kasu-server --test metadata --locked
docker build -f tools/git-compat/Dockerfile -t kasu:lineage-test .
```
