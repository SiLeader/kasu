# kind 上の GitOps 受け入れ検証

`tools/gitops/run.py` は専用 kind クラスターで kasu、ArgoCD、Flux を起動します。
バージョンと公式インストールマニフェストの SHA-256 は
[`versions.json`](../tools/gitops/versions.json) に固定しています。

Docker、Git、Python 3、kind v0.33.0、kubectl v1.37.0 が必要です。
Docker にはコントローラーを含めた十分なメモリとディスクを割り当ててください。
初回は Rust ビルドとコンテナイメージの取得が発生します。

```sh
python3 tools/gitops/run.py --workdir /tmp/kasu-gitops-e2e
```

既存クラスターと同じ名前の場合は停止します。kubeconfig は workdir 内に作成し、
すべての kubectl 呼び出しで明示します。通常成功時は専用クラスターを削除し、
失敗時と `--keep` 指定時は調査のため残します。
このテストが作成した環境を再利用する場合のみ、同じ workdir と `--reuse` を指定できます。
`--skip-build` はビルド済み `kasu:gitops-e2e` イメージを使用します。

`--webhooks` は ArgoCD／Flux の定期ポーリングを1時間に延ばし、署名付き
GitHub push webhook を ArgoCD `/api/webhook` と Flux Receiver へ送ります。
初回同期後の ref 更新では手動 refresh を使わず、60秒以内の新 SHA／ConfigMap の反映、
両 endpoint の2xx、ArgoCD の webhook refresh ログ、Flux の reconcile 要求更新を検査します。
`evidence/webhook-proof.json` に検証結果を保存します。

## Image Updater／Image Automation の書き戻し

webhook 検証後の専用クラスターを `--keep` で残し、同じ workdir で実行します。

```sh
python3 tools/gitops/run.py --workdir /tmp/kasu-gitops-e2e --keep --webhooks
python3 tools/gitops/writeback.py --workdir /tmp/kasu-gitops-e2e
```

固定した Argo CD Image Updater v1.4.0 と Flux image controllers を使います。
クラスター内に digest 固定の OCI registry を起動し、固定 BusyBox イメージの
タグ `1.0.0` を登録します。初期 manifest を Git push で kasu に保存してから、
`1.0.1` を追加します。Image Updater は Kustomization、Flux は setter コメントのある
Deployment を変更し、それぞれ実際に commit／push します。
Image Updater は ArgoCD repository credentials を再利用します
（[固定版の明示 Secret 認証経路](https://github.com/argoproj-labs/argocd-image-updater/blob/v1.4.0/pkg/argocd/gitcreds.go#L142) は HTTP URL を受け付けません）。

クライアントが作る commit は初期 fixture だけです。その後の Git 履歴から両 bot の commit、
更新ファイル、`git fsck --strict`、Metadata API でのネイティブ内容一致、webhook の配信確定、両 Deployment の新しい image、
ArgoCD の同期 SHA を照合し、`evidence/writeback-result.json` に保存します。
Deployment は `replicas: 0` とし、検証対象は Git 書き戻しと Kubernetes manifest の反映です。
定期ポーリングは1時間で、タグ追加後に手動 refresh は行いません。
Docker が VM 内で動く場合も使えるよう、イメージの登録はホストから OCI HTTP で行います。

`writeback-client` をすでに作成した workdir は再実行時に拒否します。
失敗した環境を調査後、専用クラスターと新しい workdir で全手順を再実行してください。

検証は次の順序で行います。

1. 小さな Git fixture を kasu のネイティブストアへ取り込みます。
   Git リポジトリは init container の一時領域に置き、配信プロセスには渡しません。
2. HTTP Basic のテスト用トークンで ArgoCD Application と Flux GitRepository／Kustomization を設定します。
3. 両コントローラーの状態、コミット SHA、作成された ConfigMap の内容を照合します。
4. fixture を更新し、稼働中の kasu と共有するストアへ再取り込みして ref を進めます。
5. 両コントローラーが新 SHA と新 ConfigMap を反映したことを同じ条件で照合します。

`<workdir>/evidence/` に各同期時の Kubernetes リソース、バージョン、実イメージ ID、
イベント、コントローラーログ、Git pkt-line を保存します。
`result.json` の `success: true` は両段階の成功を意味します。
これはテスト専用 fixture とトークンを使う環境です。kubeconfig と Secret は成果物に含めません。
pack 本文と HTTP 認証ヘッダーは通信ログに含めません。

夜間・手動実行用の [`gitops.yml`](../.github/workflows/gitops.yml) は
同じランナーを Linux amd64 で実行し、失敗時も evidence をアップロードします。
GitHub 上の実行結果は別途確認が必要です。

## パス射影 URL

```sh
python3 tools/gitops/projection.py --workdir /tmp/kasu-projection-gitops-e2e
python3 tools/gitops/record.py --evidence /tmp/kasu-projection-gitops-e2e/evidence \
  --output crates/kasu-git/tests/fixtures/clients/path-projection-kind
```

専用クラスター `kasu-projection-e2e` を作り、両コントローラーの repository URL を
`http://kasu.gitops-test.svc.cluster.local:50080/demo/-/path/deploy.git` に設定します。
元リポジトリの `deploy/argo` と `deploy/flux` が、それぞれ射影内の `argo` と `flux` になります。
射影外には別のファイルを置きます。

初回同期、射影外だけの更新、射影内の ConfigMap 更新の3段階を検査します。
各段階で実 Git クライアントが clone／fetch し、全ファイルの mode・OID・パスと
元リポジトリの `deploy/` を照合し、`git fsck --strict` を実行します。
元 SHA と異なる射影 SHA に両者が同期すること、射影外更新では元 SHA だけが変わること、
射影内更新では新しい射影 SHA と ConfigMap の値が両者に反映されることを確認します。
明示 refresh の受理は ArgoCD の `reconciledAt` と Flux の `lastHandledReconcileAt` でも確認します。

`evidence/projection-{one,outside,two}.json` に Git の照合結果、
`sync-{one,outside,two}.json` に両コントローラーと ConfigMap の状態、
`result.json` に元／射影 SHA と refresh の受理記録を保存します。
このテストは明示 refresh を使い、webhook の検証は通常 URL のシナリオで行います。
夜間・手動 CI では通常 URL の書き戻しとパス射影を別の matrix job で実行します。

2026-10-24（JST）、macOS arm64 の Docker／kind 上でこの3段階が通過しました。
元 SHA は `8b2e4029…` → `56f1f25e…` → `7638c1d2…` と進み、射影 SHA は
`e744c005…` → `e744c005…` → `1f2f1220…` になりました。
両更新の refresh 受理、両コントローラーの同期と ConfigMap、全3段階の subtree 一致／strict fsck を
[結果と実イメージ ID](../crates/kasu-git/tests/fixtures/clients/path-projection-kind/gitops-kind-summary.json)、
[ArgoCD の通信](../crates/kasu-git/tests/fixtures/clients/path-projection-kind/argocd-3.5.6-kind.trace)、
[サーバー側の通信](../crates/kasu-git/tests/fixtures/clients/path-projection-kind/gitops-kind-server.trace) に保存しています。
この追加シナリオの GitHub 上の実行結果は未確認です。

## 2026-10-23 の実測

macOS arm64 の Docker 上に kind v0.33.0／Kubernetes v1.37.0 の専用クラスターを作成し、
ArgoCD v3.5.6 と Flux v2.9.6 の両段階の同期が通過しました。
署名付き webhook の再検証でも、手動 refresh なしで更新後5.602秒以内に両者が同期しました。
ポーリング間隔は1時間、ArgoCD の webhook refresh 数は1から2へ増加し、
Flux Receiver の reconcile 要求更新とネイティブログ sequence 2 の配信確定も確認しました。
続くクリーン環境の書き戻し E2E も通過しました。タグ追加から22.486秒で
Image Updater／Flux Automation の両 commit と push、Deployment image 更新、
ネイティブ内容一致、strict fsck、sequence 5 の webhook 配信確定を確認しました。
[書き戻し結果](../crates/kasu-git/tests/fixtures/clients/gitops-kind-writeback.json) に
実 commit SHA とネイティブ Changeset ID を保存しています。Image Updater コンテナの Git は2.54.0です。
ArgoCD コンテナの Git は 2.53.0、Flux source-controller／kustomize-controller は v1.9.6 です。
Flux の [固定ソースの go.mod](https://github.com/fluxcd/source-controller/blob/v1.9.6/go.mod) は
go-git v5.19.1 を指定しています。通信上の agent は `go-git/5.x` です。

初回 SHA は `60be29eea4594d7b543fcfa4a70935093f70a20c`、
更新 SHA は `6e1773d9c0ad7af349d31c76b3f09a09ed15721b` でした。
[結果と実イメージ ID](../crates/kasu-git/tests/fixtures/clients/gitops-kind-summary.json)、
[ArgoCD の通信](../crates/kasu-git/tests/fixtures/clients/argocd-3.5.6-kind.trace)、
[両コントローラーを含むサーバー側通信](../crates/kasu-git/tests/fixtures/clients/gitops-kind-server.trace) を保存しています。
時刻を除いた fixture の再生成:

```sh
python3 tools/gitops/record.py --evidence /tmp/kasu-gitops-e2e/evidence \
  --output crates/kasu-git/tests/fixtures/clients
```
