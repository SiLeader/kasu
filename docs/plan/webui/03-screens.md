# 03. 画面仕様

## URL 体系

リポジトリ名は `/` を含み得るが、名前規則で `-` 単独のセグメントを禁止している（[06](../06-multi-repository.md#名前規則)）。
そのため `/<repo>/-/<view>/...` の最初の `/-/` でリポジトリ名と画面を分ける。トップレベルの `/-/...` は Web UI 自身の画面に使う。

```
/                                      リポジトリ一覧
/-/login  /-/logout  /-/preferences    認証・利用者設定
/-/new                                 リポジトリ作成
/-/static/<hash>/<file>  /-/healthz  /-/readyz  /-/metrics
/<repo>                                リポジトリトップ（default branch の tree + README）
/<repo>/-/tree/<rev>/<path>            ディレクトリ
/<repo>/-/blob/<rev>/<path>            ファイル
/<repo>/-/raw/<rev>/<path>             生データ
/<repo>/-/blame/<rev>/<path>           blame
/<repo>/-/log/<rev>/<path>             履歴（path 省略で第一親ログ）
/<repo>/-/changeset/<id>               Changeset 詳細
/<repo>/-/compare/<base>...<head>      比較
/<repo>/-/refs                         branch / tag / その他の ref
/<repo>/-/git/<oid>                    Git OID → Changeset へのリダイレクト
/<repo>/-/edit|new|upload|delete/<rev>/<path>   Web 書き込み（GET でフォーム、POST で実行）
/<repo>/-/reviews  /<repo>/-/reviews/new  /<repo>/-/reviews/<n>[/files|/changesets]
/<repo>/-/settings[/<section>]         リポジトリ管理
```

### rev と path のエンコード

- `<rev>` は **1 セグメント**。ref 名の `/` を含むすべての予約文字をパーセントエンコードする（`feature/x` → `feature%2Fx`）。
  axum のパス抽出は `%2F` を復号してしまうため、kasu-web は生の `uri.path()` を自前で分割してから各セグメントを復号する。
- `<rev>` の解決順:
  1. 64 桁の 16 進 → Changeset ID
  2. `refs/` で始まる → 完全な ref 名
  3. `refs/heads/<rev>` → `refs/tags/<rev>` の順に `ResolveRef`
  4. いずれもなければ 404
- `<path>` は残り全体で、各バイトをパーセントエンコードした `/` 区切り。非 UTF-8 のバイトもそのまま往復する。
  `RepoPath` の検証に通らないパスは 400。
- Git OID（40 桁）は `/-/git/<oid>` で `FindChangesetByGitOid` により解決し、`/-/changeset/<id>` へ 302。
- 短縮 Changeset ID は初期版では受け付けない（サーバーに前方一致索引がないため）。表示は先頭 12 桁、リンクは常に全桁。

UI が生成するリンクは常にこの規則でエンコードし、`rev` / `path` の往復一致をプロパティテストで保証する。

## 共通の表示規則

- ヘッダー: リポジトリ名（パンくず）、rev 切替（branch / tag 検索付きドロップダウン、JS なしではリンク一覧ページ）、
  clone URL（HTTP / SSH、`clone_urls` 設定から）、ログイン状態。
- ref で表示中は解決した Changeset ID を併記し、「パーマリンク」で ID 指定 URL に移れる（`y` キーでも移動）。
- `read_filtered=true` の応答を表示するときは「ディレクトリ限定ビュー」のバッジを出す。
- 時刻: Changeset の署名に記録されたタイムゾーンでの絶対時刻と、相対時刻の両方を表示する。利用者設定で表示 TZ を選べる。
- Entry の種類: ファイル（実行可能ビット表示）、symlink（リンク先を表示し、同一リポジトリ内なら blob / tree へリンク）、
  gitlink（サブモジュールの commit OID を表示。内容は取得しない）、ディレクトリ。
- 言語: 英語と日本語。`Accept-Language` を既定に、`/-/preferences` の選択を Cookie（署名不要の表示設定）に保存する。
- アクセシビリティ: 意味的な HTML、キーボード操作（ファイル一覧・行選択・diff 移動）、WCAG 2.1 AA のコントラスト。

## 閲覧

### リポジトリ一覧 `/`

- `ListRepositories` を cursor でページング。prefix 入力で絞り込み（`team-a/` 等）。
- 状態（ReadOnly / Archived）をバッジ表示。Deleting は API が返さない。
- `create_repositories` を持つ利用者には「新規作成」ボタン。

### リポジトリトップ `/<repo>`

- `GetRepository` の `default_branch` を解決して tree を表示し、ルートの README（`README.md` → `README` → `README.*` の順）を描画する。
- default branch が unborn の場合は「空のリポジトリ」画面: clone / push 手順と、書き込み権限があれば「最初のファイルを作成」。
- サイドに refs 数、オープンなレビュー数、使用量（`GetRepositoryUsage`、管理者のみ）。

### ディレクトリ `/-/tree/`

- `ListDir` の entry をディレクトリ → ファイルの順、名前のバイト順で表示する。
- ディレクトリの最終変更 Changeset を `Log(path, limit=1)` で 1 件だけ表示する。
  entry ごとの最終変更は W7 で検討する（[05](05-roadmap.md#w7-拡張)）。
- 当該ディレクトリの README を下に描画する。
- 書き込み権限がある場合: 「新規ファイル」「アップロード」ボタン。

### ファイル `/-/blob/`

- テキスト: 行番号付き表示、ハイライト、行 / 範囲選択（`#L10-L20`、JS なしでも行番号がリンク）。
- Markdown はレンダリング表示とソース表示を切り替える。
- 画像（PNG / JPEG / GIF / WebP、上限内）は raw URL の `<img>` で表示。その他のバイナリはサイズと Git blob OID とダウンロードリンク。
- 操作: raw、blame、履歴、パーマリンク、編集 / 削除（権限がある場合）。

### blame `/-/blame/`

- `Blame` の `LineOrigin` を、連続する同一 Changeset ごとにまとめて表示する（短縮 ID、作者、日付、メッセージ 1 行目）。
- `follow_moves` / `follow_copies` をトグルできる（クエリ `?moves=1&copies=1`）。
- 各行から「この行の由来の時点で blame」へ移動できる（由来 Changeset の親で、由来パスを開く）。

### 履歴 `/-/log/`

- `Log(start, path, limit=50, cursor)`。「次へ」は返された cursor をクエリで渡す（戻るはブラウザ履歴）。
- 各行: 短縮 ID、メッセージ 1 行目、作者、日付、親が複数ならマージ表示、Git commit 射影 OID。

### Changeset `/-/changeset/<id>`

- メッセージ全文、作者 / コミッター、親（リンク）、Git OID、`read_filtered` 表示。
- 第一親との diff（下記 diff 表示）。マージ Changeset では「第一親との差分」を既定に、他の親を選べる。
- この Changeset を含むオープンなレビューがあればリンクする（W6）。

### 比較 `/-/compare/<base>...<head>`

- `MergeBase(base, head)` の結果を基点に diff を表示する（three-dot の意味）。`..`（two-dot）は base と head の直接比較。
- merge-base が複数ある場合は全件を表示し、先頭を使った旨を示す。
- `head` が `base` の子孫かどうか（`IsAncestor`）を表示する。書き込み権限があれば「この差分でレビューを作成」。

### diff 表示（共通コンポーネント）

- ファイル一覧（追加 / 削除 / 変更 / mode 変更 / 種別変更）とファイル単位の折りたたみ。
- unified と side-by-side の切替（利用者設定）。`ContentEdit` の範囲に前後 3 行の文脈を付け、文脈の展開は
  `ReadFile` で取得した内容から行う。
- binary は「バイナリ変更」とサイズのみ。
- 大きい diff は上限で折りたたみ、ファイル単位の読み込みリンクにする（[01](01-architecture.md#表示上限設定可能な既定値)）。
- パスフィルタ（クエリ `?path=`、`Diff.paths` に渡す）。

### refs `/-/refs`

- `ListRefs` を `refs/heads/`、`refs/tags/`、その他に分けて表示し、各先端の Changeset と日付を出す。
- 名前の部分一致で絞り込み（kasu-web 側でのフィルタ）。
- 権限に応じて「branch 作成」「削除」ボタン（下記）。保護 ref（レビュー必須）にはバッジを出す。

## Web からの書き込み

すべて `UploadBlobs` → `CreateChangeset`（親 = 表示中の Changeset）→ 着地、の順で行う。

### 共通フォーム

- コミットメッセージ（必須）、署名（名前 / メール。既定はプロキシヘッダー → アカウントの `display_name` / `email` →
  `<account>@<default_email_domain>`）。
- 着地先の選択:
  1. **直接着地**: 表示中の branch へ `Land`（`required_ancestor` に編集開始時の Changeset を指定しない。
     先端が進んでいても 3-way rebase で適用し、競合時はエラー画面で再編集を促す）。
  2. **新しい branch に作成して着地**: `refs/heads/<name>` を新規作成して `Land`。
  3. **レビューを作成**: 新しい Changeset でレビューを作成する（W6）。
- 着地先がレビュー必須の保護 ref の場合、直接着地は選べず、レビュー作成が既定になる（`GetPermissions.refs[].review_required`）。
- 冪等キーはフォーム表示時に生成した 128 bit 乱数。二重送信・応答喪失後の再送信は同じ結果を返す。
- `Land` の結果: `Landed` → 新 Changeset のファイル表示へリダイレクト、`Conflict` → 競合パスを表示し、
  編集内容をフォームに残して再編集、`Retry` → 再送信ボタン、`Denied` → サーバーの理由を表示。

### 各操作

| 操作 | 画面 | 内容 |
|---|---|---|
| ファイル編集 | `/-/edit/<rev>/<path>` | `<textarea>` による編集（JS 有効時は行番号付き）。テキストかつ上限内のファイルのみ。改行コード・末尾改行・実行可能ビットを保持し、変更可能にする。改名（パス変更）も可 |
| 新規ファイル | `/-/new/<rev>/<dir>` | パスと内容を入力。既存パスとの衝突は 409 |
| アップロード | `/-/upload/<rev>/<dir>` | `multipart/form-data` で複数ファイル。上限内のみ。既存ファイルの上書きは確認チェック必須 |
| 削除 | `/-/delete/<rev>/<path>` | ファイル / ディレクトリ（配下全削除）。確認画面を挟む |
| branch 作成 | `/-/refs` | 名前と起点 rev を入力し、`UpdateRef(expected = 不存在)` |
| ref 削除 | `/-/refs` | `UpdateRef(expected = 表示時の target / version, target = なし)`。保護 ref は不可 |

- 編集フォームには編集開始時の blob ID を hidden で持ち、送信時に現在の先端で同じパスが変わっていれば
  警告を表示する（Land の 3-way rebase に任せる前に、利用者に気付かせる）。
- force 更新・tag の置換は Web UI では提供しない（CLI / API を使う）。

## リポジトリ管理

`GetPermissions.admin` が true の利用者に `/<repo>/-/settings` を表示する。作成は `/-/new`。

| セクション | 内容 | API |
|---|---|---|
| 一般 | default branch の変更 | `UpdateRepository(default_branch)` |
| レビュー | 保護 ref と承認ルール（[04](04-review.md#レビューポリシー)）のフォーム編集 | `UpdateRepository(config_json)` |
| 書き込みポリシー / 上限 | `write_policy` / `limits` の JSON 編集（スキーマ検証付き） | 同上 |
| Webhook | endpoint 一覧の追加・削除・URL 変更。`secret_file` はサーバー上のパスとして編集し、秘密の内容は扱わない | 同上 |
| 状態 | Active / ReadOnly / Archived の切替 | `UpdateRepository(state)` |
| 改名 | 新しい名前、旧名を alias として残すか | `RenameRepository` |
| フォーク | 元 ref と新しい名前 | `ForkRepository` |
| 使用量 | Changeset / tree / blob 数、論理サイズ、集計完了フラグ | `GetRepositoryUsage` |
| 削除 | リポジトリ名の再入力による確認 | `DeleteRepository` |

- 設定の更新は表示時の `config_version` を hidden で持ち、CAS 不一致（他者の変更）なら差分を示して再編集させる。
- `config_json` の未知フィールドは kasu-server が拒否する。Web のフォームは既知フィールドのみを編集し、
  それ以外のフィールドは JSON 編集でそのまま保持する。
- 作成 `/-/new`: 名前（名前規則をクライアント側でも検査）、default branch、初期設定。冪等キー付き。
