# Land

`kasu.v1.Writer.Land` と HTTP POST `/v1/land` は、Changeset を指定 branch へ着地させます。
認証・repo 所属の規則は [Write API](write-api.md) と共通です。

リクエストは `repo`、`id`、`name`、`idempotency_key` と任意の `max_attempts`／`timeout_ms`／`required_ancestor` です。
キーは1〜256バイトで、repo 内の「source Changeset・ref・API actor」と、指定時の `required_ancestor` に結び付きます。
同じキーを異なる内容に使うと InvalidArgument です。
試行回数や時間の予算は結び付きに含めないため、Retry 後に同じキーで予算を変えて再送できます。

`required_ancestor` は省略または32 bytes の Changeset ID です。指定すると、各試行の現在先端がその子孫であることを検査します。
検査した先端の target／version を同じ Land の CAS に使うため、検査後の branch 置換・削除・ABA は CAS 失敗になり、次の試行で再検査します。
先端がない場合や祖先条件を満たさない場合は `Denied` で、branch を作成しません。
既存 receipt の replay は現在の先端の条件検査より優先します。条件を指定しない既存リクエストの fingerprint は変えていません。

HTTP POST `/v1/guarded-land` は、32-byte の祖先条件を必須とする endpoint です。
この endpoint がない旧サーバーに対しても条件付き操作を通常 Land へ切り替えず停止でき、
[revert bot](automation.md) はこの endpoint を使います。gRPC の条件指定は、この field を実装したサーバーが必要です。

## 着地と結果

1. 保存済みの確定結果があれば、それを `replayed=true` で返します。
2. 現在の ref とポリシーを読み、source の第一親との差分を現在の先端へ3-way rebase します。
   新規 branch では source を使用します。source が既に先端の祖先なら再適用しません。
3. 結果を repo に所属させ、Git commit 射影を準備します。
   内容が先端と同じなら、新しい空 Changeset を着地させずに現在の先端を採用します。
4. ref の target と version を CAS し、確定結果・ref_log を同じトランザクションで保存します。
5. CAS が競合したらポリシーを再検査し、元の source を新しい先端へ再適用します。

| outcome | 意味 |
|---|---|
| `Landed` | id と actual に着地結果。既に含まれる変更なら ref の version／log は増やさない |
| `Conflict` | conflict に構造化競合。ref が計算時の状態のままであることを検査して保存 |
| `Retry` | CAS 競合、保存障害、キャンセル、期限切れで試行予算を終了。同じキーで再送可能 |
| `Denied` | ref の書き込み／作成ポリシーが許可しない |

認証失敗、他 repo、存在しない source、壊れた入力は通常の RPC／HTTP エラーです。
Landed と Conflict は永続化し、Retry と Denied は確定結果として保存しません。
Conflict の解消後に別の source を着地させる場合は、新しいキーを指定します。
保存した actual はその着地時点の状態です。ref が後から進んでも、再送では元の結果を返します。

Land は1 Changeset の再適用です。merge Changeset も第一親との差分として扱い、
再適用結果の親は現在の ref 先端1つになります。
複数コミットの feature branch 全体をまとめて再適用する操作ではありません。

## 上限とキャンセル

既定は最大8回／5秒、指定できる上限は32回／30秒です。
競合・一時的な保存障害では、要求と試行番号に基づく jitter を加えた指数バックオフを使います。
待ち時間は最大200 msです。キャンセルと期限は待機中も監視します。

`Backend::land_with_cancellation` は外部の `LandCancellation` を受け付けます。
RPC／HTTP handler の future が破棄された場合も、実行中の worker にキャンセルを伝えます。
SQLite はトランザクション内でキャンセルを検査し、保存後の検査でキャンセルされていれば
ref・ref_log・結果のすべてをロールバックします。
期限切れの応答は、blocking worker の終了を待ちません。
準備中の不変オブジェクトが残る場合はありますが、ref 公開はキャンセル検査を通ります。

キャンセルとコミットが競合した場合は、同じキーの再送で保存済みの結果を確認できます。
応答喪失やサーバー再起動後も同じ手順で確認でき、二重着地を防ぎます。

## 検証

```sh
cargo test -p kasu-store --test land --locked
cargo test -p kasu-server --test write --locked
cargo test -p kasu-sim --test land_runtime --locked
cargo run --release --locked -p kasu-sim -- --seed 42 --clients 1000
```

ストアの16接続での重複キー競合、再オープン、トランザクション途中のキャンセル、
実 gRPC／HTTP の再送、ref 移動後の元の結果、保存障害、応答喪失、期限切れ、future 破棄、
準備とコミットの間のポリシー変更を検査します。
`kasu-sim` は本番の prepare／commit 処理と実 SQLite／FS ストアを使い、仮想時刻で
全クライアントの要求・CAS・再送・障害を決定的に並べ替えます。
ネットワークスループットのベンチマークではありません。
[1,000クライアントの実行記録](simulations/README.md) を保存しています。
