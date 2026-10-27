# Git パス射影

[English](git-projection.md) | 日本語

指定ディレクトリを root とする読み取り用の Git リポジトリを、同じネイティブストアから配信します。
サーバーで `git subtree` や `git upload-pack` は起動しません。

```sh
git clone http://git:dev-token@127.0.0.1:50080/my-repo/-/path/deploy.git
git clone --depth=1 http://git:dev-token@127.0.0.1:50080/my-repo/-/path/deploy.git
git clone --filter=blob:none http://git:dev-token@127.0.0.1:50080/my-repo/-/path/deploy.git
```

この例では `deploy/manifest.yaml` が clone 先の `manifest.yaml` になります。
source の同じ branch 名を使い、ディレクトリ外だけの更新では射影 branch の OID は変わりません。
`my-repo.git/-/path/deploy.git` の形式も使えます。
非 UTF-8 のパスはバイト列の hex で指定します。たとえば `0xff` のディレクトリは
`my-repo/-/path-hex/ff.git`、`deploy` は `my-repo/-/path-hex/6465706c6f79.git` です。
どちらの指定方法でも、同じ repo／パスは同じ射影になります。空・絶対・dot component・NUL のパスは拒否します。

URL 末尾の `.git` は省略可能なサフィックスで、ディレクトリ名には含みません。
`app.git` ディレクトリは `my-repo/-/path/app.git.git`、または
`my-repo/-/path-hex/6170702e676974.git` と指定してください。HTTP／SSH とも同じ規則です。

## 履歴の生成規則 v1

`ProjectionId` は repo とバイト列パスを持ち、Derived Data の SubKey と Git OID 索引の namespace に使います。
通常リポジトリの既存キー形式は変えず、パス射影には別の型タグを使います。
元の commit をそのままコピーせず、次の規則で `PathCommit` を反復導出します。

1. 各 source Changeset の RootTree から指定した subtree を取り出し、Git tree を合成します。
   パスがない場合や途中・末端が file／symlink／gitlink の場合は空 tree とします。
2. 全 source 親を射影した先端を、source の親順に列挙します。履歴がまだない親を除き、重複を除きます。
3. 別の射影親の祖先になっている親を除きます。判定には射影 Git DAG を使います。
4. 親がなく tree も空なら、射影 commit は作りません。
5. 親が一つで、その tree と同じなら、その親を現在の射影先端とします。
6. それ以外は新しい commit を作ります。独立した複数の射影親を結ぶ merge は、第一親と tree が同じでも保持します。
7. ディレクトリ削除は空 tree の commit として残します。再作成時にはその削除 commit が親になります。

author／committer／message は source Changeset の値を使い、`kasu-changeset` と
`kasu-projection-v1` ヘッダーを含めます。時刻や接続先に依存しません。
tree はネイティブの File／Symlink／Gitlink から合成し、元の非正規 Git mode や tree の並びは射影では正規化します。
元 commit の追加ヘッダーや署名は継承しません。メッセージはパスに応じて編集しません。

この設計は [Git subtree split](https://raw.githubusercontent.com/git/git/v2.54.0/contrib/subtree/git-subtree.adoc)
の目的に沿うものですが、OID や merge の選択を Git subtree と同一にする仕様ではありません。
上の v1 規則とゴールデン OID を固定します。

## Ref と tag

| source の ref 先 | 射影先 |
|---|---|
| commit | その commit の射影先端。該当パスの履歴がなければ ref を省略 |
| tree | その tree の指定 subtree。ディレクトリがなければ ref を省略 |
| blob | ディレクトリへの射影がないため ref を省略 |
| annotated tag／tag の連鎖 | 上記の先端へ object 行を付け替え、tag の連鎖を再構成 |
| symbolic ref | source の target 名を維持。未生成の HEAD は unborn として扱う |

書き換えた annotated tag の元署名は有効ではないため、Git が認識する PGP／SSH／X.509 の署名開始行から後を除去します。
開始形式は [Git 2.54.0 の実装](https://raw.githubusercontent.com/git/git/v2.54.0/gpg-interface.c) を参照しています。
元の署名の検証や、新しい署名の付与は行いません。

## 認可と通信

Basic／Bearer による既存の repo 単位認可を、ref 広告や射影導出より前に実行します。
want／have の索引と到達可能性は射影ごとに分離します。元の commit、パス外の blob、別の射影の commit は、
元 repo に存在するだけでは want できません。同じ内容の blob／tree が複数の射影で同じ Git OID を持つことはあります。

v0／v1／v2 の clone／fetch、shallow、`blob:none` と後続 promisor fetch は同じ upload-pack を使います。
receive-pack の広告・push は HTTP 405 で拒否します。書き込みは元リポジトリを使います。

[射影専用アカウント](projection-accounts_ja.md) は、許可したディレクトリの射影だけを読み取り可能にします。
通常の運用者 PAT は元 repo 全体の権限を持ちます。source の author／message／ref 名を隠す仕組みではありません。
ネイティブ API のディレクトリ単位読み取りや、アカウントごとの書き込み ACL は別の残作業です。
初回導出は source の全親履歴を処理し、結果は不変キーで再利用します。大規模 repo の初回時間・複数射影の容量評価は未実施です。

## 検証

macOS arm64 の Git 2.54.0 と、固定 Rust 1.90／Linux arm64 の Git 2.39.5 で検証しています。

- v0／v1／v2、それぞれ全履歴・depth=1・partial clone、strict fsck。
- パス外だけの更新で OID が変わらず、パス内更新の fetch で内容が更新されること。
- annotated／nested tag の付け替え、tree tag、blob tag の省略、元署名 suffix の除去。
- 元の commit／パス外 blob／別射影の commit の want 拒否、push 拒否、非 UTF-8 のパスの HTTP clone。
- 単体 DAG で、独立した merge の保持、祖先親の除去、削除・再作成、file への置換、キャッシュ再利用。
- 生成 pack の `git index-pack --strict`／`git fsck --strict` と、決定的 OID のゴールデン。

実通信の記録は [互換性マトリクス](../crates/kasu-git/tests/fixtures/clients/matrix.json)、
ソース・通信のハッシュと範囲は [検証結果](../crates/kasu-git/tests/fixtures/clients/path-projection-summary.json) を参照してください。
ArgoCD v3.5.6／Flux v2.9.6 にパス射影 URL を設定した [kind E2E](gitops-e2e_ja.md#パス射影-url) も通過しました。
初回・射影外のみの更新・射影内更新の3段階で、両者の SHA／ConfigMap、refresh の受理、
実 clone／fetch の subtree 一致と strict fsck を確認しています。
[通信と同期結果](../crates/kasu-git/tests/fixtures/clients/path-projection-kind/gitops-kind-summary.json) を保存しています。

```sh
cargo test -p kasu-git --test path_projection --locked
KASU_CAPTURE_DIR=/tmp/kasu-path-traces \
  cargo test -p kasu-server --test git_http path_projection --locked
docker build -f tools/git-compat/Dockerfile -t kasu:path-projection-test .
```
