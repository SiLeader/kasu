# Git 取り込みと OID の保持

[English](git-import.md) | 日本語

```sh
cargo run -p kasu-ingest -- <Git リポジトリ> <保存ディレクトリ> <リポジトリ名>
```

未登録のリポジトリは `--create` で作成してから取り込みます（`kasu-ingest --create <Git リポジトリ> <保存ディレクトリ> team-a/infra`）。
`--create` なしの未登録名はエラーで、レジストリが空の場合だけ名前を legacy キーとして登録します
（[マルチリポジトリ](multi-repository_ja.md#起動と移行)）。リポジトリが異なる取り込みは並行実行できます。

Git の bare／working repository を gix で読み、親・依存オブジェクトを先に処理して Changeset に変換します。
保存先は `meta.db`（SQLite）と `objects/`（zstd の content-addressed ストア）です。
`RUST_LOG` で tracing のログレベルを指定できます。

branch は `refs/heads/*` という名前のネイティブ ref に変換します。tag と HEAD は Git 専用の ref に保存します。
取り込み中は不変データだけを書き込み、接続性・型・tree の再現性の検証後に、全 ref と対応するログを
単一トランザクションで公開します。CAS の競合時には全 ref を更新せずエラーを返します。
同じ取り込みの繰り返しは冪等で、ref が同じなら ref_log も増えません。

## ロスレスな表現

- commit と tag の raw バイト列を保持します。署名・追加ヘッダ・メッセージの改行を正規化しません。
- tree も元の OID ごとに raw を保持します。同じネイティブ Tree に対応する複数の非正規 tree が共存できます。
- file／executable／symlink／gitlink と、非 UTF-8 のパス・人名・メール・メッセージに対応します。
- blob はサイズと Git OID を同時に記録します。
- 索引のキーにリポジトリを含めます。他のリポジトリに同じ raw オブジェクトが存在していても、その索引からは検索できません。
- ネイティブ commit が Git を経由して戻ってきた場合は、既存の Changeset ID を再利用します。

ネイティブ Changeset の Git 合成は v1 として固定しました。全親を順に書き、`kasu-changeset` ヘッダを必ず含め、
メッセージはそのまま出力します。`GitTree` と `GitCommit` は ProjectionId を SubKey とする Derived Data です。
共有 tree ノードの変換結果も保存し、変更されたノードだけを合成します。

## 明示的に拒否する入力

現在は SHA-1 リポジトリを対象とします。SHA-256、shallow 履歴、欠落／型不一致の依存、重複した tree 名、
パスとして表現できない名前、FileChange では再現できない空ディレクトリ、HEAD 以外の symbolic ref はエラーにします。
各オブジェクトはヘッダ段階と読み込み後に 256 MiB 上限を確認します。
submodule の内容は取得せず、gitlink の commit OID のみを保持します。

## 検証

通常のテストでは、非 UTF-8、submodule、symlink、executable、2-parent マージ、tag→tag、tree／blob タグを含む
fixture をその場で生成します。Git の pack にまとめた source から取り込み、すべての raw バイト列を照合します。
`write_pack` の非デルタ pack を `git index-pack --stdin --strict` で取り込み、ref 復元後に
`git fsck --strict` と clone を実行します。非正規 tree は別のテストで raw OID と内容を照合します。
非正規入力そのものが Git の strict 検査に通るとは主張しません。

実在 OSS の検証には [libgit2/TestGitRepository](https://github.com/libgit2/TestGitRepository) を使用しています。
`crates/kasu-ingest/tests/fixtures/oss-refs.txt` に固定した 7 ref（21 commit、70 object）を全件照合します。
このリポジトリには octopus merge と commit／tree／blob を指す tag が含まれます。
固定 libgit2 release の gitlink fixture による追加検証は下記を参照してください。
非 UTF-8 パス・gitlink・annotated tag・merge をまとめて持つ固定 GitLab upstream 入力も追加し、下記のとおり全183 OID の往復を検証しています。

```sh
KASU_OSS_REPO=/path/to/TestGitRepository.git cargo test \
  -p kasu-ingest --test roundtrip pinned_oss_repository --locked -- --ignored
```

ネットワーク依存のテストは通常のテストから分離しています。夜間 CI は固定した OID を fetch して実行します。
GitHub 上での夜間 CI 実行はまだ未確認です。

[Smart HTTP の読み取り](git-read_ja.md) で取り込んだリポジトリを配信できます。
受信 pack と書き込みは [Git push](git-push_ja.md)、実コントローラーとの同期は [GitOps E2E](gitops-e2e_ja.md) に記録しています。
ネイティブ commit の Git 合成では、NUL を含むメッセージや Git の署名ヘッダに表現できない値は明示的に拒否します。

## 追加 OSS 入力の選定記録

2026-10-26 に libgit2 v1.9.0（commit `338e6fb681369ff0537719095e22ce9dc602dbf0`）の
upstream fixture 4件を Git オブジェクトとして調べました。
`submodules.git` は2 commit・6 object、gitlink 1件で strict fsck が通過しました。
`binaryunicode` も通過しましたが、いずれも非 UTF-8 の tree 名はありませんでした。
`nasty` と `testrepo.git` は strict fsck が失敗するため、そのまま正常入力の受け入れ証拠には使いません。
[選定結果](git-import-upstream-research.json) は Git 側の調査であり、Kasu の取り込み検証済みとは区別します。
この選定調査に続く gitlink の Kasu 往復検証は下記のとおり完了しました。
この初期調査の後、下記の GitLab upstream branch で非 UTF-8 パスを含む固定入力を選定し、Kasu での往復検証を完了しました。
追加で libgit2 の loose object 2,308件と JGit の pack 7件・tree 42件の名前を調べましたが、該当する非 UTF-8 名は見つかりませんでした。
gitoxide の fixture スクリプト242件も選定用に読みましたが、アーカイブ全体の不在を証明する調査ではありません。
go-git の固定コーパスでも小さな SHA-1 pack 22件・延べ tree 3,704件を検査しましたが、該当する名前はありませんでした。
大きな未取得 pack と、基底不足の pack 各1件は対象外です。各固定 commit と調査範囲は同じ選定結果 JSON に記録しています。


## 固定 upstream gitlink fixture の往復

libgit2 の固定 commit [`338e6fb681369ff0537719095e22ce9dc602dbf0`](https://github.com/libgit2/libgit2/tree/338e6fb681369ff0537719095e22ce9dc602dbf0/tests/resources/submodules.git) の
`tests/resources/submodules.git` を使います。これは upstream のテスト入力で、実プロダクトの大規模な履歴を代表するものではありません。
2 commit・2 tree・2 blob の到達可能な全6 object と、1件の gitlink を持ちます。
元の ODB にある3個の dangling blob は検証対象の履歴から除外します。

```sh
python3 tools/oss-fixtures/prepare.py --output /tmp/kasu-upstream-submodule.git
KASU_OSS_SUBMODULE_REPO=/tmp/kasu-upstream-submodule.git \
  cargo test -p kasu-ingest --test roundtrip pinned_upstream_submodule --locked -- --ignored
```

出力先にはまだ存在しないディレクトリを指定します。
準備ツールは固定 commit を fetch し、archive の regular file から HEAD／refs／objects だけを読みます。
upstream の config／hooks／alternates と worktree の symlink を持ち込みません。
Git の local upload-pack で到達可能な履歴を bare clone し、全6 OID と ref の固定値、strict fsck を確認します。
`--source /path/to/libgit2.git` で、固定 commit と必要な blob があるローカル source も使えます。
両方の取得方法が同じ manifest を作ることを確認しています。

Kasu のテストは各 object の raw bytes、生成 pack の strict index-pack、ref／tag の peel、
symbolic HEAD、export／bare clone の strict fsck と全 OID を照合します。
native root の `testrepo` が、元の target `a65fedf39aefe402d3bb6e24df4d4f5fe4547750` を持つ
`Entry::Gitlink` であることと、target commit を取り込まないことも検査します。
submodule の内容を clone／追跡する機能は追加しません。

macOS arm64／Git 2.54.0 では既存 TestGitRepository と次節の GitLab 入力を含む11テスト、
固定 Rust 1.90／Linux arm64／Git 2.39.5 では10テストが通過しました。
Linux では fixture を読み取り専用で mount し、コンテナ内の一時領域へコピーして Git の所有者検査を維持します。
実行中の network は無効です。

```sh
python3 tools/oss-fixtures/prepare_byte_paths.py --output /tmp/kasu-upstream-byte-paths.git
docker build -f tools/oss-fixtures/Dockerfile -t kasu:oss-fixtures-test .
docker run --rm --network none --user "$(id -u):$(id -g)" \
  --mount type=bind,source=/tmp/kasu-upstream-submodule.git,target=/fixtures/submodules.git,readonly \
  --mount type=bind,source=/tmp/kasu-upstream-byte-paths.git,target=/fixtures/byte-paths.git,readonly \
  kasu:oss-fixtures-test
```

[入力 manifest・実行ログ・ソースハッシュ](git-import-upstream-submodule/summary.json) と
[CI](../.github/workflows/oss-roundtrip.yml) を追加しています。GitHub 上での実行結果は未確認です。
非 UTF-8 パスを含む受け入れ入力は次節の GitLab fixture です。

## 固定 upstream byte-path fixture の往復

[GitLab.org/gitlab-test](https://gitlab.com/gitlab-org/gitlab-test) が公開する
`invalid-utf8-diff-paths` の固定 tip
[`99e4853698007ed7e68826730ef5313d24841991`](https://gitlab.com/gitlab-org/gitlab-test/-/commit/99e4853698007ed7e68826730ef5313d24841991) と、
annotated tag `v1.0.0` の固定 OID `f4e6814c3e4e7a0de82a9e7cd20c626cc963a2f8` を使います。
全親履歴を持つ50 commit・14 merge、76 tree、56 blob、tag 1件の全183 object です。
この固定2 ref から到達するオブジェクトをすべて対象にします。上流の他 ref は対象外です。
これは外部で作成されたテスト用リポジトリで、大規模な実運用の履歴を代表するものではありません。

ルート直下のパス `hello\x80world` は、hex では `68656c6c6f80776f726c64` です。
文字列としての UTF-8 には変換できません。

| 操作 | 元 commit | blob OID |
|---|---|---|
| 追加 | `d0888d297eadcd7a345427915c309413b1231e65` | `3742e48c1108ced3bf45ac633b34b65ac3f2af04` |
| 内容更新 | `19950f03c765f7ac8723a73a0599764095f52fc0` | `ea20973903d3800ac659d503596f9105f0a7423f` |
| 削除 | `99e4853698007ed7e68826730ef5313d24841991` | なし |

```sh
python3 tools/oss-fixtures/prepare_byte_paths.py --output /tmp/kasu-byte-paths.git
KASU_OSS_BYTE_PATH_REPO=/tmp/kasu-byte-paths.git \
  cargo test -p kasu-ingest --test roundtrip pinned_gitlab --locked -- --ignored
```

出力先は存在しない directory を指定します。準備ツールは固定 commit／tag を fetch し、
[183件の固定 OID と型](../crates/kasu-ingest/tests/fixtures/upstream-byte-path-objects.txt)、全到達集合、
ref／HEAD、strict fsck、14 merge、上記パスの3 snapshot と tag の peel を検査してから出力を公開します。
shallow boundary はありません。bare repository のまま扱い、submodule や LFS の外部実体は取得しません。

`--source /path/to/cached-repo.git` では、同じ183件の raw object を読み、元 OID を照合して新しい bare repository に格納します。
新しい出力に source の config／hooks／alternates／worktree を引き継ぎません。
公開元からの取得と cached source からの準備が、同じ manifest を作ることも確認しました。

Kasu 取り込み後は次を検査します。

- 全183 object の raw bytes と OID、strict index-pack、export／bare clone の strict fsck と全 OID。
- branch、annotated tag と peel、symbolic HEAD の保持。
- native tree で上記の生パス・内容を保持し、Set／Delete が同じパスを指すこと。
- 全50 commit の native 親リストと Git の親リストの順序付き一致、merge 14件。
- HEAD の `gitlab-grack`、`gitlab-shell`、`six` が元 target OID の native Gitlink になり、target commit を取り込まないこと。

macOS arm64／Git 2.54.0 の11テストと、固定 Rust 1.90／Linux arm64／Git 2.39.5 の10テストが通過しました。
Linux は前節の Docker コマンドで、入力2件を読み取り専用で渡し、network を無効にして実行します。
APFS 上で非 UTF-8 名の working tree を実体化する試験ではありません。

[summary.json](git-import-byte-paths/summary.json)、[入力 manifest](git-import-byte-paths/preparation.json)、
[macOS ログ](git-import-byte-paths/macos.log)、[Linux ログ](git-import-byte-paths/linux.log)、
[Linux ビルド情報](git-import-byte-paths/linux-build.log) に結果とソース hash を保存しています。
`tools/oss-fixtures/record_byte_paths.py` が準備結果の一致、件数、テスト名、Linux のソース hash と image manifest を照合します。
夜間 workflow にも追加しました。GitHub 上での実行結果は未確認です。
