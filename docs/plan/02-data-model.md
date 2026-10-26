# 02. データモデル

## 基本方針

- ネイティブの content address は **BLAKE3**（`ObjectId = [u8; 32]`）。Git 互換用の SHA-1 OID は別の索引/Derived Data で併記（04 参照）。
- 全ての不変オブジェクトは「ハッシュ → バイト列」の put-if-absent で保存し、上書きしない。
- 不変オブジェクトは **全リポジトリで共有** する。リポジトリから見える範囲は「所属索引」で決まる（後述「リポジトリと所属」）。
- **パス・人名・メールアドレス・メッセージはすべてバイト列**（`BString` 相当）として扱う。Git は非 UTF-8 を許容するため、
  `String` を使うと往復一致が壊れる。表示時にのみ lossy 変換する。

## リポジトリと所属

詳細は 06 を参照。

```
RepoKey  = 不変の内部キー（新規: "r1\0" + 16 バイト乱数 / 既存移行: 旧リポジトリ名バイト列）。再利用しない
RepoName = 外部名（`team-a/infra` 等）。改名可能。repo_names で RepoKey に解決
所属索引 = repo/<kind>/v1/<RepoKey>/<id> -> ()      # kind = changeset / tree / blob
```

- 永続キー・Derived Data の SubKey（`ProjectionId` 等）・読み取り scope 名前空間は全て `RepoKey` を使い、名前は使わない。
- Changeset / Tree / Blob の作成時に、作成先リポジトリへの所属を put-if-absent で追加する。読み取り・親参照・
  merge / rebase の入力は当該リポジトリの所属オブジェクトに限る。
- 共有ストアに存在しても所属のないオブジェクトは「存在しない」と同じに扱う（存在オラクル防止。06）。

## Blob

```
BlobId   = blake3(raw content)
保存形式 = zstd 圧縮（BlobStore のキー: blob/<BlobId>）
付随情報 = size, git_blob_oid = sha1("blob {len}\0" + content)   # 作成時に計算して索引
```

大きなファイルはチャンク分割（固定長 or CDC）を将来検討。初期は単一オブジェクト。

## Changeset（作業単位）

Scion 同様、Changeset は「第一親に対する変更エントリのリスト」を持つ不変レコード。tree へのポインタは持たない。

```rust
struct Changeset {
    parents: Vec<ChangesetId>,          // [0] が第一親。空 = ルート Changeset。マージは 2 以上
    author: Signature,                  // name/email: BString, timestamp: i64, tz: 生バイト列
    committer: Signature,
    message: BString,
    changes: Vec<FileChange>,           // パス順ソート・パス重複なし、第一親に対する差分
    git_provenance: Option<GitProvenance>, // Git 由来の場合のみ（後述）
}

enum FileChange {
    Set    { path: RepoPath, entry: Entry },  // 追加/変更を区別しない（rebase 時に意味が変わるため）
    Delete { path: RepoPath },
}

enum Entry {
    File    { blob: BlobId, executable: bool },
    Symlink { blob: BlobId },
    Gitlink { commit: GitOid },        // submodule。内容は追跡しないが、往復一致のため表現は必須
}

struct GitProvenance {
    raw_commit: ObjectId,  // 元の raw commit バイト列（content-addressed なサイドオブジェクト）
}

ChangesetId = blake3(canonical_encode(Changeset))
```

- 作成はサーバーのみ。`put-if-absent` なので **ロック・協調不要**。同一内容の再送は冪等。
- 検証: 親の存在、`changes` のパス重複なし、Delete 対象が第一親の tree に存在、ディレクトリ/ファイルの衝突なし。
- ref に依存しない。Changeset は作成された瞬間から全サーバーで ID 指定で取得可能。
- `git_provenance` は ID 計算に含まれる。ネイティブの rebase / merge で作られた Changeset には **コピーしない**
  （新しい Changeset は新しい合成 Git commit を持つ）。

## Tree（Derived Data）

Changeset は tree を持たないため、ルート tree は Derived Data `RootTree` として増分計算する。

```
RootTree(cs) = apply(EmptyTree,                  cs.changes)   if cs.parents is empty
             = apply(RootTree(cs.parents[0]),     cs.changes)   otherwise
```

- マージ Changeset も第一親に対する差分のみで tree が決まる（第二親以降は tree 計算に不要）。
- tree は永続（persistent）Merkle 構造。変更パス上のノードのみ再生成し、それ以外は親と共有 → コストは変更量に比例。
- ネイティブ Tree ノード: `TreeId = blake3(sorted entries)`, entry = (name: BString, Entry | SubTree(TreeId))。
- クライアントのチェックアウトはこの Tree をルートから再帰取得する（03 参照）。

## Ref

ネイティブ ref はシンプルに保ち、Git 固有の ref 表現（tag オブジェクト・非 commit ターゲット）は 04 の **Git ref 射影** で扱う。

```
refs テーブル: (repo: RepoKey, name) -> (target: ChangesetId, version: u64)
ref_log    : (repo: RepoKey, seq) -> (name, old, new, actor, time)   # 同一トランザクションで追記（Webhook のアウトボックス兼用）
```

- 更新は CAS（`expected` 一致時のみ）。Changeset 作成とは独立。
- ref の作成は原則 `main` とリリースブランチに限定（ポリシーで制御）。作業中の変更は ref なしの Changeset として存在し、
  レビューシステム等が ID で参照する。

### RefStore の能力モデル

| 能力 | 必須 | 用途 |
|---|---|---|
| 単一 ref CAS + ref_log 追記の原子性 | ○ | 通常の ref 更新、Webhook 配信の取りこぼし防止 |
| 複数 ref のトランザクション CAS | 任意 | Git `atomic` push。非対応バックエンドでは `atomic` を広告しない。対象は同一リポジトリ内の ref に限る |
| 変更フィード（ref_log の tail） | ○ | Webhook / Eager Derived Data のトリガ |

ref_log 追記と CAS を同一トランザクションにするため、**ref 系テーブルだけは** 単一パーティション内のトランザクションを要求する。
不変オブジェクト（Changeset / Blob / Derived）は put-if-absent のみで、トランザクションに依存しない。

## Derived Data フレームワーク

Derived Data は「キー空間」「依存」「どの親を辿るか」を型ごとに宣言する。

```rust
trait DerivedData: Sized + Encode + Decode {
    const NAME: &'static str;
    const VERSION: u32;
    const MODE: DeriveMode;                 // Eager / Lazy
    type SubKey: Encode;                    // () / RepoPath / ProjectionId など
    const PARENTS: ParentSelection;         // None / FirstParent / AllParents

    /// 他型への依存（例: GitCommit は GitTree と RootTree に依存）
    fn dependencies(ctx: &Ctx, cs: &Changeset, key: &Self::SubKey) -> Vec<DerivedRef>;
    fn derive(ctx: &Ctx, cs: &Changeset, key: &Self::SubKey, parents: &[Self]) -> Result<Self>;
}
```

- 保存キー: `derived/<NAME>@<VERSION>/<ChangesetId>/<SubKey>`（不変・冪等）。Changeset 内容のみで決まる型は
  リポジトリ間で共有し、リポジトリ固有の型（Git OID 等）は SubKey に `RepoKey` を含める。
- **SubKey** によって `Blame(cs, path)` や射影ごとの Git OID のような部分キーを持てる。Lazy 型は要求されたキーのみ計算し、
  リポジトリ全体を実体化しない。
- 計算スケジューラは依存グラフ（型間 + 親方向）を辿り、未計算の境界から **古い方へ向かって反復的に** 計算（再帰しない）。
- 決定的な計算なので並行計算の衝突は無害（put-if-absent）。重複計算は singleflight で抑制。
- バージョンを上げると新キー空間でバックフィルし、旧版と並行運用できる。

### 初期実装する Derived Data

| 名前 | 親選択 | SubKey | モード | 用途 |
|---|---|---|---|---|
| `RootTree` | FirstParent | () | Eager | チェックアウト、diff、Git tree の基礎 |
| `Generation` | AllParents | () | Eager | 世代番号（全親の最大 +1）。ancestry 探索の枝刈り |
| `FirstParentSkip` | FirstParent | () | Eager | 第一親チェーンの skip pointer。第一親 log の高速化のみ |
| `GitTree` / `GitCommit` | FirstParent / AllParents | ProjectionId | Eager（Git 公開 repo） | Git OID 計算（04） |
| `PathHistory` | FirstParent | RepoPath | Lazy | パスを直前に変更した Changeset へのリンク |
| `Blame` | AllParents | RepoPath | Lazy | 行→Changeset 対応。親の Blame + diff から増分計算 |
| `CopyDetect` / `PathIndex` | — | — | Lazy（後半） | rename/copy 検出、パス検索 |

### Ancestry / merge-base

- `IsAncestor` / `MergeBase` は **全親を辿る** 探索とし、`Generation` で枝刈りする（Git の commit-graph 世代番号と同様）。
  第一親 skip list だけでは第二親経由の祖先を見落とすため、これ単独では使わない。
- 大規模化したら到達可能性ビットマップ / 区間ラベリング等の索引を Derived Data として追加する。

## Merge / Rebase（サーバー側）

- **3-way merge**: merge-base（上記）と両側の tree diff から、パス単位で自明な変更を適用。
  同一ファイルの両側変更は行単位 diff3。競合時は構造化された競合情報を返し、Changeset は作らない。
- **rebase**: `RootTree(旧親)` / `RootTree(cs)` / `RootTree(新親)` の **3-way merge** として計算し、
  新親に対する差分を `changes` として再計算した新 Changeset を作る。単純に `changes` を再適用するのではない。
- **Land**（グローバルロックなしの merge queue）:
  1. rebase（または merge）→ ポリシー/ACL 検査 → ref CAS。
  2. CAS 失敗時は再 rebase してリトライ。**上限回数・指数バックオフ・キャンセル** を持ち、超過時は `Retry` を返す。
  3. 冪等キー（クライアント指定）で同一 Land 要求の二重着地を防ぐ。
  4. リトライのたびにポリシー/ACL を再検査する。
- Scion 同様、着地前のグローバル直列テストは前提にしない。**着地後テスト + デプロイ前テスト** と、自動 revert/bisect を
  Metadata API 上に構築する（05 参照）。
