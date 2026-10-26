# 03. プロトコルと API

## API 面

gRPC（`kasu-proto`）を正とし、主要な読み取り系は HTTP/JSON でも公開する（CI スクリプトや bot 用）。

全 RPC は `repo`（リポジトリ名）を持ち、サーバーは要求ごとにレジストリで `RepoKey` に解決してから処理する。
権限のないリポジトリ・存在しないリポジトリ・当該リポジトリに所属しないオブジェクト ID は、運用者以外には区別なく
`NotFound` を返す（06）。`ReadOnly` / `Archived` のリポジトリへの書き込みは `FailedPrecondition`。

### Metadata API（チェックアウト不要の問い合わせ）

| RPC | 説明 |
|---|---|
| `GetChangeset(id)` | Changeset 本体 |
| `ResolveRef(name)` / `ListRefs(prefix)` | ref 解決 |
| `Log(start, path?, limit, cursor)` | 第一親ログ / パス履歴（PathHistory 使用） |
| `IsAncestor(a, b)` / `MergeBase(a, b)` | 全親探索 + Generation 枝刈り |
| `Diff(base, head, paths?)` | tree diff + 必要なら内容 diff |
| `Blame(cs, path)` | Blame Derived Data |
| `ReadFile(cs, path)` / `ListDir(cs, path)` | チェックアウトなしの読み取り |
| `FindChangesetByGitOid(oid)` | Git 互換の逆引き |

### Write API

| RPC | 説明 |
|---|---|
| `UploadBlobs(stream)` | blob アップロード（存在チェック付き、既存はスキップ） |
| `CreateChangeset(parents, changes, meta)` | Changeset 作成（ロックなし） |
| `Rebase(cs, onto)` / `Merge(a, b)` | サーバー側 3-way 演算。競合時は構造化された競合情報 |
| `UpdateRef(name, expected, new)` | CAS。fast-forward 制約・ポリシー検査付き |
| `Land(cs, ref, idempotency_key)` | rebase + CAS を上限・バックオフ付きでリトライ。結果は `Landed / Conflict / Retry / Denied` |

チェックアウトなしの書き込み（bot がファイル 1 つを書き換える等）は `UploadBlobs` + `CreateChangeset` + `Land` で完結する。

`UploadBlobs` の「既存スキップ」判定は当該リポジトリの所属で行い、他リポジトリにだけある blob は本文の送信を求める。

### Admin API

| RPC | 説明 |
|---|---|
| `CreateRepository(name, default_branch, config)` | リポジトリ作成（unborn HEAD）。冪等キー対応 |
| `GetRepository(name)` / `ListRepositories(prefix, cursor)` | 呼び出し元に可視なリポジトリのみ |
| `UpdateRepository(name, expected_config_version, patch)` | 設定・状態（Active / ReadOnly / Archived）の CAS 更新 |
| `RenameRepository(name, new_name, keep_alias)` | 改名（`RepoKey` 不変、旧名 alias） |
| `DeleteRepository(name)` | `Deleting` に遷移し非同期に削除。名前は即時解放 |
| `ForkRepository(source, ref, new_name)` | 所属のみコピーするフォーク（後半） |

## Object 取得プロトコル（全階層共通）

```
GetTrees(ids: [TreeId]) -> stream TreeObject
GetBlobs(ids: [BlobId]) -> stream BlobChunk
```

- Git の have/want ネゴシエーションは行わない。クライアントは **欲しいもの** だけを ID 指定で要求する。
- 手順: `ResolveRef` → `RootTree` 取得 → ローカルストアにない子 tree を再帰要求 → 必要な blob を要求。
- スパースチェックアウト: スパースプロファイル外のサブツリーは要求しない（特別なサーバーサポート不要）。
- 各リクエストは不変オブジェクトのみを対象とするため、任意階層でキャッシュ可能・中断再開可能。
  ただし可視性はリポジトリの所属で決まるため、cache / daemon は (repo, id) の所属を上位で確認済みの場合だけヒットとする。
- ワイヤ形式＝daemon のディスク格納形式（zstd 圧縮済みバイト列）とし、キャッシュは変換なしでストリームする。
- バッチサイズ・並列度はクライアント側で制御。大きな blob はチャンクストリーム。

## クライアント (`kasu`)

### Working copy
- `.kasu/` に: サーバー URL とリポジトリ名、現在の基底 Changeset ID、スパースプロファイル、ファイル状態（mtime/size/inode → BlobId）。
- 変更検出: FS 監視（`notify`）+ 起動時のステート比較。macOS は FSEvents、Linux は inotify / fanotify。
- 状態は「基底 Changeset + ローカル変更」のみ。ローカル履歴は持たない。

### 主要コマンド（案）
```
kasu clone <server> <repo> [--sparse <profile>]
kasu checkout <ref|cs> [--sparse <profile>]
kasu status / diff
kasu commit -m ...          # サーバー上に Changeset を作成（push 不要）
kasu rebase --onto main
kasu land                   # main へ着地
kasu log / blame / show     # 全てサーバー問い合わせ
kasu sparse add|remove <dir>
kasu repo list|create|show  # Admin API
```

### 仮想化チェックアウト（後半フェーズ）
- Linux: FUSE（`fuser`）。macOS: FSKit / NFS ループバック等を調査。
- ファイル読み取り時に blob を取得、書き込まれたファイルのみ実体化。
