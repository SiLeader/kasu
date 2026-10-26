# ネイティブ3-wayマージの基盤

`kasu-ops::merge_text(base, ours, theirs)` と `merge_trees(repo, base, ours, theirs)` は
M6 の基盤です。tree 演算は呼び出し元が明示した共通基底を使用します。
Changeset 単位の `Operations::merge`／`rebase` と [Write API](write-api.md) からも使用します。
[Land](land.md) の再適用にも使用します。

`merge_text` は改行を保持するバイト列の行で差分を計算します。
UTF-8 への変換を行わず、末尾改行の有無も保持します。
双方が同じ変更をした場合と片側だけの変更は、その内容を採用します。
離れた行の変更は統合し、重なる編集や同じ境界に接する編集をまとめて比較します。
競合時は、基底の0始まり半開行範囲と両側のバイト列を全競合箇所について返します。
インラインの競合マーカーは保存しません。双方が異なる変更をした NUL を含む内容は binary conflict です。

`merge_trees` は明示スタックで tree をたどり、変更されないサブツリーの ID を保持します。
同じパスの通常ファイルは内容と実行権限をそれぞれ3-wayで処理します。
双方が別の子ファイルを追加したディレクトリは統合します。
片側による最後の旧ファイルの削除も、反対側の独立した新ファイル追加と統合できます。
symlink と gitlink の内容は不透明な値として扱い、双方の異なる変更は競合にします。
rename／copy の推測は行いません。

`TreeConflict` はバイト列パス、競合の種類、base／ours／theirs の `TreeEntry`、
内容競合の行範囲を保持します。種類は content、binary、add/add、delete/modify、
file/directory、type change、symlink、gitlink です。
競合が1件でもあれば `TreeMerge.tree` は `None` となります。
tree 演算中の不変 blob／tree は保存できますが、Changeset や ref は公開しません。

## Changeset 単位の演算

`merge(a, b, metadata)` は、片側が祖先なら子孫を返します。
分岐していれば共通基底と両先端を3-way merge し、両親を `[a, b]` とする Changeset を作成します。
共通祖先がなければ空 tree が基底です。
複数の best merge-base は ID 順に、共通基底の計算を伴う仮想マージで統合します。
この処理も明示スタックを使い、仮想 Changeset を最終結果の親へ追加しません。
基底の統合自体に競合があれば、`MergeBases` stage と元の best base 一覧を返します。
競合を含む仮想基底へマーカーを埋めて後続演算を続ける方式は採用していません。

`rebase(cs, onto, committer)` は1 Changeset の第一親との差分を `onto` へ再適用します。
author と message を保持し、committer を置き換え、親を `[onto]` にします。
ルート Changeset は空 tree との差分、merge Changeset は第一親との差分として扱い、後者は親を1つにします。
履歴全体をまとめて付け替える API ではありません。
いずれも ref の更新は行いません。API は成功した Changeset を指定 repo に所属させます。

検証:

```sh
RUSTC_WRAPPER= cargo test -p kasu-ops --test merge --locked
RUSTC_WRAPPER= cargo test -p kasu-ops --test rewrite --locked
```

挿入・削除・置換の位置境界144通りと追加 fixture を Git の `merge-file` と比較しています。
同値・片側変更・左右対称性を proptest で検査し、独立した内容／モード変更、削除と追加、
file/directory、symlink、gitlink、binary、type change、全競合箇所の保持を個別に検証します。
Changeset の親・署名・message、fast-forward、root／merge Changeset の再適用、
criss-cross の2個／3個の best base と基底自体の競合も検証します。
