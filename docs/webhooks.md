# ref 更新の webhook

`KASU_WEBHOOKS_FILE=/path/to/webhooks.json` を指定すると、GitHub push 形式の
POST を配信します。設定は起動時に読み込みます。

```json
{
  "repository_url": "https://kasu.example/demo.git",
  "default_branch": "main",
  "endpoints": [
    {
      "id": "argocd",
      "url": "https://argocd.example/api/webhook",
      "secret_file": "webhook-secret"
    }
  ]
}
```

`secret_file` は設定ファイルからの相対パス、または絶対パスです。
ファイルのバイト列をそのまま HMAC-SHA256 の鍵に使います。改行も鍵の一部です。
受信側にも同じ鍵を設定してください。`secret_file` を省略すると署名しません。
ArgoCD は `webhook.github.secret`、Flux は `type: github` の Receiver と
その `secretRef` を使います。URL は HTTP／HTTPS を受け付け、リダイレクトには追従しません。

## 配信と再送

ref の CAS と同じ SQLite トランザクションで記録する `ref_log`／`git_ref_log` を
outbox として使います。endpoint ID・repo・ログ種別ごとに cursor と未確定本文を永続化し、
送信前に本文を確定します。HTTP 2xx で cursor を進め、それ以外は再送します。
ネイティブ branch と Git tag のログは別々の順序付きキューです。
`HEAD` のシンボリックな変更は push イベントを送らず cursor だけ進めます。

`X-GitHub-Event: push`、安定した UUIDv8 の `X-GitHub-Delivery`、
署名を設定した場合は `X-Hub-Signature-256: sha256=...` を付けます。
応答喪失や再起動でも未確定本文と delivery ID を再利用します。
受信側には重複排除が必要です。同じキューに複数 worker を接続しても cursor は巻き戻りませんが、
同時送信による重複は発生し得ます。

新しい endpoint ID はログ先頭から送信します。ID を維持すると再起動後も配信位置を引き継ぎます。
URL・鍵の変更は再起動後の再送にも適用しますが、すでに確定した本文は変えません。
停止中の endpoint は他の endpoint を止めません。同じキューの後続は成功まで待ちます。
サーバー終了時の未確定配信は次回起動時に再送します。

## 本文と上限

作成・更新・force・削除、旧／新 OID、commit、第一親との差分パスを含めます。
最後の commit には旧 ref から新 ref への最終差分も反映し、force rewind や削除でも
パスによる refresh 判定を可能にします。commit 配列は最大2,048件で、総件数と省略フラグを別に含めます。
非 UTF-8 の文字列は標準フィールドでは置換し、`kasu` 拡張に元のバイト列を base64 で保存します。

| 設定 | 既定値 |
|---|---:|
| `poll_interval_ms` | 100 |
| `timeout_ms` | 5,000 |
| `retry_base_ms` | 100 |
| `retry_max_ms` | 60,000 |
| `max_payload_bytes` | 26,214,400（25 MiB） |

再送は決定的 jitter 付き指数 backoff で、HTTP 試行終了後から待機します。
endpoint は最大32件、鍵は1〜65,536バイトです。
本文上限超過は cursor を進めません。ログを確認し、設定上限を増やして再起動してください
（設定可能な最大値は128 MiB）。現実装はイベント生成時に到達履歴を列挙するため、
大規模履歴での生成時間とメモリ使用量の評価は継続課題です。

```sh
RUSTC_WRAPPER= cargo test -p kasu-store --test webhook --locked
RUSTC_WRAPPER= cargo test -p kasu-server --test webhook --locked
```

RFC 4231 の署名、再オープン・独立接続、HTTP 500・timeout 後の同一本文／ID、
redirect 拒否、force／削除、本文上限と終了を検証します。
ネイティブ UpdateRef と実 Git clone→commit→push からの outbox 配信も検証します。
実コントローラーの検証は [kind E2E](gitops-e2e.md) の `--webhooks` を使います。
