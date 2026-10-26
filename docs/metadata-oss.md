# 固定 OSS の Metadata 差分検証

[libgit2/TestGitRepository](https://github.com/libgit2/TestGitRepository) の
[固定7 ref](../crates/kasu-ingest/tests/fixtures/oss-refs.txt) を取り込み、ref を公開した後の Metadata backend API を Git と比較します。
入力は21 commit・70 object です。生成 fixture の比較に加えて、外部で作成された履歴を検証するテストです。

2026-10-26、macOS arm64（Git 2.54.0）と固定 Rust 1.90 の Linux arm64（Git 2.39.5）で、両方とも次の全件が一致しました。

| 問い合わせ | 各環境の件数 | 比較対象 |
|---|---:|---|
| パス履歴 | 189 | 全21 commit × 全8パスと存在しない1パス。`git log --first-parent --format=%H -- <path>` |
| Blame | 77 | 全 commit の全 regular file snapshot。`git blame --line-porcelain` の由来 OID・元行番号・由来パス |
| MergeBase | 441 | 全21 × 21の順序付き tip ペア。`git merge-base --all` の集合 |

Log は2件ずつページを取得し、終端と cursor の重複がないことも検査します。
Log の第一親指定は [API 計画](plan/03-protocols-and-api.md) の意味に合わせています。
MergeBase は同じ tip、自分自身が祖先の tip、分岐、無関係な root、octopus merge を含む全組を対象にします。
共通祖先なしの Git 終了コード1は、stdout／stderr が空の場合だけ受け入れます。
取り込み前には全 ref の固定値と strict fsck、取り込み後には commit／object 数を確認します。

```sh
KASU_OSS_REPO=/path/to/pinned-TestGitRepository.git RUSTC_WRAPPER= \
  cargo test -p kasu-server --test oss_metadata --locked -- --ignored --nocapture
```

入力は [Git 取り込みの固定 fixture](git-import.md#検証) と共通です。
固定 OID からの準備手順は [夜間 workflow](../.github/workflows/oss-metadata.yml) にあります。
通常の `cargo test` では外部入力を要求するこのテストを実行しません。

Linux では次のコマンドを使います。入力を読み取り専用で渡し、コンテナ内の一時ディレクトリにコピーして実行します。
Rust イメージは digest で固定し、ネットワークはテスト実行時に無効にします。

```sh
docker build -f tools/oss-fixtures/metadata.Dockerfile -t kasu:oss-metadata-test .
docker run --rm --network none --user "$(id -u):$(id -g)" \
  --mount type=bind,source=/absolute/path/to/pinned-TestGitRepository.git,target=/fixtures/TestGitRepository.git,readonly \
  kasu:oss-metadata-test
```

[summary.json](metadata-oss/summary.json)、[macOS ログ](metadata-oss/macos.log)、
[Linux ログ](metadata-oss/linux.log)、[Linux ビルド情報](metadata-oss/linux-build.log) に件数とソースの SHA-256 を保存しています。
`tools/oss-fixtures/record-metadata.py` がテスト名・件数・ビルド時のソース hash を照合して記録します。
GitHub 上の workflow の実行結果は未確認です。

この入力は小さく、実在 OSS 全般や Git の全 rename／copy ヒューリスティクスを代表しません。
非 UTF-8 パスも含みません。行移動・copy と transport の比較は [別の生成履歴テスト](blame-lineage.md) が対象です。
