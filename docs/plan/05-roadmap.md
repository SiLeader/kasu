# 05. ロードマップ・テスト戦略・リスク

## マイルストーン

各マイルストーンは「動くものを E2E テスト付きで出す」縦切りの単位。Git 互換の段階（G1〜G6）は 04 を参照。

### M0: 基盤とロスレスなオブジェクトモデル（2 週）
- Cargo workspace 化、CI（fmt / clippy / test / cargo-deny）、`tracing` 設定。
- `kasu-core`: ID 型、BString ベースのパス/署名、`Entry`（File / Symlink / Gitlink）、ルート Changeset、
  `GitProvenance`、canonical encoding。
- 対象 Git クライアントのバージョン固定とパケットキャプチャ → **互換性マトリクス** 作成（04）。
- 完了条件: encoding のゴールデンテストと proptest（encode/decode 往復）。互換性マトリクスがフィクスチャ化されている。

### M1: ストレージと Derived Data（2〜3 週）
- `kasu-store`: 不変オブジェクトストア（put-if-absent）、`RefStore`（CAS + ref_log 原子追記、能力モデル）。SQLite・ローカル FS 実装。
- `kasu-derived`: SubKey・型間依存・親選択を持つフレームワーク、反復スケジューラ。
  `RootTree`, `Generation`, `FirstParentSkip`。
- 完了条件: 10 万 Changeset（マージ含む）の合成履歴で、Changeset 作成コストが履歴長に依存しないことをベンチで確認。
  `IsAncestor` が第二親経由の祖先を正しく判定する proptest。

### M2: Git の正確な取り込み（2 週）
- `kasu-ingest`: `gix` で既存リポジトリをトポロジカル順に Changeset 化。raw commit / 非正規 tree / tag を保存。
- `GitTree` / `GitCommit` Derived Data、リポジトリ単位 OID 索引、Git ref 射影。
- 完了条件: 非 UTF-8 パス・submodule・annotated tag・マージを含む OSS を取り込み、全 commit/tree/tag OID が一致。
  再現不能な入力は明示的エラー。

### M3: Metadata API（2 週）
- `kasu-server`（gRPC + HTTP/JSON）: Changeset / ref / log / diff / merge-base / ancestry / ReadFile。
- `PathHistory`, `Blame`（Lazy, SubKey=パス）。
- 完了条件: `git log -- <path>` / `git blame` / `git merge-base --all` と結果一致する差分テスト。

### M4: Git 読み取り G1（2 週）★ GitOps 最小価値
- Smart HTTP、v0/v1 ref 広告、v2 `ls-refs`、全履歴 clone、depth=1 fetch、非デルタ pack ストリーム、トークン認証。
- 完了条件: `git ls-remote`/`clone --depth 1`、go-git の shallow clone、**ArgoCD と Flux が kind 上で同期成功**。
  生成 pack が `index-pack --strict` / `fsck` を通過。

### M5: Git 読み取り G2（2 週）
- have ネゴシエーション（v0/v1 ACK 状態機械、v2 fetch）、増分 fetch、deepen/unshallow、SHA 指定 want、`include-tag`。
- 代表的 GitOps リポジトリでの帯域計測 → 必要ならデルタ導入を判断。
- 完了条件: 増分 fetch を連続 N 回行っても `fsck` が通る（マージ・複数 want を含むシナリオ）。

### M6: ネイティブ書き込み（2〜3 週）
- Write API: UploadBlobs / CreateChangeset / Rebase / Merge / UpdateRef / Land（冪等キー・上限・バックオフ）。
- `kasu-ops`: 3-way merge と 3-way rebase。
- 完了条件: シミュレーションで並行 1,000 クライアントが Land しても lost update なし・二重着地なし・
  全要求が有限時間で `Landed/Conflict/Retry` のいずれかを返す。

### M7: Git 書き込み G3 と Webhook（2 週）
- receive-pack（`atomic` なし）、接続性チェック、最終 tree 差分による ACL、ref パターン単位の force push 許可。
- ref_log tail による GitHub 互換 webhook。
- 完了条件: Argo CD Image Updater（git write-back）/ Flux Image Automation の E2E 成功。webhook で ArgoCD が即時リフレッシュ。

### M8: ネイティブクライアントとオブジェクトプロトコル（3 週）
- `GetTrees`/`GetBlobs`、`kasu` CLI（checkout/status/diff/commit/rebase/land/sparse）。
- 完了条件: スパースチェックアウトで要求オブジェクトがプロファイル内に限定されることを検証。

### M9: キャッシュ階層（2 週）
- `kasu-daemon`（Unix socket、ディスクストア、スナップショット起動）、`kasu-cache`（メモリ、singleflight、上位レート制限）。
- 完了条件: 負荷試験で各階層ヒット率・上位 fan-in 制限が機能。cache 障害時に server が過負荷にならない。

### M10: スケールアウトと本番ストレージ（3 週〜）
- PostgreSQL / S3 実装 → 分散 KV（FoundationDB or TiKV）評価。複数 ref トランザクション対応時に G4（`atomic`）。
- 複数 server インスタンスでの整合性テスト。

### M11 以降: 拡張
- G5（partial clone）、G6（パス射影リポジトリ、SSH、LFS）、ディレクトリ ACL、仮想化チェックアウト（FUSE）、CopyDetect / PathIndex。
- 着地後テスト / デプロイ前テストのための仕組み（Metadata API 上の自動 bisect / revert bot のリファレンス実装）。
- M12 完了後のマルチリポジトリ拡張: `ForkRepository`、共有オブジェクトのオフライン mark-and-sweep 回収、リポジトリ単位の容量クォータ。

### M12: マルチリポジトリ（3〜4 週）
詳細は 06 を参照。既存の単一リポジトリ運用を壊さない順序で段階的に入れる。

1. **レジストリと移行**: `RepositoryStore`（SQLite / PostgreSQL）、名前規則、`RepoKey`、既存リポジトリを legacy キーで登録する
   `migrate-registry` と互換起動モード。
2. **要求ごとの解決**: `Backend` から固定 `repository` を除き `RepoContext` を導入。Metadata / Writer / Objects / Git HTTP / SSH の
   全経路でレジストリ解決 → 所属判定。不可視リポジトリ・未所属 ID を `NotFound` に統一。`UploadBlobs` / thin pack の存在オラクル対策。
3. **リポジトリ単位設定と認可**: `KASU_REPOSITORIES_FILE`、アカウント grant（glob・最具体一致）、射影アカウントの
   `(repository, path)` 化、WritePolicy / Webhook / 上限のリポジトリ単位化と全体設定との積。旧形式設定の読み替え。
4. **Admin API とライフサイクル**: Create / Get / List / Update / Rename（alias）/ Delete（2 段階削除）、`kasu repo`、`kasu clone`、
   `kasu-ingest --create`。
5. **cache / daemon / 運用**: 複数リポジトリ対応の cache / daemon（(repo, id) 所属ヒット）、リポジトリ単位の admission 公平性、
   メトリクスラベルの許可リスト、論理サイズ集計。

- 完了条件:
  - 既存単一リポジトリのデータを移行後、全 Git OID・Derived Data キー・webhook cursor・Land 冪等結果が不変。
  - 1 server 上の複数リポジトリ（共有 blob・フォーク相当の共有 Changeset を含む）で、全 RPC / Git fetch / push / Objects /
    cache 経由の取得が他リポジトリ専用オブジェクトを返さず、未所属と不存在が区別できないことのプロパティテスト。
  - 改名後の旧 URL clone、削除 → 同名再作成で旧データが見えないこと、削除中の Land / push が確定しないことのシミュレーション。
  - ArgoCD / Flux が同一 server の別リポジトリをそれぞれ同期し、Webhook がリポジトリごとに正しく届く kind E2E。
  - 1 リポジトリへの過負荷中も他リポジトリの応答が上限内であることの負荷試験。

> M4 を早期に置くのは、GitOps 互換が本プロジェクト固有の価値であり、ネイティブクライアント（M8）なしでも
> 「Git から取り込んで GitOps に配信する」ユースケースで単独利用可能になるため。

## テスト戦略

| 種別 | 内容 |
|---|---|
| 単体 / プロパティ | encoding 往復、tree apply/diff 往復（`apply(a, diff(a,b)) == b`）、merge 不変条件（両側が merge-base と同一なら結果も同一・競合なし変更の保存・クリーン時の tree 妥当性）、ancestry（全親） |
| ゴールデン | Changeset ID、Git commit/tree OID の固定値 |
| 差分テスト | `git` 本体との結果比較（log, blame, merge-base --all, diff） |
| ファジング | pack パーサ、pkt-line パーサ、protobuf 入力（`cargo-fuzz`） |
| 決定的シミュレーション | `kasu-sim`: ストレージ/ネットワーク障害注入、並行 CAS / Land、キャッシュ階層障害、リポジトリ削除・改名と並行する書き込み |
| 隔離 | 複数リポジトリ間で所属外オブジェクト・不可視リポジトリが全経路（API / Git / Objects / cache）で漏れないこと |
| E2E | 互換性マトリクスの git CLI / go-git / ArgoCD / Flux / Image Updater / Image Automation（kind） |
| ベンチ | Changeset 作成・Derived Data 計算・pack 生成スループット・pack サイズ（`criterion`） |

## 主要リスクと対策

| リスク | 影響 | 対策 |
|---|---|---|
| 合成 Git commit のエンコード規則の変更 | 全 OID が変わり GitOps の追跡が壊れる | 規則をバージョン固定、ゴールデンテスト。公開済みリポジトリでは変更しない |
| Git オブジェクトのロスレス表現漏れ | 往復 OID 不一致・fsck 失敗 | バイト列ベースの型、raw オブジェクト保存、取り込み時検証で明示エラー |
| pack 列挙の誤り | 増分 fetch で欠落オブジェクト | 到達可能性の差集合を正とし最適化はその下に置く。全テストで `fsck` |
| 非デルタ pack の帯域 | 増分 fetch・巨大 repo で転送量増大 | M5 で計測し判断。オブジェクト単位圧縮キャッシュ、同一パスデルタ、パス射影リポジトリ |
| Land の競合多発 | 着地遅延・リトライ嵐 | 上限・バックオフ・冪等キー、競合率メトリクス、着地後テスト前提の運用 |
| ディレクトリ ACL と Git OID の両立不可 | Git 経由で権限制御が粗くなる | Git ではリポジトリ単位 + 射影リポジトリ、書き込みは最終 tree 差分で検査 |
| 複数 ref トランザクション非対応バックエンド | `atomic` push 不可 | RefStore 能力モデルで広告を切り替え |
| Git クライアントの多様な capability / バージョン差 | 想定外のネゴシエーション | バージョン付き互換性マトリクス、完全実装まで capability を広告しない |
| SHA-1 依存 | 将来の SHA-256 移行 | OID 型を抽象化し `object-format` を将来追加可能に |
| リポジトリ間の情報漏えい | 他リポジトリのオブジェクト・存在の露出 | 所属索引による可視性、未所属＝不存在の統一エラー、dedup 判定をリポジトリ所属で行う、隔離プロパティテスト |
| 名前ベースのキーによる改名・再作成の不整合 | 権限の取り違え・旧データの復活 | 不変・非再利用の `RepoKey`。既存データは legacy キーで書き換えずに移行 |
| noisy neighbor | 1 リポジトリの負荷が全体を劣化 | リポジトリ単位の admission 上限と公平配分 |
| 共有オブジェクトの回収 | 削除後もストレージが減らない | 初期は許容（メンテナンス不要を優先）。必要時にオフライン mark-and-sweep |
