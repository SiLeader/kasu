# 仮想化 checkout

[English](virtual-checkout.md) | 日本語

Linux の `kasu mount` は、読み取り用 FUSE と書き込み用 OverlayFS を重ねた作業コピーを起動します。
ファイルを読むまで blob を取得せず、変更した作業ファイルだけを上位ディレクトリへ実体化します。
既存の `status`／`diff`／`commit`／`rebase`／`land`／`sparse` は同じ `.kasu` の作業状態を使います。

## 起動

Linux の FUSE デバイス、`mount`／`umount`、OverlayFS のマウント権限（`CAP_SYS_ADMIN`）が必要です。
保存先は OverlayFS の upper として使用できるローカルファイルシステムを選びます。
Docker 自身の OverlayFS 上を upper にした起動はカーネルに拒否されました。
検証では独立した Docker volume と tmpfs の両方を使用しています。

```sh
export KASU_TOKEN=dev-token
kasu mount main /work/project --backing /work/project-state \
  --server http://127.0.0.1:50051 --repo demo --sparse app
```

プロセスは foreground で動きます。別のシェルで通常の操作を行います。

```sh
cat /work/project/app/file
printf 'changed\n' > /work/project/app/file
kasu -C /work/project status
kasu -C /work/project diff
kasu -C /work/project commit -m 'edit from virtual checkout'
kasu -C /work/project rebase --onto main
kasu -C /work/project land
```

`--objects unix:///path/to/node.sock` など、通常 CLI と同じ Objects endpoint も指定できます。
Metadata は権威サーバーへ、Tree／Blob は指定した Objects endpoint へ接続します。
mountpoint と backing は別のディレクトリにし、新規 backing と mountpoint は空にします。
制御用ディレクトリのパスは UTF-8 で、空白・改行・backslash・comma・colon を含めず、末端に symlink を指定できません。
リポジトリ内のパスは byte 列を保持し、非 UTF-8 を扱えます。

Ctrl-C または SIGTERM で、上位 OverlayFS、下位 FUSE の順に解除します。
開いたファイルなどで解除できない場合は FUSE を維持して待つので、ハンドルを閉じて再度シグナルを送ります。
強制終了後は同じ引数で再起動します。backing のロックを取得してから、自分の古いマウントだけを解除し、
保存済みの起点と upper を使って復旧します。再起動時に main の最新値へ自動的に切り替えません。

## 保存と遅延取得

| backing 内 | 内容 |
|---|---|
| `mount.json` | 初期 Changeset、ref、repo、endpoint、sparse profile、mountpoint。トークンは保存しない |
| `mount.lock` | mount プロセスの排他ロック |
| `lower/` | 読み取り用 FUSE の mountpoint |
| `upper/` | 変更した作業ファイル、whiteout、通常 CLI の `.kasu` |
| `work/` | OverlayFS の作業領域 |

初期化はプロファイル内の Tree だけを取得して file entry を列挙し、blob 本文は取得しません。
新しい `Metadata.GetBlobMetadata`／HTTP JSON `/v1/blob-metadata` は、repo 所属を検査した ID の
サイズと Git blob OID を最大128個ずつ返します。保存済み BlobMetadata を読み、blob 本文は読みません。
そのサイズを FUSE の属性に使うため、`stat`／`readdir` による blob の先読みはありません。

通常ファイルの read と symlink の readlink は Objects RPC から内容を取得します。
FUSE は検証済み blob を backing ディレクトリ内の一時ファイルにキャッシュします（最大512 MiB・1,024件、最も長く使われていないものから削除）。32 MiBを超える blob も、要求されたバイト範囲のみを読み出します。キャッシュはアンマウント時に破棄され、カーネルのページキャッシュも再利用します。
大きな blob は単一オブジェクトとして読み、最大サイズは既存の256 MiBです。
Objects の1回の取得には30秒の期限を設け、失敗・サイズ不一致はファイル I/O エラーを返します。
Gitlink は通常 checkout と同じ `.kasu-gitlink` を持つディレクトリとして表示します。

読み取りだけでは upper に作業ファイルを作りません。
write／chmod／rename などで必要になるコピー、削除の whiteout と新規ファイル作成は OverlayFS が扱います。
CLI の更新ジャーナルと ref CAS／Land は通常 checkout と同じ実装です。
元の blob が未取得のまま上書きした場合、テキスト diff の直前に必要な元 blob だけを取得します。

現状の native `status`／`commit` は内容照合のためプロファイル内のファイルを読みます。
この操作は blob を取得して `.kasu/objects` にキャッシュしますが、変更していない作業ファイルは upper にコピーしません。
FUSE の読み取りキャッシュと既存 daemon／共有 cache の容量制限は利用できます。
CLI の `.kasu/objects` 全体の容量管理、巨大プロファイルのメタデータ量、長時間のページキャッシュと障害評価は残作業です。

## 検証

固定 Rust 1.90／Linux arm64 の実カーネルで、次を確認しました。

- `stat`／`readdir` まで blob 要求0回。最初の通常ファイル read は1回、同じ read の繰り返しは追加要求0回。
- 非 UTF-8 の file、symlink、実行可能 mode、Gitlink、プロファイル外の除外。
- read 後にも upper の作業ファイルが存在せず、write 後だけ実体化すること。
- 追加・編集・削除、通常 CLI の status／diff／commit、上流更新を含む rebase／Land、ネイティブ内容一致。
- native の内容走査後も、変更していない作業ファイルが upper にコピーされないこと。
- SIGKILL 後の再マウントでファイル・whiteout・着地後の状態を保持すること。
- 使用中ファイルがある終了要求ではマウントを維持し、ハンドルを閉じた後の終了で両階層を解除すること。

macOS／Linux の通常 CLI 回帰テストと、Metadata の gRPC／HTTP JSON も通過しました。
[結果・ソース hash](simulations/virtual-checkout/summary.json) と各ログを保存しています。
CI の [virtual.yml](../.github/workflows/virtual.yml) は実マウント用の明示ジョブを持ちます。
GitHub 上での実行結果は未確認です。

```sh
docker build -f tools/virtual/Dockerfile -t kasu:virtual-test .
docker run --rm --network none --mount type=volume,destination=/tmp \
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  kasu:virtual-test
```

## macOS の調査

2026-10-24 に Apple の FSKit sample と、macOS 付属および Apple 公開ソースの `mount_nfs(8)` を確認しました。
macOS の `kasu mount` adapter はまだ実装していません。通常の materialized checkout は利用できます。

| 方式 | 確認した API／要件 | kasu 側で必要な作業 |
|---|---|---|
| FSKit | user space の file system extension。sample は `FSUnaryFileSystem`／`FSVolume` と `com.apple.developer.fskit.fsmodule` entitlement を使用 | Swift／Objective-C の extension と Rust の Objects／Metadata クライアント間の adapter、copy-on-write 保存、アプリとしての起動・有効化の実機検証 |
| NFS loopback | 標準 client の TCP、`port`／`mountport`、タイムアウト・再試行設定 | localhost の NFS server、安定した file handle／directory cookie、書き込み・flush とロックの実装、切断時の有限終了と mount／unmount の実機検証 |

FSKit の extension を薄い OS adapter として使う案を第一候補とします。NFS loopback は別の wire protocol と
ファイル操作のサーバーまで実装する必要があり、既定の server 障害時の待機動作も評価する必要があります。
これは調査に基づく設計判断で、どちらも kasu の macOS 実マウント成功を示すものではありません。

参照: [Apple FSKit sample](https://developer.apple.com/documentation/fskit/building-a-passthrough-file-system)、
[Apple の mount_nfs(8) ソース](https://github.com/apple-oss-distributions/NFS/blob/main/mount_nfs/mount_nfs.8)、
[fuser 0.18 Filesystem API](https://docs.rs/fuser/0.18.0/fuser/trait.Filesystem.html)。
