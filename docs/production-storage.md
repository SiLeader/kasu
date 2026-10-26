# PostgreSQL／S3 と複数 server

`kasu-store` の `production` feature は PostgreSQL の MetaStore／RefStore／GitRefStore／
WebhookStore と S3 の BlobStore を有効にします。
`kasu-server`／`kasu-ingest` の同じ feature から利用できます。

```sh
cargo build -p kasu-server -p kasu-ingest --features production --bins --locked

export KASU_POSTGRES_DSN_FILE=/run/secrets/kasu-postgres
export KASU_S3_BUCKET=my-kasu-bucket
export KASU_S3_REGION=ap-northeast-1
export KASU_S3_PREFIX=kasu/objects/v1
export KASU_TOKEN=your-user-token

target/debug/kasu-ingest /path/to/repository.git /path/to/local-fallback my-repo
target/debug/kasu-server /path/to/local-fallback my-repo 127.0.0.1:50051 127.0.0.1:50080
```

DSN ファイルは、たとえば `host=db.example port=5432 dbname=kasu user=kasu sslmode=require` などの
PostgreSQL 接続設定です。パスワードが必要なら同ファイルに含めて保護してください。
最大64 KiB、UTF-8、末尾の CR／LF は除去します。空の接続設定は拒否します。
TLS は WebPKI roots とホスト名で検証し、プライベート CA は PEM ファイルの
`KASU_POSTGRES_CA_FILE` で追加できます。
TLS の `prefer` は `require` に引き上げ、暗号化できない接続へ自動で切り替えません。

S3 の認証情報は [object_store の AWS 設定](https://docs.rs/object_store/0.14.2/object_store/aws/struct.AmazonS3Builder.html#method.from_env)
に従います。環境変数の access key／secret／session token、web identity、ECS／EC2 の資格情報供給元を利用できます。
キーの値を CLI 引数や working copy に保存しません。
bucket は事前に作成してください。同じ bucket／prefix を利用する server／ingest に、
必要な GetObject／PutObject 権限を付与します。ストアはオブジェクトを削除しません。
既存オブジェクトを保持する bucket policy と資格情報の管理は運用側で行ってください。

| 環境変数 | 動作 |
|---|---|
| `KASU_POSTGRES_DSN_FILE` | 設定時は PostgreSQL、未設定時はローカル SQLite |
| `KASU_POSTGRES_CA_FILE` | PostgreSQL 用の追加 CA 証明書 |
| `KASU_S3_BUCKET` | 設定時は S3、未設定時はローカル FS |
| `KASU_S3_PREFIX` | 圧縮オブジェクトの prefix。既定 `kasu/objects/v1` |
| `KASU_S3_REGION` | AWS region の明示指定。省略時は AWS 設定を利用 |
| `KASU_S3_ENDPOINT` | S3 互換サービスの URL。既定は AWS の endpoint |
| `KASU_S3_PATH_STYLE` | `true`／`1` で path style。既定 false |
| `KASU_POSTGRES_ALLOW_PLAINTEXT` | `true`／`1` と DSN の `sslmode=disable` を併用して平文を明示許可 |
| `KASU_S3_ALLOW_PLAINTEXT` | `true`／`1` で HTTP endpoint を明示許可 |

外部ストアを指定したときの設定・接続エラーは起動または要求を失敗させます。
ローカルストアへ切り替えて別の状態を書き込むことはありません。
feature なしのバイナリも、指定された PostgreSQL／S3 設定を無視せず起動エラーにします。
両方の外部ストアを指定した場合は、位置引数のローカル directory を作成・利用しません。
複数 server では必ず同じ PostgreSQL と S3 bucket／prefix を共有してください。
片方だけの外部化もできますが、その場合もう片方のローカルデータの共有は運用側で必要です。

## PostgreSQL の保存契約

起動時に専用の `kasu` schema と version 1 のテーブルを作成します。
初期化の DDL だけに advisory lock を使います。schema の削除や既存 SQLite からの
自動移行は行いません。未対応の schema version は拒否します。
初期化と通常の保存に必要な権限を持つ専用 DB ロールを用意してください。

不変 KV は BLAKE3 のキー索引と元のキー・値を保持します。
`INSERT ... ON CONFLICT DO NOTHING` の後に一致を検査し、別の値への上書きを拒否します。
長いパスや非 UTF-8 のキーを PostgreSQL の B-tree の行サイズ上限に依存せず保存できます。
repo と ref の索引も固定長 hash と元バイト列を照合します。

可変 ref は repo／型／名前ごとの行をロックしてから、target と version の両方を比較します。
複数 ref のロック順を固定し、全件一致時のみ ref とログを同じトランザクションで更新します。
削除は version を保持した tombstone です。branch と Git-only ref の混合 transaction と
[Git atomic push](git-push.md) に対応します。

ログの番号には `SERIAL`／`nextval` を使いません。
[PostgreSQL の sequence](https://www.postgresql.org/docs/18/functions-sequence.html) はトランザクションと独立して進むため、
発行順と commit 順が逆転すると cursor 配信が遅い transaction のログを取りこぼす可能性があります。
repo／stream ごとの行カウンターをログ追記と同じ transaction で増やし、
後続番号が先に commit することを防ぎます。この短い区間は同じ repo／stream で直列化します。
無関係な repo のカウンターは共有しません。

Land は repo／冪等キーの claim、結果の fingerprint、応答 bytes を共有します。
claim、ref CAS、ログ、結果の保存は同じ transaction に入り、キャンセル時は rollback します。
同じキーへの別接続の要求は元の結果を返し、違う fingerprint は拒否します。
webhook も endpoint／stream ごとの cursor・確定済み本文・試行数・次回時刻を PostgreSQL に保持します。
別 server が再送しても本文が変わらず、古い worker は cursor を巻き戻せません。
外部 HTTP 配信は at-least-once です。

各 `PostgresStore` は1つの同期接続を再利用し、インスタンス内では直列化します。
SQL 実行のためにトランザクションを不変 KV 全体へ広げることはありません。
既定の接続期限5秒、SQL statement 期限10秒、ロック待ち期限5秒、idle transaction 期限30秒、
`synchronous_commit=on` を設定します。
接続が閉じた後は次の要求で再接続します。失敗した SQL／commit を内部で自動再実行しません。
応答喪失時の Land は元のキーで再送できます。通常の CAS は最新の ref を確認してください。
大規模運用向けの接続 pool と容量評価は今後の作業です。

## S3 の保存契約

キーは `<prefix>/<先頭2桁>/<残り62桁>`、内容は保存済み zstd frame です。
[S3 の conditional write](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html) を利用し、
`If-None-Match: *` で作成します。
既存時は展開後の内容を照合し、同じ raw bytes の異なる圧縮 frame が来ても最初の frame を保持します。
conditional write 非対応を通常の上書き PUT に置き換える fallback はありません。
S3 互換サービスにも、conditional create と read-after-write の整合性が必要です。

読み取りと書き込みで、展開上限256 MiB と BLAKE3 を検証します。
圧縮 bytes は最大257 MiB、受信ストリームも上限と Content-Length を検査します。
破損した既存オブジェクトはエラーになり、通常の put で修復・上書きしません。
オブジェクトの frame は転送・取得時に再圧縮しません。
S3 GET／PUT と PostgreSQL の保存は分散 transaction にせず、
不変オブジェクトを先に保存してから ref を公開します。失敗した要求の未参照オブジェクトが残る場合があります。

S3 の処理全体は既定15秒、接続は5秒、最大2回の内部再試行です。
再試行も15秒の処理全体期限に含めます。
プロバイダーのエラーから SQL パラメーター、接続 DSN、署名付き URL、資格情報を応答に露出させません。
データの削除・保持世代・バックアップと復元は別の運用手順で管理してください。

## 実通信の検証

```sh
python3 tools/storage/run.py --workdir /tmp/kasu-storage-host
python3 tools/storage/run.py --linux --workdir /tmp/kasu-storage-linux
```

Docker Compose の固定 PostgreSQL 18／SeaweedFS 4.45 を localhost の専用ポートへ起動します。
既定ポート55432／58333を変える場合は `KASU_TEST_PG_PORT`／`KASU_TEST_S3_PORT` を指定します。
検証用資格情報は `tools/storage/s3.json` の公開されたテスト値だけを使い、クラウドアカウントは不要です。
短期テスト CA を生成し、未信頼 CA の拒否と明示 CA の TLS 接続、匿名 S3 の拒否も確認します。
終了時にこの実行の Compose project と volume を削除します。`--keep` で調査用に保持できます。
作業 directory にログ、公開 CA 証明書、成功時の `summary.json` を保存します。

- 16接続の混合 CAS で単一勝者、ref と両種ログの一致、ABA／stale の拒否。
- SQL 障害時の ref／ログ／カウンター rollback、未 commit の小さい番号を後続ログが追い越さないこと。
- 8接続の同一 Land キー、再接続とキャンセル、webhook 本文の競合と stale worker。
- S3 の16並行 writer、異なる圧縮 frame の不変保存、再オープン、破損の拒否。
- 実 ingest バイナリと2つの server プロセスによる Git atomic push／strict fsck、
  Metadata／Objects の即時共有、native CAS の単一勝者、別 server・再起動後の Land replay。
- 誤った S3 資格情報による取得失敗と、資格情報の非露出・ローカル fallback なし。

この検証はローカルの S3 互換 API を対象とします。実 AWS の IAM／KMS／ネットワーク障害、
地理的に離れた配置での容量・遅延・災害復旧までを検証したものではありません。
2026-10-23 の [macOS／Linux の実行記録](simulations/README.md#共有ストレージの受け入れ試験) を保存しています。
[TiKV の初期評価](benchmarks/kv-evaluation.md) では複数 region の transaction、replica／PD 障害、
全再起動後の全件照合を実施しました。既定の共有ストレージは PostgreSQL／S3 を継続します。
TiKV の製品用 Rust adapter、同等 HA 構成・大規模負荷・災害復旧の評価は今後の作業です。
