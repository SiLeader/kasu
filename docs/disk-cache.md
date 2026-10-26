# daemon のディスク容量管理

`kasu-daemon` のディスク Objects cache は、保存済み zstd frame の容量と型付き索引数に上限を持ちます。
上限を超える新規保存では LRU の索引を追い出し、最後の索引がなくなった frame を削除します。
実行中の配信が既に取得した `Arc<Vec<u8>>` はそのまま使えるので、eviction で返却中の内容は変わりません。

```sh
kasu-daemon serve --repo my-repo --upstream http://127.0.0.1:50091 \
  --token-file /run/kasu/user-token --upstream-token-file /run/kasu/daemon-hop \
  --data /var/cache/kasu/node --socket /run/kasu/node.sock \
  --disk-bytes 8589934592 --disk-entries 100000 --snapshot-keep 2
```

| オプション | 既定値 | 意味 |
|---|---:|---|
| `--disk-bytes` | 8 GiB | キャッシュする zstd frame の合計 bytes。1–2^50 |
| `--disk-entries` | 100,000 | Tree／Blob の型付き索引の合計数。1–1,000,000 |
| `--snapshot-keep` | 2 | 管理対象の定期 snapshot の保持数。1–1000 |
| `--snapshot-interval-secs` | 3600 | 定期保存間隔。0 で停止、最大1年 |
| `--snapshot-max-bytes` | 8 GiB | 起動時 import の frame 合計上限。キャッシュ上限とは独立 |

bytes は保存 payload の論理サイズです。filesystem の block 単位の割当量、SQLite・索引・lock、
import の staging、snapshot archive、利用者が置いたファイルは含みません。
Tree と Blob が同一 ID の frame を使う場合、bytes は一度だけ数え、entries はそれぞれ数えます。
SQLite には型付き索引と LRU を保持し、既存の圧縮 frame の表現は置き換えません。
同じ内容でも別の zstd 表現を受け取った場合は最初の frame を使います。

単体 frame が byte 上限より大きいときは、保存せずに取得した frame を配信します。
既存キャッシュは追い出しません。次回の要求では通常の cache 階層を通って再取得します。
eviction 後の miss も daemon → cache → authority の既存の singleflight・pacing・同時数・回路遮断を使い、
権威サーバーへ直接迂回しません。

## 保存障害時の再取得制御

Store の保存に失敗した cold miss はエラーを返します。上位から取得できても保存できなかった場合、
fill の回路遮断器では失敗として数えます。既定で3回連続すると cooldown に入り、
新しい cold miss は上位へ取得を繰り返さず `Unavailable` で終了します。
すでに取得中の要求は fills の上限まで残り得ます。既存 frame の hit はこの回路を通りません。
cooldown 後の1要求だけが probe となり、取得と保存の両方が成功してから遮断を解除します。
保存に失敗し続ける probe は再び cooldown に入ります。
設定上限より大きい frame を意図的に保存しない正常経路は、これまでどおり成功です。

[回帰検証の記録](simulations/cache-write-failures/summary.json) は、修正前の再現、macOS の階層6テスト、
固定 Rust 1.90 Linux の cache／daemon／CLI 43テストを含みます。
Store::put のエラーを注入して、3回の失敗後の cold miss 30件が権威取得を増やさず、
warm hit、失敗する probe、復旧後の保存と次回 hit を検査します。
実 filesystem の ENOSPC／電源断を発生させた試験ではありません。

## 永続 LRU と複数インスタンス

`scope.json` と `index/<kind>/<id>` の8-byte size marker、`objects/<先頭2桁>/<残り62桁>` の形式は維持します。
新たに `cache-lru-v1.sqlite3` と `.cache-lru.lock` を使います。
成功した get／put は永続的な順序番号を進めます。snapshot export は LRU を更新しません。
同じ frame の Tree／Blob はアクセス順序を共有し、同順序では ID／kind の順で eviction を決めます。
順序番号の上限では現在の相対順序を保って振り直します。

公開 API の `DiskStore::open_with_limits` は上限を設定し、既存データにも適用します。
`DiskStore::open` は保存済みの設定を使い、新しい cache の場合だけ既定値を使います。
手動 snapshot コマンドが稼働中の daemon の設定を既定値へ戻すことはありません。
daemon の serve は起動オプションを設定として保存するため、再起動時も同じ上限を指定してください。

各インスタンスは SQLite WAL／FULL synchronous と共通の file lock に参加します。
ファイルの公開・削除は排他的に行い、読み取り・export は共有 lock で eviction から保護します。
同じ daemon の二重起動は従来の daemon／socket lease が拒否しますが、稼働中の手動 export は可能です。
プロセス間 file lock は5秒、SQLite の busy 待ちは2秒で終了します。
同一インスタンスのディスク処理は直列化するため、大きな snapshot の間は通常の cache fill が待機します。
待機中もメトリクスはディスクの lock を取得しません。

データ directory は専用の空 directory、または既存の有効な cache scope がある directory を指定します。
新規 cache は無関係な既存ファイルのある directory を採用せず、`meta.db` のある権威ストアも拒否します。
既存 cache の初回起動では marker を移行し、現在の上限へ縮小する場合は LRU eviction を行います。
この directory は信頼した所有者だけが管理し、旧バージョンや別プログラムと同時に書き込まないでください。
メタデータを手で削除・変更している間の協調は対象外です。

## 複数リポジトリの所属

カタログは frame とは別に `(repo, kind, id)` の所属と、名前ごとのリポジトリ識別子を保存します。
所属のない frame は、そのリポジトリでは上位に確認してから返します。識別子が変わった名前
（削除後の同名再作成など）の所属は破棄します。単一リポジトリ時代のカタログは、開いたときに
既存の全 frame をそのリポジトリの所属として登録します。snapshot（manifest version 2）は所属を含み、
`kasu-daemon snapshot --repo` で対象リポジトリを選べます。version 1 の snapshot も読み込めます。

## 中断と復旧

ファイルを変更する前に SQLite に dirty intent を確定し、marker・frame・directory を同期してから
索引とカウンターを commit します。途中でプロセスが終了した場合は、次の操作または open が排他的に復旧します。

- サイズ marker が公開済みで frame が存在するものから型付き索引を再構成する。
- frame のない marker と、どの marker からも参照されない canonical frame を回収する。
- 中断した所有 temp file と `.snapshot-import-` の staging を回収する。
- 容量・索引数を再計算し、上限へ縮小して dirty intent を解除する。

通常の読み取りは型付き索引・サイズ・zstd・内容 hash を検証します。
壊れた圧縮 frame や不正な marker はエラーとして扱い、配信や自動再取得で隠しません。
復旧による未公開 frame／欠落 marker の回収は、後続の正当な cache miss として再取得できます。
SQLite 自体の破損は自動的に削除・再作成しません。

## snapshot と世代管理

定期保存は `data/snapshots/kasu-snapshot-v1-<timestamp>.tar` を一時ファイル経由で公開します。
成功後と起動時に、数値 timestamp が新しい `--snapshot-keep` 件を残します。
新規書き込み中は一時的に1世代分の追加容量を必要とします。
prune は同じ cache の排他 lock を取るため、他の export の書き込み途中のファイルを削除しません。
所有する未完了の snapshot temp file も回収します。

`user.tar`、以前の数字だけの `<timestamp>.tar`、手動で別の場所へ保存した archive は削除しません。
旧形式の定期 archive や利用者のファイルは運用者が管理します。管理用のファイル名・directory は予約されています。

snapshot 形式は version 1 のままです。import は全 entry の検証と容量の事前検査を済ませてから公開し、
全 manifest キーを pin して import の途中で先の entry が eviction されることを防ぎます。
キャッシュの上限に収まらない archive は、公開・eviction の前に拒否します。
既存 frame が別の圧縮表現の場合は、維持する既存 frame の実サイズで事前検査します。
検証後の IO 障害では、検証済みの一部だけが残る場合があります。

archive は型付き entry ごとに payload を持つので、共通 frame でも2つの型に属すれば2回収録します。
import の `--snapshot-max-bytes` はこの合計を検査します。export／import は最大10万 entry・manifest 16 MiB を維持します。
キャッシュを10万 entry より大きく設定した場合、全体の export はこの上限を超えるとエラーになります。

## メトリクスと検証

既存の `kasu_object_entries` は型付き索引数、daemon の `kasu_object_bytes` は重複を除く frame bytes です。
次のメトリクスを `layer="daemon"` で追加します。

| メトリクス | 意味 |
|---|---|
| `kasu_object_disk_limit_bytes`, `kasu_object_disk_limit_entries` | 設定した上限 |
| `kasu_object_disk_evictions_total` | 容量のために削除した型付き索引数 |
| `kasu_object_disk_evicted_bytes_total` | 最後の索引の削除により解放した frame bytes |
| `kasu_object_disk_admission_skips_total` | 単体 frame の保存を見送った回数 |
| `kasu_object_disk_repairs_total` | 復旧で回収した欠落 marker・孤立 frame・所有 temp の数 |

累積値は確定した操作の値を catalog に永続化し、中断した削除数の完全な復元は行いません。
メトリクスはディスクの lock を取らない atomic counter 群による近似値です。
他のインスタンスによる更新は、そのインスタンスが次の操作を行った後に反映されます。
正確な共有状態は `DiskStore::stats()` で照会できます。

[検証記録](simulations/disk-lru/summary.json) に macOS と固定 Rust 1.90 Linux のログ・ソース hash を保存します。
検証は LRU の再 open、byte／entry 上限、型を共有する frame、別圧縮表現、過大 frame、legacy 移行、
不正な directory、snapshot pin・事前拒否、並行インスタンス／export、整数境界とメトリクスの非待機を含みます。
書き込みを SIGSTOP してファイルと SQLite の不一致を実際に観測した後に SIGKILL し、
再 open・全残存 frame の検証・`integrity_check`・上限遵守を確認します。
実 daemon は1-entry cache での追い出し・再取得、強制終了後の warm hit、単体過大 frame の非保存、
定期 snapshot の世代数と無関係な archive の保持を検証します。

これは大規模な実運用の容量・スループットの保証ではありません。
filesystem の電源断、ネットワーク filesystem、disk-full を含む広範な IO 障害、巨大 catalog／snapshot の性能評価は残作業です。
