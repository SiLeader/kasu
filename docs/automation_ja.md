# Metadata API のテスト gate・bisect・revert bot

[English](automation.md) | 日本語

`tools/automation/kasu_bot.py` は、着地後テストとデプロイ前テストに使う Python 標準ライブラリだけのリファレンス実装です。
Python 3.11 以降と POSIX の process group／file lock を使います。Git や working copy は使いません。
接続先は gRPC ではなく、サーバーの HTTP JSON ポートです。

```sh
python3 tools/automation/kasu_bot.py \
  --endpoint http://127.0.0.1:50080 --repo my-repo \
  --token-file /run/secrets/kasu.pat --journal /var/lib/kasu-bot/check.json \
  check --revision "$CHANGESET_ID" --ref refs/heads/main --test-id "$TEST_IMAGE_DIGEST" \
  -- python3 tools/automation/check_file.py --path deploy/app.yaml --contains 'healthy: true'
```

ID は Git SHA ではなく、64桁のネイティブ Changeset ID です。Git SHA を受け取る連携では
Metadata の `FindChangesetByGitOid` で先に変換します。ref を使う連携では `ResolveRef` で immutable ID を取得します。
PAT は秘密ファイルから読みます。射影専用 PAT は native API を使えないため、この bot には使えません。
`check`／`bisect` 本体と同梱 checker は Metadata の読み取りだけを呼び、`revert` は Writer API も使います。

## Tester の契約

`--` の後はシェルを介さず argv として起動する、CI 側で用意した tester です。
環境変数 `KASU_METADATA_ENDPOINT`、`KASU_REPO`、`KASU_CHANGESET`、指定時は `KASU_TOKEN_FILE` を渡します。
tester は指定 ID の snapshot を評価し、次の終了コードを返します。

| tester の終了コード | 意味 |
|---|---|
| 0 | good／テスト成功 |
| 1 | bad／対象コード・設定のテスト失敗 |
| 125 | その revision は判定できないため skip |
| その他 | tester／認証／通信などの異常。bad として記録せず処理を中断 |

stdout／stderr は bot の出力に混ぜません。CI の詳細ログが必要な tester は、自身でログ先を指定します。
tester が fork した子も含め、終了・タイムアウト・キャンセル時に専用 process group を停止します。
既定の tester 上限は30秒、操作全体の予算は300秒です。`--test-timeout` と、subcommand より前の `--timeout` で変更できます。
タイムアウトした revision に bad の判定は保存しません。

同梱の `check_file.py` は、ReadFile の内容に指定した bytes があるか調べる小さな例です。
`--path-hex`／`--contains-hex` で非 UTF-8 パスやバイナリ値も指定できます。
ファイル不在・期待値不在は1、認証・通信・引数不備は2を返します。
実際の CI では、アプリケーションや manifest に適した tester に差し替えます。

## Check とデプロイ gate

`check` は指定 ID を検証し、`passed`／`failed`／`skipped` を返します。
`--ref` 指定時はテスト前後の target と version を比較し、先端が違う場合や途中で ABA が起きた場合は `stale` になります。
デプロイ先は結果の `id` に固定します。ref の比較後に進んだ新しい先端へは、そのテスト結果を適用しません。

着地後テストでは Land の結果 ID をそのまま `check --revision` に渡せます。
先端が既に進んでいても着地した snapshot 自体を検証する場合、`--ref` を省略します。
デプロイ前の current-head gate では `--ref` を併用し、成功した ID に対応する成果物をデプロイします。

## DAG bisect

```sh
python3 tools/automation/kasu_bot.py \
  --endpoint http://127.0.0.1:50080 --repo my-repo \
  --token-file /run/secrets/kasu.pat --journal /var/lib/kasu-bot/bisect.json \
  bisect --good "$LAST_GOOD_ID" --bad "$FAILED_ID" --test-id "$TEST_IMAGE_DIGEST" \
  -- python3 tools/automation/check_file.py --path deploy/app.yaml --contains 'healthy: true'
```

GetChangeset で bad の全親方向の DAG を読み、good の祖先を候補から除きます。第一親だけには限定しません。
good は複数指定でき、すべて bad の厳密な祖先である必要があります。境界も tester で検証します。
候補をできるだけ二分する revision を選び、good ならその祖先を除去、bad ならその祖先に候補を限定します。
原因一つ・同じテスト定義・子孫方向に good から bad へ移る、という通常の bisect の仮定を使います。

skip のため一意に決まらない場合は `ambiguous` と候補集合を返し、原因を断定しません。
既定は最大10,000 Changeset、境界の検証に加えて新たな tester 実行64回です。
`--max-nodes` は最大100,000、`--max-tests` は最大10,000まで設定できます。
探索・RPC・tester は全体予算を共有し、予算切れは再開可能な `retry` になります。
候補ごとの祖先集合を比較する参照アルゴリズムなので、大規模 DAG の探索性能は未評価です。

## 3-way revert

```sh
python3 tools/automation/kasu_bot.py \
  --endpoint http://127.0.0.1:50080 --repo my-repo \
  --token-file /run/secrets/kasu.pat --journal /var/lib/kasu-bot/revert.json \
  revert --bad "$CULPRIT_ID" --branch refs/heads/main --operation-id "$CI_REVERT_ID"
```

このコマンドは実際に ref を更新します。revert を行う CI job から明示的に起動します。
merge の revert は `--mainline 1` など、1-based の親番号を必須にしています。root commit も対象にできます。

1. bad から選んだ親へ向かう逆差分を Metadata の Diff で取得します。
   旧 blob ID の所属索引を GetRootTree で補完し、内容をダウンロードせず再利用します。
2. bad を親とする inverse Changeset を CreateChangeset で作成します。root の場合は元の各 entry を削除します。
3. 現在の branch 先端へ Rebase します。同じファイルの無関係な後続編集を保持し、重なる編集は conflict として返します。
4. `/v1/guarded-land` で着地します。bad が現在の先端の祖先であることを各試行で検査し、
   その先端の target／version を CAS するため、途中の force update・削除・ABA で条件をすり抜けません。
5. 既存の ref／最終 tree ACL、有限リトライ、Land の冪等結果をそのまま使います。

guarded endpoint がない旧サーバーではエラーになり、通常の Land へフォールバックしません。
rebase／Land の conflict は終了コード2で止まり、ref は更新しません。
Land の一回の要求は8試行／5秒です。`retry` の場合は同じ journal と引数で再実行できます。
bad が branch から外れた場合は `denied` で、別の履歴へ逆差分を適用しません。

## Journal と再開

各操作には `--journal` が必要です。ファイルは0600で atomic replace／fsync し、同時使用は OS の file lock で拒否します。
repo、endpoint、操作、ID、テスト定義または revert 設定の結び付きを記録します。
別操作や別の tester を同じ journal に流用すると拒否します。予算だけは増やして再開できます。
`--test-id` には、tester の image digest と設定を識別する、再現可能な値を使います。
tester 本体の差し替え・環境の変化・flaky な結果を、自動で同一視する仕組みではありません。

revert は metadata・inverse ID・rebase 先・Land ID・キーを、次の API 呼び出しより前に保存します。
Land 送信後の応答喪失では、新しい draft を作らず同じ ID／キーを再送します。
`--operation-id` は repo 内で一意にします。同じ ID を別の操作に使うと、保存済み Land の fingerprint と一致せず拒否されます。
journal は再開に必要です。確定済み receipt は履歴上の結果で、再取得時の現在先端を表すものではありません。
terminal conflict を別の先端でやり直す場合は、新しい operation ID と journal を使います。

| bot の status | 終了コード |
|---|---|
| `passed`／`culprit`／`landed` | 0 |
| `failed`／設定・認証・tester 異常の `error` | 1 |
| `skipped`／`ambiguous`／`conflict`／`denied` | 2 |
| `retry`／`stale` | 3 |

処理結果は stdout の JSON で返します。bisect の0は「原因を特定した」であり、デプロイ gate の成功には `check` の `passed` を使います。
Metadata response／journal は最大16 MiB、認証ファイルは最大4096 bytesに制限します。HTTP redirect は追跡しません。

## 検証

macOS arm64 の Python 3.14.8 と、固定 Rust 1.90／Linux arm64 の Python 3.11 で検証しています。

- 12種類の分岐・merge DAG について全12候補、合計144通りの原因を探索。
- skip の候補集合、予算切れの再開、判定キャッシュ、tester のタイムアウト／異常終了と process group の停止。
- 実 Metadata API の第二親にある回帰の特定。check／bisect は Metadata 読み取り endpoint だけを利用。
- 実 Writer API で後続編集を保持する revert、conflict、merge mainline、root revert、同じ journal の再実行。
- Land 保存後の応答喪失と、その後の branch 置換を経た receipt の replay。ref log が二重に増えないこと。
- CAS に負けた後の祖先条件再検査、条件を変えたキー流用の拒否、redirect／条件欠落の拒否。
- check の成功／失敗、テスト前の先端不一致、テスト中の ABA。native ID を固定した gate の判定。

[検証記録](simulations/automation/summary.json) にソースのハッシュとログを保存しています。
GitHub CI に Python unit と既存の Rust 全 feature テストを追加していますが、GitHub 上の新しい実行結果は未確認です。
CI job や実デプロイ先への自動接続、OIDC、分散した journal 保管、大規模 DAG の容量評価は今後の対象です。

```sh
python3 -m unittest discover -s tools/automation -v
cargo test -p kasu-server --test automation --locked
docker build -f tools/automation/Dockerfile -t kasu:automation-test .
```
