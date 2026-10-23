# 04. Git 互換レイヤー

## 目的と対象クライアント

GitOps ツールが kasu のリポジトリを通常の Git リポジトリとして扱えるようにする。

| クライアント | 実装（要バージョン固定・要検証） | 主に使う操作 |
|---|---|---|
| ArgoCD repo-server | 現行版は `git` CLI 中心（`ls-remote` は protocol v2 指定）。旧版は go-git で ls-remote | `ls-remote`, リビジョン指定 fetch（SHA 指定含む）, depth 指定 fetch, `checkout` |
| Flux source-controller | go-git（protocol v0/v1） | shallow clone（depth=1）、sparse checkout（クライアント側）、任意で submodule |
| Argo CD Image Updater | `git` CLI | depth=1 fetch → commit → push |
| Flux Image Automation | go-git | clone → commit → push（push 先ブランチが異なる場合 **force push** し得る） |
| 人間・汎用 CI | `git` CLI | clone, fetch, partial clone |

> 上記は実装前に、対象バージョンを固定してパケットキャプチャ（`GIT_TRACE_PACKET=1` / go-git のトレース）で確認し、
> **バージョン付き互換性マトリクス** として `kasu-git` のテストフィクスチャに保存する。広告する capability はこのマトリクスで
> 必要と確認されたものに限定する。

## 対応範囲（段階）

| 段階 | 内容 |
|---|---|
| G1 | Smart HTTP、`ls-remote`（v0/v1 広告 + v2 `ls-refs`）、全履歴 clone、depth=1 fetch |
| G2 | 増分 fetch（have ネゴシエーション）、`deepen` / unshallow、SHA 指定 want、tag（`include-tag`） |
| G3 | receive-pack（`atomic` なし）、ref ポリシー、Webhook |
| G4 | `atomic` push（RefStore が複数 ref トランザクション対応時のみ） |
| G5 | `filter`（partial clone）— 実クライアントで必要と確認された場合のみ。promisor 側の後続 fetch まで含めて実装 |
| G6 | パス射影リポジトリ、SSH、Git LFS（検討） |

### トランスポートと認証
- Smart HTTP（`GET /info/refs?service=...`, `POST /git-upload-pack`, `POST /git-receive-pack`）。
- `Git-Protocol: version=2` ヘッダを解釈し、v2 と v0/v1 で **別々の capability 表・状態機械** を実装する。
  - v0/v1: ref 広告、`multi_ack_detailed` / `no-done` を含む ACK 状態機械、`side-band-64k`, `ofs-delta`, `shallow`, `include-tag`,
    `allow-tip-sha1-in-want` / `allow-reachable-sha1-in-want`（認可スコープ内）。
  - v2: `ls-refs`（`peel`, `symrefs`, `ref-prefix`）, `fetch=shallow`（後で `filter`）、パケットは常に sideband。
- 完全なセマンティクスを実装するまで capability を広告しない。
- 認証: HTTP Basic（ユーザー名任意 + トークン）および Bearer。

## オブジェクトマッピング

### 往復一致の方針
Git から取り込んだ/push されたオブジェクトは **バイト単位で再現** することを要件とする。
正規化で再現できない場合に備え、raw オブジェクトを content-addressed なサイドオブジェクトとして保存し、Git OID から参照できる索引を持つ。

| Git オブジェクト | 生成方法 | 往復一致の担保 |
|---|---|---|
| blob | 内容そのまま | 常に一致 |
| tree | ネイティブ Tree から合成 | 合成結果の OID が元と一致しない場合（非標準 mode・非正規順序等）は raw tree を保存し、そのノードは raw を返す |
| commit | Git 由来: `git_provenance.raw_commit` を返す / ネイティブ: 決定的に合成 | raw の `tree` 行が指す tree OID が実際に提供可能であることを取り込み時に検証 |
| tag | raw tag オブジェクトを保存 | 常に raw |

取り込み時に再現不能なリポジトリ（検証失敗）は **明示的にエラー** とし、黙って不整合な状態を作らない。

### Tree（`GitTree`）
- ネイティブ `RootTree` の各ノードに対し Git tree OID を増分計算（変更パス上のノードのみ）。
- Git のソート規則（ディレクトリ名は末尾 `/` 付きとして比較）、mode（`100644`, `100755`, `120000`, `040000`, `160000`）。
- Gitlink（`160000`）はコミット OID を指すエントリとして出力。submodule の **中身の追跡** は非ゴールだが、エントリの表現は必須。

### Commit（`GitCommit`）
ネイティブ Changeset からの合成規則:

```
tree <GitTree(cs).root>
parent <GitCommit(p).oid>   (for p in parents)
author <name> <email> <ts> <tz>
committer <name> <email> <ts> <tz>
kasu-changeset <ChangesetId>          # 追跡用ヘッダ（任意・規則に含めるなら固定）

<message>
```

- Changeset の内容のみから一意に定まり、サーバー状態・時刻・設定に依存しない。
- 合成規則にはバージョンを持たせるが、**一度公開したリポジトリでは変更しない**（全 OID が変わるため）。
- マージ Changeset: 第二親以降は `parent` 行として出力するだけでよく、tree は `RootTree` から決まる。

### OID 索引とスコープ
- `(repo, git_oid) -> (kind, native ref)` を **リポジトリ単位** で持つ。グローバル索引を直接 want 解決に使わない（他リポジトリのオブジェクト露出防止）。
- 射影リポジトリは別 OID 空間（SubKey = ProjectionId）。

### Git ref 射影
ネイティブ ref（Changeset のみを指す）とは別に、Git 公開用の ref 表現を持つ。

```
git_refs: (repo, name) -> target: GitOid, peeled: Option<GitOid>, kind
```

- `refs/heads/*` はネイティブ ref から自動導出（`target = GitCommit(cs)`）。
- `refs/tags/*` は commit / annotated tag（tag→tag の連鎖含む）/ tree / blob を指せる。v2 `ls-refs peel` と v0 の `^{}` 広告に対応。
- `HEAD` は `symref=HEAD:refs/heads/main`。未生成ブランチ（unborn）も表現可能にする。
- ref なしの Changeset を SHA 指定で fetch したい場合（ArgoCD の SHA 指定同期など）は、**認可された ref から到達可能** な commit、
  またはレビューシステム等が明示的に公開した Changeset に限り want を許可する。

## Pack 生成（upload-pack）

ネイティブストアに packfile は存在しない。pack は要求ごとにストリーム生成する **派生成果物** であり、キャッシュは有界・使い捨て。

### 意味論（正）
```
送信オブジェクト = reachable(wants, shallow/deepen 境界, filter)
                 − クライアントが確実に保有するオブジェクト（common commits から到達可能なもの。既存 shallow 境界を考慮）
```
複数 want、複数の共通祖先、マージの第二親側の履歴、中間 commit のみが参照する tree/blob、`include-tag` の tag オブジェクトをすべて含む。

### 実装（最適化）
1. **ネゴシエーション**: `have` をリポジトリ内 OID 索引で解決し、Generation 枝刈り付きの全親探索で共通集合を決定。
2. **commit 列挙**: want から common 集合・shallow 境界まで全親方向に辿る（Generation で早期打ち切り）。
3. **tree/blob 列挙**: 列挙した各 commit について、その第一親の tree との差分（= Changeset の `changes` とそれに伴う
   tree ノード）を候補とし、common 側から到達可能な tree ノードは GitTree の共有構造でスキップ。
   マージ commit は「第一親との差分」だけでは不十分なので、第二親側から来た tree/blob が common 側に無い場合を含め
   tree 同士の差分で補完する。
4. **depth=1 / clone**: 対象 commit の tree 全体を送る（GitOps の主要ケース）。
5. **検証**: テストでは生成 pack を `git index-pack --strict` / `git fsck` で必ず検証する。

### エンコードとキャッシュ
- 初期は非デルタ（zlib のみ）でストリーム生成。Git オブジェクト単位の **zlib 圧縮済みバイト列をキャッシュ** して再圧縮を避ける。
- 代表的な GitOps リポジトリで帯域を計測し、許容できない場合は同一パスの旧 blob を基底とした ofs-delta を導入。
- pack 全体のキャッシュは「ref 先頭の depth=1 pack」など高ヒット率が見込めるキーに限定し、容量上限付き LRU とする。

### モノレポ規模への配慮: パス射影リポジトリ（G6）
GitOps コントローラーは通常リポジトリ全体を取得するため、巨大モノレポでは depth=1 でも重い。

- `https://host/<repo>/-/path/<dir>.git` のような URL で、指定ディレクトリを root とする仮想 Git リポジトリを公開。
- tree: `RootTree` の該当サブツリー。commit: 該当パスを変更した Changeset のみから合成した決定的なコミット列
  （`git subtree split` 相当）。親は「該当パスに影響した直近の祖先」で、マージの扱いは規則として固定する。
- 射影ごとに別 OID 空間（Derived Data の SubKey = ProjectionId）。
- ArgoCD の `path` 設定と組み合わせ、マニフェスト用ディレクトリだけを配信できる。ディレクトリ ACL の Git 側の受け皿にもなる。

## Push 受信（receive-pack）

1. pack を受信・検証（`gix` でパース、オブジェクト数・サイズ上限、zlib bomb 対策）。**接続性チェック**（新 ref 先頭から
   既存オブジェクトまで欠落がないこと）と型チェックを ref 更新前に完了する。
2. 新規 commit を **トポロジカル順（親が先）** に処理: 第一親の tree と自身の tree を diff → `FileChange` 列に変換し、
   raw commit / 必要なら raw tree を保存して Changeset を作成（blob は BlobStore へ）。
3. ポリシー検査:
   - 許可 ref（例: `refs/heads/main`, `refs/heads/image-updates/*`）。
   - **ディレクトリ ACL は `旧 ref の tree → 新 ref の tree` の最終差分で判定** する（マージの第二親経由で
     未許可の変更が入るのを防ぐため、各 commit の第一親差分だけで判定しない）。
   - non-fast-forward（force push）はデフォルト禁止、ref パターン単位で許可可能（Flux Image Automation の
     別ブランチ push 用）。
4. ref 更新: 各 ref を `UpdateRef` CAS（`old-oid` を expected に）。`atomic` は RefStore が複数 ref トランザクションに
   対応する場合のみ広告・実行（G4）。
5. 結果を `report-status` で返す。CAS 失敗は `fetch first` として拒否（クライアントのリトライ前提）。

## 認可とディレクトリ ACL の制約

- Git プロトコルではサブツリーを隠すと tree OID が変わり、整合した履歴を提供できない。
- 方針: 通常の Git 公開リポジトリは **リポジトリ単位** の読み取り権限で判定。
  ディレクトリ単位で制限したい場合は、許可されたディレクトリの **パス射影リポジトリ** を公開する。
- 書き込みは最終 tree 差分に対してディレクトリ ACL を完全に適用できる。

## Webhook

- ref_log（02 参照、ref CAS と同一トランザクションで追記）を tail して、GitHub 互換 `push` イベント
  （`ref`, `before`, `after`, `commits[].added/modified/removed`）を送信。取りこぼしなし・少なくとも 1 回配信。
- ArgoCD はこれで `modified` パスに基づくリフレッシュ判定ができる。Flux には Receiver（generic / github 形式）向けに送信。

## 互換性テスト

- 互換性マトリクスの各クライアント・バージョンで E2E（ls-remote / clone / depth fetch / 増分 fetch / deepen / SHA 指定 fetch / push）。
- 生成 pack は必ず `git index-pack --strict` と `git fsck --strict` で検証。合成 commit OID のゴールデンテスト。
- kind 上の ArgoCD・Flux・Image Updater・Image Automation を使った同期/書き戻し E2E（CI の夜間ジョブ）。
- 往復テスト: 実在 OSS リポジトリ（非 UTF-8 パス、submodule、annotated tag、マージを含むものを選定）を取り込み →
  Git として clone → 全 commit/tree/tag OID が元と一致し `fsck` が通ること。
