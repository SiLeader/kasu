# 実装状況

[English](implementation-status.md) | 日本語

`docs/plan` 全体が実装対象です。このファイルは現在の到達点と残作業を記録するもので、
元の計画や完了条件を縮小しません。

[計画との対応表](plan-audit.json) に、元の6文書の hash と36項目の実装箇所・検証記録をまとめています。
`implemented` は実装の所在を示し、すべての外部受け入れ試験を現在のビルドで再実行済みという意味ではありません。
各外部試験の適用範囲・実行時ソースはリンク先の記録を参照してください。

ユーザーの「セキュリティ検証が必要なことは一旦スキップして構わない」という指示に従い、
OIDC と異なる権限 scope 間の共有 cache は未実装の保留項目としています。
fuzz campaign・依存脆弱性の評価・第三者 security review も未検証です。
PAT／ACL と通常の不正入力回帰テストは実装済みですが、セキュリティ承認を示すものではありません。

2026-10-26 の最終回帰検証は、全 workspace・全 target・全 feature で214件成功、21件は外部 fixture／
サービス・特権環境・子プロセス fixture のため通常実行では除外、Criterion スモーク11件成功でした。
fmt／Clippy（警告をエラー化）／actionlint／cargo-deny の bans・licenses・sources も通過しています。
[実行条件・ログ・ソース hash](verification/plan-regression/summary.json) を保存しました。
除外された試験の個別実行と適用範囲は、この記録と各マイルストーンの受け入れ記録を参照してください。

## 現在のコード

| クレート | 実装済み機能 | 検証 |
|---|---|---|
| `kasu-core` | BLAKE3 の型付き ID、Git SHA-1、バイト列パス／署名、File／Symlink／Gitlink、Changeset と GitProvenance、Tree、BlobMetadata、v1 canonical codec | エンコードと ID のゴールデン、非 UTF-8・全 Entry・provenance の proptest、壊れた入力の拒否 |
| `kasu-store` | MetaStore／BlobStore／RefStore、SQLite 不変 KV、zstd FS オブジェクト、CAS と ref_log の原子追記、複数 ref CAS、削除後も単調な ref version、endpoint ごとの永続 webhook cursor／未確定本文 | 再オープン、衝突時の上書き拒否、バッチ失敗時のロールバック、ABA 拒否、独立した 16 接続の競合 |
| `kasu-store` の `production` feature | PostgreSQL の不変 KV・混合 ref CAS・commit 順ログカウンター・Land 冪等結果・webhook、S3 conditional put と圧縮 frame 不変保存、TLS と明示的な平文許可、接続再確立、server／ingest の外部ストア設定 | macOS arm64／固定 Rust 1.90 Linux arm64 の実 PostgreSQL 18・S3 互換 SeaweedFS 4.45、16接続の CAS と PUT、8接続の Land、SQL 障害と順序逆転防止、TLS／不正 CA・資格情報の拒否、2つの実 server プロセスと実 ingest／Git clone・atomic push |
| `kasu-derived` | SubKey／バージョン／型間依存／親選択／eager keys、通常依存後の追加依存にも対応する反復スケジューラ、singleflight、RootTree／Generation／FirstParentSkip@2（固定長 eager、旧 @1 は遅延互換）、PathHistory／Blame@2／MovedBlame／CopyBlame@2／LineMatches／CopyDetect／内容 ID の PathIndex（Lazy）、変更パスだけを再生成する Merkle tree、apply／diff／lookup | 1 万件の反復計算、skip pointer の距離、構造共有、tree 差分の往復、32 同時要求の単一計算、Lazy の部分キー、版の共存、循環検出、登録型だけの cache hit／計算成功・失敗／遅延計測 |
| `kasu-ops` | Changeset 作成時の親・blob・Delete・tree 衝突検証、全親 ancestry、複数の best merge-base、cursor 付きログ、ReadFile／ListDir、パス／内容 diff、行単位3-way merge／反復 tree merge、Changeset 単位 Merge／Rebase、全基底の仮想統合、構造化競合 | ランダム DAG の推移閉包との照合、第二親経由の祖先、criss-cross の2個／3個の best base と統合競合、144通りの行編集を Git merge-file と比較、バイト列保持・削除・型／directory 衝突、再適用時の親・署名・内容 |
| `kasu-git` | repo 単位 OID 索引、raw オブジェクト、ProjectionId ごとの GitTree／GitCommit、PathCommit によるパス履歴・tag 射影、native Merkle node 共有、branch／tag／HEAD 射影、peel、非デルタ pack encoder、pkt-line／upload-pack 状態機械、公開 want 検査、shallow／common 集合、blob:none filter と promisor want、明示公開履歴の blob 認可、通常 fetch の永続 radix 到達索引と複数 want／common の正確な差分、zlib LRU、receive コマンド／report-status、上限付き gix pack／thin・ofs/ref delta 入力 | 合成 OID のゴールデン、Git 本体の strict pack／fsck、repo 間隔離、Git parser proptest、pack のサイズ／深さ／破損／任意バイト列、receive framing |
| `kasu-ingest` | gix で packed／loose Git またはメモリ上のオブジェクト供給元から反復取り込み、raw commit／非正規 tree／tag、接続性・型・再現性検証、CLI、branch／tag／HEAD とログの一括 CAS 公開 | 非 UTF-8／symlink／gitlink／merge／tag 連鎖の生成 fixture、native→Git→native の ID 維持、OSS 21 commit・70 object と GitLab の50 commit・183 object（非 UTF-8／gitlink／tag／14 merge）の全件往復、clone |
| `kasu-proto` | kasu.v1.Metadata の13 RPC、Writer の6 RPC（UploadBlobs は client streaming）、Objects の2 RPC（zstd frame を chunk stream）、バイト列を保持する型付き protobuf、tonic クライアント／サーバー生成 | protoc ビルド、gRPC ストリーム・単発 RPC の送受信 |
| `kasu-server` | Metadata／Writer の gRPC／HTTP JSON、永続ストアから起動する実行ファイル、Basic／Bearer、レジストリによる要求ごとのリポジトリ解決・grant・Changeset／blob 所属照合、[マルチリポジトリ](multi-repository_ja.md) の Admin API、blocking worker、Git Smart HTTP、有界ストリーム、receive-pack と能力別 atomic push、ref パターン／fast-forward／force／削除／最終 tree ACL、CLI の JSON ポリシー、永続 outbox と HMAC 付き webhook、[共有 admission／Prometheus](server-runtime_ja.md)、IP／PAT のレートと同時数・待機数制限 | Git のログ／Blame／全 merge-base／diff との比較、TCP 実通信、HTTP JSON、実行ファイル起動、認証・repo 分離・cursor・Lazy SubKey、Write API→Git clone／strict fsck、CAS／ABA、Git／go-git の通常・shallow push、atomic 混合 ref の成功／全件拒否、未公開依存／最終 merge tree の拒否、署名／再送、実 Image Updater／Automation と webhook refresh、HTTP／gRPC の割当量共有、切断後の worker 保持・Objects／Git backpressure、有限 scrape 枠、ソケット IP、認証と低 cardinality |
| `kasu-cli` | Linux の FUSE／OverlayFS 仮想 checkout、スパース checkout、working copy 状態と排他ロック、mtime／size／inode と内容照合、notify、status／diff／累積 draft commit／rebase／Land／sparse／log／blame／show、zstd frame cache、ジャーナル復旧と symlink 親の保護 | macOS／Linux の実 CLI＋gRPC、要求 Tree／Blob 集合の照合、除外後の新 subtree、累積 commit・追加取り消し・型変換、競合／応答喪失後の再送、notify、Gitlink、非 UTF-8、中断復旧と外部編集の保護 |
| `kasu-object` | 共通 Objects クライアント、TCP／TLS／Unix socket、frame の構造・上限検証、階層 role とトークンの照合 | CLI／cache／daemon の実通信、直接接続の互換性、誤った role・資格情報の拒否 |
| `kasu-telemetry` | 明示的な OTLP HTTP/protobuf、有限 queue・送信／終了期限、sampler、W3C context、固定 span と結果、worker／stream の寿命 | macOS／固定 Rust 1.90 Linux の実5バイナリ・TCP／Unix socket、cold／warm の全親子関係、sampling・無効化・秘匿値・受信先遅延／停止／redirect、飽和 queue・scoped subscriber |
| `kasu-cache` | メモリ LRU、`(repo, id)` 所属を確認したヒットと任意のリポジトリ許可リスト、singleflight、fill／pending／stream 上限、上位 pacing と回路遮断、Objects gRPC、Prometheus metrics、共通ディスク／snapshot 実装、共有上限・永続 LRU・dirty intent 復旧 | 1,000クライアントを2ノードから集約、server 取得1回、cache 停止後の source fallback なし、異なる ID の rate／同時数、遮断復旧・キャンセル、破損 snapshot と型・repo の拒否、実バイナリ |
| `kasu-daemon` | ディスク Objects cache、所有者限定 Unix socket、二重起動ロック、stale socket 復旧、snapshot 起動・手動／定期 export と世代管理、容量上限・過大 frame の非保存、metrics | macOS／Linux の実バイナリと CLI、上位停止時の snapshot 起動、SIGKILL 後の復旧、socket 権限・置換保護 |
| `kasu-sim` | 本番 Land の prepare／commit を使う決定的イベントスケジュール、実 SQLite／FS、保存前失敗／保存後応答喪失、キャンセル、再送／再起動。paused Tokio と in-memory HTTP/2 上の本番 cache Layer／Objects、2 node DiskStore、cache 不通／不完全 stream／上位期限／保存失敗／probe／再オープン | 1,000クライアントの有限終了、受理した全変更の保存、同一キーの二重着地防止、全確定結果の再生、小規模 seed の再現性、実非同期 Land の期限／future 破棄 |

不変レコードは put-if-absent だけで保存し、ref の更新・ログ追記だけをトランザクション化します。
ネイティブの Changeset 作成は GitProvenance を受け付けません。取り込み側で raw Git オブジェクトと
接続性を検証してから保存します。取り込みコマンドは tracing subscriber を初期化します。
cache／daemon／権威サーバーの Prometheus メトリクスと、OTLP trace 出力・W3C context 伝搬は実装済みです。
collector の実運用評価と、永続 webhook を跨ぐ trace の連結は残作業です。

## 受け入れ条件と追加評価

| マイルストーン | 状態 | 検証記録・適用範囲・追加評価 |
|---|---|---|
| M0 | 基盤・ローカル検証済み | Git 2.54.0／go-git v5.19.3／ArgoCD v3.5.6／Flux v2.9.6／Image Updater v1.4.0 の固定と通信 fixture・matrix を追加。実行ファイルの tracing 初期化と cache／daemon metrics は追加済み。CI 設定は追加済みだが GitHub 上の実行結果は未検証。[権威サーバーの共有 admission／Prometheus](server-runtime_ja.md)、Derived Data の遅延・cache hit と ref CAS の計測を追加。[OTLP trace 出力と W3C 伝搬](telemetry_ja.md) を CLI／daemon／cache／server／ingest に実装し、実プロセスで検証。[Criterion の11ケース](benchmarks/criterion_ja.md) で Changeset 作成・PathHistory の cold／warm・pack の cold／warm throughput とサイズを両 OS 各20サンプルで計測。collector の実運用・大容量評価と非同期 workflow の trace 連結は残作業 |
| M1 | 基盤・固定参照コスト検証済み | [FirstParentSkip v2](benchmarks/first-parent-skip-v2_ja.md) を eager にし、v1 を旧キーで遅延読み取り／バックフィル用に維持。1万件の各作成で index get 7回以下・10／82 bytes、v1 参照0回を検査。macOS／固定 Rust 1.90 Linux の10万 Changeset・1万分岐合流で、全体の MetaStore get は平均約24.3回・最大27回、旧版との最終 Changeset／Tree ID 一致を検証。全5 node DAG の第一親判定と全親 ancestry を照合。初回移行時のバックフィルと、ストレージ内部のサイズ依存・実運用規模の評価は別途残る |
| M2 | 受け入れ往復検証済み | [Git 取り込み](git-import_ja.md) と固定 OSS・特殊入力 fixture の全 OID を検証。libgit2 の gitlink 6 object に加え、[固定 GitLab byte-path fixture](git-import_ja.md#固定-upstream-byte-path-fixture-の往復) の50 commit・14 merge・76 tree・56 blob・annotated tag 1件の全183 object を macOS／固定 Rust 1.90 Linux で往復。非 UTF-8 パスの追加・更新・削除、全親リスト、gitlink 3件と target 未取得、strict pack／fsck、refs／HEAD／clone の一致を確認。対象は固定2 ref の完全履歴で、upstream の他 ref は対象外。GitHub 上の夜間 CI 実行結果は未確認 |
| M3 | API・差分検証済み | [Metadata API](metadata-api_ja.md) を実装し、Git 本体との比較と実通信が通過。[rename／copy の Blame](blame-lineage_ja.md) を追加し、純粋／編集 rename・ファイル全体／行断片の copy・行移動・その後の編集を3モード・24通りで macOS Git 2.54.0／Linux Git 2.39.5 と比較。gRPC／HTTP JSON と実 CLI の両 flag が通過。非 UTF-8 元パス・第二親の断片 copy・並べ替えの proptest・候補／比較上限と未確定結果の非保存も検証。[MergeBase の frontier 終了条件](benchmarks/divergent-history_ja.md#mergebase-の探索終了条件) を追加し、全1,024個の5 node DAG・25,600 tip ペアの照合と、長い共通履歴上の短い分岐で32回以下の Metadata get を検証。[固定した実在 OSS の Metadata 検証](metadata-oss_ja.md) で全21 commit のパス履歴189件・Blame 77件・merge-base 全441組が両 OS で一致。Git ヒューリスティクス全体・任意の OSS 全般の parity は残作業 |
| M4 / G1 | 受け入れ検証済み | [Git 読み取り](git-read_ja.md) の Smart HTTP、全履歴／depth=1、認証、有界 zlib キャッシュを実装。Linux go-git checkout と strict pack／fsck が通過。[kind E2E](gitops-e2e_ja.md) で ArgoCD／Flux の初回同期と ref 更新後の再同期を検証。夜間 CI の GitHub 上での実行結果は未確認 |
| M5 / G2 | 連続 fetch 検証済み・帯域初回計測済み | have／ACK、増分 fetch、deepen／unshallow、時刻／ref 境界、SHA want 認可、include-tag を追加。v0／v1／v2・全履歴／shallow の6クライアントで、分岐・マージ・複数 ref の8回連続 fetch と毎回の strict fsck が通過。[固定 Flux サンプルの帯域計測](benchmarks/gitops-pack_ja.md) に基づき非デルタを継続。[永続到達索引](benchmarks/pack-reachability_ja.md) を追加し、ランダム DAG の全走査一致、変更 Merkle パスだけの読み取り、再オープン後の本文取得0回、両 OS の Git 互換テストと macOS の同じ固定ソースでの再計測を実施。shallow 列挙の最適化、索引の大規模バックフィル・容量・複数ノード負荷、大規模 repo での delta 評価は残る |
| M6 | 受け入れシミュレーション通過 | [行単位 diff3／Merge／Rebase／全基底の統合](native-merge_ja.md)、[Write API](write-api_ja.md)、[永続的冪等性・有限リトライ・キャンセルを持つ Land](land_ja.md) を実装。[1,000クライアントの記録](simulations/README_ja.md) で有限終了、lost update／二重着地なし、再起動後の全件再送を検証。夜間 CI の GitHub 上の実行結果は未確認。大規模な実通信負荷・横断的な障害網羅性は継続課題 |
| M7 / G3 | 受け入れ検証済み | [Git 書き込み](git-push_ja.md) に pack／展開量／delta 深さ上限、接続性・型検証、最終 tree ACL、ref パターン、force／削除ポリシーを実装。通常／shallow 書き戻し・tag・一部拒否を検証。[永続 webhook](webhooks_ja.md) は本文／ID の再送、署名、cursor を実装。kind 上で1時間ポーリング・手動 refresh なしの ArgoCD／Flux 更新を5.602秒で検証。固定 Image Updater v1.4.0／Flux image controllers v1.2.5 が実レジストリのタグ追加から22.486秒で commit・push・Deployment 更新。strict fsck／ネイティブ内容一致／sequence 5 配信確定を保存。夜間 CI の GitHub 実行結果は未確認 |
| M8 | 受け入れ E2E 検証済み | [Objects RPC](object-api_ja.md) の GetTrees／GetBlobs、repo 所属索引、保存 zstd の64 KiB chunk 配信、有界キュー、切断・エラー処理を実装。実 gRPC のスパース対象だけの要求、frame の一致、認可・上限・競合 Tree、索引の再オープンと変更 subtree だけの検証が通過。[ネイティブ CLI](native-cli_ja.md) の working copy／notify／主要コマンドと復旧処理を追加。macOS arm64／固定 Rust 1.90 の Linux arm64 で実 CLI のスパース要求集合、commit→rebase→Land、競合・応答喪失・再送を検証。daemon／共有 cache は M9 の対象。GitHub 上の新 macOS CI 実行結果は未確認 |
| M9 | 受け入れ負荷試験通過 | [cache／daemon 階層](cache-hierarchy_ja.md)、Unix socket、snapshot 起動・定期保存、role／専用資格情報による階層強制、singleflight・上位レート／同時数・待機数制限、回路遮断、metrics を実装。[実 RPC の1,000クライアント記録](simulations/README_ja.md#cache-階層の負荷試験) で2ノードから権威取得1回、cache 停止後の cold miss 拒否・権威負荷不変を検証。macOS arm64／固定 Rust 1.90 Linux arm64 の実 CLI・バイナリ・snapshot・強制終了復旧が通過。同一 UID 共有が現在の範囲（複数リポジトリの所属は 06 の行を参照）。[ディスク LRU／共有容量上限・snapshot 世代管理](disk-cache_ja.md) を追加し、並行 open／export、型を共有する frame、書き込み途中の SIGKILL 復旧、実 daemon の再取得・再起動を検証。[保存障害時の回路遮断](disk-cache_ja.md#保存障害時の再取得制御) を追加し、保存失敗後の再取得抑制・warm hit・probe の再失敗・復旧を両 OS で検証。[決定的 cache 障害シミュレーション](simulations/README_ja.md#決定的な-cache-階層障害) は4 seed・各2回・1,000クライアントで両 OS の全結果が一致。初回権威取得1回、cache 不通・再オープン時0回追加、仮想期限／probe と未完了 frame 非公開を検証。大容量・多 cache と実 filesystem の disk-full／電源断の評価は残作業。GitHub CI の実行結果は未確認 |
| M10 / G4 | PostgreSQL／S3・複数 server の実通信と TiKV 初期評価を検証済み | [Git atomic push](git-push_ja.md)、[PostgreSQL／S3](production-storage_ja.md) と起動設定を実装。macOS／固定 Rust 1.90 Linux の実サービスで TLS／認証、16接続の CAS／PUT、8接続の Land、ログ順序・SQL rollback・接続復旧、2つの実 server の Git／native 書き込み・再起動 replay を検証。[TiKV 評価](benchmarks/kv-evaluation_ja.md) は3 PD／3 TiKV、複数 region、CAS・冪等結果・rollback、データ／PD leader の SIGKILL、quorum 喪失の有限 Retry、復旧・全再起動後の全件照合が通過。サイズ上限・read fence・競合／復旧時間を記録し、既定は PostgreSQL／S3 を継続。TiKV の製品用 Rust adapter、接続 pool、大規模・同等 HA・実クラウド・災害復旧評価は今後の作業。FoundationDB は未評価。新しい CI の GitHub 上の結果は未確認 |
| M11+ | 計画の拡張機能・実通信検証済み | [partial clone](git-read_ja.md#partial-clone)、[パス射影](git-projection_ja.md)、[射影専用 PAT](projection-accounts_ja.md) を macOS Git 2.54.0／固定 Rust 1.90 Linux Git 2.39.5 の全 protocol で検証。clone／shallow／promisor fetch、tag、OID 分離、native API 拒否、資格情報分離が通過。[CopyDetect／PathIndex](blame-lineage_ja.md) は Lazy 実装と1,024回の rename・逆索引のプロパティ・第二親・比較上限・追加依存の循環を検証。[check／DAG bisect／revert bot](automation_ja.md) は Metadata-only の gate、144通りの原因探索、3-way inverse、journal 再開、Land 祖先条件、応答喪失後の replay を両 OS の実 API で検証。[射影 URL の kind E2E](gitops-e2e_ja.md#パス射影-url) では初回／射影外のみ／射影内更新の3段階で ArgoCD／Flux の同期・refresh 受理と実 Git の subtree 一致／strict fsck を検証。[Linux 仮想 checkout](virtual-checkout_ja.md) は blob の遅延取得、Metadata サイズ RPC、COW、既存 CLI の commit／rebase／Land、SIGKILL 復旧と busy unmount を実カーネルで検証。macOS の FSKit／NFS 調査も記録。[Git SSH](git-ssh_ja.md) を OpenSSH forced command で実装し、実公開鍵接続で Git CLI の全 protocol／shallow 増分 fetch／push と go-git の full／depth1 clone→push、strict fsck・native 内容を検証。[LFS 評価](git-lfs_ja.md) は固定 Git LFS 3.3.0 と実 Kasu で full／depth1／partial clone、外部 upload→Git push→shallow pull、native ポインター一致、実体欠落・外部503と skip-smudge の挙動を検証。内蔵 LFS は追加せず内容追跡の非ゴールを継続。[アカウント別書き込み ACL](repository-accounts_ja.md) を Git HTTP／SSH と native Writer に実装し、global policy との積・最終 tree・並行要求・アカウント別 Land 再送を検証。[native のディレクトリ読み取り](directory-read-acl_ja.md) を canonical Tree のビュー・scope ごとの永続 Objects 索引・redacted Metadata 応答で実装。実 CLI の checkout→commit→Land と非公開内容の保持、ID 指定と blob コピーの拒否、権限変更・再起動を検証。その他の filter、macOS の仮想 checkout adapter、大規模プロファイル・索引／DAG 評価、実 CI／デプロイ接続は残作業 |
| 06 マルチリポジトリ | 実装・単体／プロセス・kind E2E 検証済み | [マルチリポジトリ](multi-repository_ja.md) のレジストリ（SQLite／PostgreSQL、名前・alias・変更ログ、ref Tx 内の状態再確認）、`RepoKey` と legacy キー移行（`migrate-registry`・互換登録）、要求ごとの解決と `NotFound` への統一、Git HTTP／SSH の複数セグメント名、リポジトリ単位の WritePolicy／webhook／admission・宣言ファイル同期、grant（glob・最具体一致・改名後の維持・旧形式の読み替え）、射影アカウントと hop 資格情報の範囲、Admin API（作成・一覧・CAS 更新・改名・削除と purge・フォーク・論理サイズ）、cache／daemon の `(repo, id)` 所属と snapshot、CLI の `clone`／`repo`、`kasu-ingest --create` を実装。Linux x86_64 で全 workspace テストと実 PostgreSQL 17 のレジストリ契約が通過。隔離のプロパティテスト、thin pack 基底・UploadBlobs の存在オラクル、削除と並行する Land、公平性の kasu-sim、実 server バイナリでの宣言ファイル・署名付き webhook・`git clone` を検証。[kind の複数リポジトリ E2E](gitops-e2e_ja.md#複数リポジトリ) は ArgoCD v3.5.6／Flux v2.9.6・1時間ポーリングで、各リポジトリの更新が webhook だけで約5.7秒後に当該リポジトリにのみ同期することを検証（固定版でない kind v0.29.0／Kubernetes v1.33.1）。共有不変オブジェクトの物理回収は計画どおり対象外 |

保留中の OIDC／scope を保つ共有 cache と fuzz に加え、運用上の追加評価として collector・非同期 workflow の trace 連結、
クラスタ全体の割当量・容量、障害マトリクスの拡充、GitHub 上の夜間ジョブ実行確認が残っています。
kind の GitOps E2E 自体は実行済みで、同期・書き戻し・webhook・パス射影の記録を上表から参照できます。
Git の capability は、実装と互換性検証が揃うまでは広告しません。
