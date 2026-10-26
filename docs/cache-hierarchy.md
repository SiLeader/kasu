# オブジェクトの cache／daemon 階層

`kasu-cache` はリージョンのメモリ LRU、`kasu-daemon` はノードの永続ディスクを提供します。
どちらも [Objects RPC](object-api.md) の GetTrees／GetBlobs だけを提供し、
Metadata／Writer は CLI から権威サーバーへ接続します。

```text
kasu CLI ─ Unix socket ─ daemon ─ gRPC ─ cache ─ gRPC ─ server
                         disk            memory       authority
```

各インスタンスは1つの repo と1つの下位トークンに固定します。
daemon の Unix socket は所有者だけが接続できる mode 600 です。
ノード上の同じ UID のジョブで共有します。socket の親は同じ UID 所有の mode 700 など、
group／other に権限のない directory が必要です。

## 起動

```sh
cargo build -p kasu-server -p kasu-cache -p kasu-daemon -p kasu-cli --locked
export PATH="$(pwd)/target/debug:$PATH"
export KASU_TOKEN=dev-token
export KASU_SERVER=http://127.0.0.1:50051
export KASU_REPO=my-repo
export KASU_LAYER_RUN="$(mktemp -d)"
openssl rand -hex 32 > "$KASU_LAYER_RUN/server-hop"
openssl rand -hex 32 > "$KASU_LAYER_RUN/daemon-hop"
printf '%s' "$KASU_TOKEN" > "$KASU_LAYER_RUN/user-token"
```

同じ環境変数を設定した別々の端末で各プロセスを起動します。
server のデータは [Git 取り込み](git-import.md) などで準備します。

```sh
KASU_OBJECT_TOKEN_FILE="$KASU_LAYER_RUN/server-hop" \
  kasu-server /path/to/kasu-data "$KASU_REPO"

kasu-cache --repo "$KASU_REPO" --upstream "$KASU_SERVER" \
  --token-file "$KASU_LAYER_RUN/daemon-hop" \
  --upstream-token-file "$KASU_LAYER_RUN/server-hop" \
  --listen 127.0.0.1:50091 --metrics 127.0.0.1:50092

kasu-daemon serve --repo "$KASU_REPO" --upstream http://127.0.0.1:50091 \
  --token-file "$KASU_LAYER_RUN/user-token" \
  --upstream-token-file "$KASU_LAYER_RUN/daemon-hop" \
  --data "$KASU_LAYER_RUN/store" --socket "$KASU_LAYER_RUN/node.sock" \
  --metrics 127.0.0.1:50093

KASU_OBJECTS_ENDPOINT="unix://$KASU_LAYER_RUN/node.sock" \
  kasu checkout main work --sparse app
```

上の各起動コマンドは foreground で動きます。SIGINT で終了できます。
トークンは起動時にファイルから読み込み、末尾の CR／LF を除去します。
ファイルは改行込みで最大4096 byte、空値は拒否します。
`--token-file`／`--upstream-token-file` は `KASU_LAYER_TOKEN_FILE`／
`KASU_UPSTREAM_TOKEN_FILE` でも指定できます。トークンを working copy やログに保存しません。
cache の TCP 待受は平文です。別ホストへの公開は TLS 終端を用意し、
上位への HTTPS 接続では WebPKI roots を使用します。

`KASU_OBJECT_TOKEN_FILE` を設定した server は、通常のユーザー PAT での Objects RPC を拒否します。
cache は daemon 用トークン、daemon はユーザートークンを受け取り、隣接する2つのトークンの
同一値を拒否します。上位応答の `x-kasu-object-layer` も照合します。
この設定と資格情報の分離で階層のスキップを防ぎます。
server の同変数を省略した場合は従来の直接 Objects 接続を許可します。

CLI のグローバル `--objects <URL>`／`KASU_OBJECTS_ENDPOINT` は Objects だけの接続先です。
checkout では選択した URL を保存し、それ以降のコマンドでは今回の実行だけ上書きします。
既存 working copy の移行にも使えます。Unix socket では応答の daemon role も検査します。
daemon／cache の障害時に他の上位や server へ接続し直す fallback はありません。

## 負荷の上限

各プロセスで次の既定値を使います。全下位接続の合計に適用します。

| オプション | 既定値 | 対象 |
|---|---:|---|
| `--streams` | 16 | 同時 Objects RPC。超過は即時拒否 |
| `--fills` | 2 | ディスク取得・上位取得・検証・保存を行う同時 fill |
| `--pending` | 128 | 異なる未完了オブジェクト。超過は即時拒否 |
| `--qps` | 20 | 上位への取得開始を一定間隔に制限 |
| `--queue-timeout-ms` | 1000 | fill 枠の待機期限 |
| `--upstream-timeout-ms` | 5000 | 上位の1オブジェクト取得期限 |
| `--cooldown-ms` | 1000 | 上位取得または取得後の保存が3回連続失敗した後の遮断時間 |
| cache の `--memory-bytes` | 67108864 | LRU に保持する圧縮 frame の合計 |

同じ kind／ID の未完了要求は singleflight で1回にまとめます。
最初の要求者が切断しても、他の要求者と cache 保存のために有界な fill を継続します。
遮断時間後は1回の probe だけを許可し、復旧していれば再開します。自動再試行はしません。
cache が停止すると daemon の既存ディスクヒットは使えますが、cold miss は失敗します。
レート制限はプロセス単位です。cache 台数を増やす場合はその合計を server の容量に合わせます。

LRU の予算は保持する圧縮 bytes を数えます。配信中の frame、RPC バッファ、検証時の展開メモリは
別途必要です。オブジェクトは core の展開上限256 MiB と共通の圧縮 frame 上限で検証します。
RPC は最大128個の重複しない ID、64 KiB 以下の chunk、有界な送信キューを使用します。
ストレージへの blocking IO の時間自体には期限がないため、遅いディスクは fill 枠を占有します。

保存済み zstd frame を再圧縮せず転送します。
miss とディスクヒットでは展開・hash・Tree の canonical 検証を行い、メモリヒットは検証済み frame を使います。
Tree／Blob は別の型付き索引を持ち、未索引データは配信しません。
圧縮 frame の破損はエラーとして扱い、自動削除・上位からの再取得は行いません。
中断した公開・eviction の復旧と未公開データの回収は [ディスク容量管理](disk-cache.md) を参照してください。

## snapshot とディスクの管理

```sh
kasu-daemon snapshot --data "$KASU_LAYER_RUN/store" --repo "$KASU_REPO" /path/to/node.tar

kasu-daemon serve --repo "$KASU_REPO" --upstream http://127.0.0.1:50091 \
  --token-file "$KASU_LAYER_RUN/user-token" \
  --upstream-token-file "$KASU_LAYER_RUN/daemon-hop" \
  --data "$KASU_LAYER_RUN/restored" --socket "$KASU_LAYER_RUN/restored.sock" \
  --snapshot /path/to/node.tar
```

手動 export は既存ファイルを上書きしません。起動中のデータも export できます。
serve は既定で3600秒ごとに `data/snapshots/kasu-snapshot-v1-<timestamp>.tar` を保存します。
`--snapshot-interval-secs 0` で定期保存を無効にできます。
daemon は既定で zstd payload 8 GiB・型付き索引10万件の永続 LRU と、定期 snapshot 2世代の保持を使います。
`--disk-bytes`／`--disk-entries`／`--snapshot-keep` で変更できます。
共有 frame の計算、起動時の移行・復旧、容量に含まないデータ、旧 archive の扱いは [ディスク容量管理](disk-cache.md) を参照してください。

形式は先頭の `manifest.json` と `objects/<kind>/<id>.zst` の tar です。
repo・型・ID・サイズ・hash を検査し、リンクや別パス、同型キーの重複、過大な manifest を拒否します。
最大10万オブジェクト、manifest は最大16 MiB、起動時の frame 合計は既定8 GiBです。
後者は `--snapshot-max-bytes` で指定できます。archive 自体にも追加64 MiB の上限があります。
tar を filesystem に直接展開せず、一時ストアで全件を検証してから型付き索引を公開します。
不正な snapshot の一部だけが配信可能になることはありません。
検証後の公開中に IO 障害が起きた場合は、検証済みの一部が残る可能性があります。
snapshot は信頼した運用者が配布するものとし、認可と repo の対応を維持してください。

ディスクの repo は `scope.json` に固定され、違う repo での再利用を拒否します。
同じデータ directory と socket の二重起動をロックで拒否します。
強制終了後は所有者が一致する停止済み socket のみ再作成し、通常終了時は自身の socket を削除します。

## 観測と検証

`--metrics <address>` を指定すると HTTP `/metrics` に Prometheus text を公開します。
既定では metrics ポートを開きません。認証のない運用 endpoint なので待受先を管理してください。
`kasu_object_*{layer="cache|daemon"}` に hits／misses／coalesced／rejected／upstream／errors、
pending、保持数・bytes、遮断回数を出します。
active／max_active は上位呼び出し開始から検証・保存完了までの fill 数です。
requests は認可済みオブジェクト lookup の数で、拒否されたすべての RPC を含みません。
hit 率と coalesced 数を併せて見て、cold burst の singleflight 効果を判断できます。

```sh
cargo test -p kasu-cache -p kasu-daemon -p kasu-cli --locked
docker build -f tools/cache/Dockerfile -t kasu:cache-test .
```

実 TCP／Unix socket で1,000クライアント、異なる ID の pacing、fill／pending／stream 上限、
回路遮断と復旧、切断、snapshot の破損・リンク・サイズ拒否、実バイナリの再起動、CLI を検証します。
負荷試験の範囲と観測値は [記録](simulations/README.md#cache-階層の負荷試験) を参照してください。

## 決定的な障害シミュレーション

[仮想時計の cache シナリオ](simulations/README.md#決定的な-cache-階層障害) は本番 Layer と Objects を使い、
2 node の集約、cache 不通、途中で切れる stream、取得期限、保存失敗と cooldown／probe、ディスク再オープンを検証します。
pacing・取得期限・回路遮断器は同じ Tokio 時計を使います。通常の runtime では実時間、pause した試験 runtime では仮想時間です。
4 seed を各2回、macOS／固定 Rust 1.90 Linux で実行し、1,000クライアントの全イベントと結果 digest が一致しました。
