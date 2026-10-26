# kasu 実装計画

kasu は Anthropic の社内 VCS **Scion**（参考: <https://www.anthropic.com/research/redesigning-source-control>）の
設計思想を踏襲したソース管理システムのクローンである。加えて、ArgoCD / Flux などの GitOps ツールが
そのまま接続できる **Git 互換レイヤー** を備える。

## ドキュメント構成

| ファイル | 内容 |
|---|---|
| [01-architecture.md](01-architecture.md) | 全体アーキテクチャ・コンポーネント・クレート構成 |
| [02-data-model.md](02-data-model.md) | Changeset / Tree / Blob / Ref / Derived Data のデータモデル |
| [03-protocols-and-api.md](03-protocols-and-api.md) | ネイティブ RPC・オブジェクト取得プロトコル・クライアント |
| [04-git-compat.md](04-git-compat.md) | Git 互換レイヤー（Smart HTTP, upload-pack/receive-pack, GitOps 対応） |
| [05-roadmap.md](05-roadmap.md) | マイルストーン・テスト戦略・リスク |
| [06-multi-repository.md](06-multi-repository.md) | 1 サーバーでの複数リポジトリ管理（レジストリ・隔離・権限・ライフサイクル） |

## ゴール

1. Scion の原則に従う、サーバー中心・水平スケール可能な VCS を Rust で実装する。
2. 既存の Git クライアント（`git` CLI, go-git）から **読み取り（clone/fetch/ls-remote）** できること。
   ArgoCD・Flux が無改造で同期できることを受け入れ基準とする。
3. GitOps の書き戻し（Argo CD Image Updater, Flux Image Automation 等）向けに、限定的な **push** をサポートする。
4. 全てのリポジトリ問い合わせ（diff, blame, path history, merge-base, ancestry）を API として公開する。
5. 1 つのサーバー（クラスタ）で **複数リポジトリ** をホストする。リポジトリ間で権限・ref・Git OID 空間・設定を分離しつつ、
   不変オブジェクトはストレージ上で共有する（06）。

## 非ゴール

- Git の完全な代替（オフライン開発・分散レプリケーション・任意の ref 運用）。
- 独自のコードレビュー UI（Scion における Antique 相当）。API フックのみ提供する。
- Git LFS・submodule 内容の追跡（gitlink エントリ自体は往復一致のため表現する。LFS はロードマップ後半で検討）。
- 初期段階での Spanner 級グローバル分散 DB。ストレージは trait で抽象化し、段階的に差し替える。

## Scion から踏襲する原則

| Scion の原則 | kasu での扱い |
|---|---|
| クライアントは常時オンライン | クライアントは必要なオブジェクトのみ取得。有界ローカルキャッシュ。 |
| クライアント/サーバーの非対称性 | commit / merge / rebase / blame / diff はサーバーで実行。 |
| 包括的なソース管理サービス | クライアントが使う問い合わせは全て公開 RPC（gRPC + HTTP/JSON）。 |
| 全て水平スケール | サーバーはステートレス。データは content-addressed で、どのノードもキャッシュ可能。packfile なし・メンテナンス不要。 |
| 安価・増分・ロックフリーな commit | Changeset は「親との差分」を記録する不変レコード。ref の移動は単一行 CAS で分離。 |
| Working copy は任意・スパース前提 | スパースチェックアウトが基本。仮想化チェックアウト（FUSE）は後半フェーズ。 |
| Derived Data | 親の値から再帰的に計算する汎用フレームワーク。eager / lazy を型ごとに選択。 |
| 階層キャッシュ | `kasu-daemon`（ノード）→ `kasu-cache`（リージョン）→ `kasu-server`（権威）。全階層が同一 RPC を話す。 |

## Git 互換レイヤーの位置づけ

Git 互換レイヤーは **ネイティブモデルの上に載る射影（projection）** であり、ネイティブ設計を歪めない。

- Git の tree / commit オブジェクトとその OID は Derived Data として増分計算する。
- ネイティブストアに packfile は存在しない。pack は要求ごとにストリーム生成する派生成果物で、キャッシュは有界・使い捨て。
- Git から取り込み/push されたオブジェクトは Changeset に変換しつつ raw オブジェクトも保存し、OID のバイト単位往復一致を保証する。
  再現できない入力は明示的に拒否する。

詳細は [04-git-compat.md](04-git-compat.md) を参照。
