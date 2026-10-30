# Web UI 仕様書兼実装計画

kasu のリポジトリをブラウザから閲覧・編集・管理し、変更をレビューして着地させるための Web UI（`kasu-web`）の
仕様と実装計画である。ネイティブ API（gRPC）の上に載るクライアントの 1 つとして実装し、権威は常に kasu-server に置く。

## 位置づけ

- kasu-web は **独立プロセス** で、kasu-server と gRPC で通信する。kasu-server 内部の型・ストアを直接使わない。
  これにより「クライアントが使う問い合わせは全て公開 RPC」という原則（[README](../README.md)）を Web UI にも適用する。
- 画面は **Rust のサーバーサイドレンダリング** で生成し、JavaScript は段階的強化（progressive enhancement）用の
  小さな自前スクリプトに限る。Node / npm はビルド要件に加えない。
- レビュー機能のデータと承認判定は kasu-server の新しい **Review API** に置く。kasu-web はステートレスのまま保つ。
- 本計画により、[設計計画の非ゴール](../README.md#非ゴール)「独自のコードレビュー UI」を撤回し、スコープに含める。

## 確定した決定事項

| 項目 | 決定 |
|---|---|
| 機能範囲 | 閲覧、リポジトリ管理、Web 上での書き込み、レビュー |
| 配置 | 独立プロセス `kasu-web`（新クレート `crates/kasu-web`）。kasu-server とは gRPC |
| フロントエンド | Rust SSR（テンプレート）＋少量の自前 JS。JS なしでも主要操作が完結する |
| 認証 | PAT ログイン → 署名付きセッション Cookie、リバースプロキシの信頼ヘッダー、匿名閲覧 |
| 権限の伝達 | kasu-web は **委譲資格情報**（delegate）で kasu-server に接続し、要求ごとに代理対象アカウントを指定する |
| レビューの保存先 | kasu-server の `kasu.v1.Review` サービス。MetaStore に保存し、CLI / bot も同じ API を使える |
| 承認の強制 | サーバーで強制。保護 ref への Land / UpdateRef / git push は承認済みレビューの Submit 経由に限る |
| レビュー単位 | base に対する Changeset スタック。更新ごとにリビジョンを残す |

## 本書で置いた既定（変更可能）

確定事項から導いた詳細設計上の既定値。異論があれば各章を修正する。

| 項目 | 既定 | 章 |
|---|---|---|
| URL 体系 | `/<repo>/-/<view>/<rev>/<path>`（GitLab 型）。`-` 単独セグメントは名前規則で予約済み | [03](03-screens.md#url-体系) |
| UI 言語 | 英語と日本語（`Accept-Language` と利用者設定で切替） | [03](03-screens.md#共通の表示規則) |
| 構文ハイライト | サーバー側で `syntect`（pure Rust の正規表現バックエンド）。サイズ上限超過時は無効 | [01](01-architecture.md#レンダリング) |
| Markdown | `pulldown-cmark` ＋ `ammonia` によるサニタイズ。README・レビュー本文・コメントに使用 | [02](02-auth.md#リポジトリ内容の安全な表示) |
| セッション失効 | PAT の鍵付き指紋をセッションに結び付け、PAT 更新で既存セッションを無効化。最長 12 時間 | [02](02-auth.md#pat-ログインとセッション) |
| 管理権限 | アカウント grant に `admin`、アカウントに `create_repositories` を追加（運用者 PAT 以外にも管理を委ねる） | [02](02-auth.md#管理権限の拡張) |
| ディレクトリ限定アカウントとレビュー | `read_directories` を持つアカウントは初期版では Review API を利用不可 | [04](04-review.md#可視性と権限) |
| スタックの着地方式 | `REBASE`（各 Changeset を保持）と `SQUASH` を選択可能。ref の CAS は 1 回 | [04](04-review.md#submit) |
| 承認の失効 | 新リビジョンで失効。ただし変更内容の指紋が同一（trivial rebase）なら維持 | [04](04-review.md#承認の判定) |

## ドキュメント構成

| ファイル | 内容 |
|---|---|
| [01-architecture.md](01-architecture.md) | プロセス構成、クレート構成、kasu-server との通信、レンダリング、キャッシュ、設定 |
| [02-auth.md](02-auth.md) | 認証方式、委譲資格情報、Identity API、管理権限、CSRF、セキュリティヘッダー |
| [03-screens.md](03-screens.md) | URL 体系、画面一覧と各画面の仕様（閲覧・書き込み・管理） |
| [04-review.md](04-review.md) | レビューのデータモデル、ポリシー、承認判定と強制、Submit、Review API |
| [05-roadmap.md](05-roadmap.md) | フェーズ分割、完了条件、テスト戦略、リスク |

## ゴール

1. 権限のあるリポジトリの ref・tree・ファイル・履歴・diff・blame をブラウザで閲覧できる。
   非 UTF-8 パス・バイナリ・symlink・gitlink・`read_filtered` ビューを含め、ネイティブモデルを正しく表示する。
2. ブラウザからファイルを編集・追加・削除して Changeset を作成し、Land またはレビュー作成まで完結できる。
3. Admin API の操作（作成・設定・状態変更・改名・削除・フォーク・使用量）を、権限のある利用者が画面から行える。
4. Changeset スタックのレビュー（リビジョン、行コメント、投票、承認条件、Submit）を提供し、
   保護 ref への着地をサーバー側で承認済みレビューに限定できる。
5. 既存の隔離規則（不可視リポジトリ・未所属オブジェクトは `NotFound`）を UI のあらゆる経路で維持する。

## 非ゴール

- Issue トラッカー、Wiki、プロジェクト管理機能。
- コード検索（全文索引が必要。将来検討、[05](05-roadmap.md#w7-拡張)）。
- kasu-web 自身による OIDC / SAML 実装。SSO はリバースプロキシ（oauth2-proxy 等）に任せる。
- ブラウザ上での競合解消エディタ。競合はネイティブ CLI で解消し、新リビジョンとして送る。
- SPA 化、モバイルアプリ、リアルタイム共同編集。
- メール等の通知配信（レビューイベントのログは保存し、Webhook 配信は将来フェーズ）。
