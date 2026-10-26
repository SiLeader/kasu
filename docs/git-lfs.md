# Git LFS の評価

G6 の LFS 検討では、外部 LFS サービスを併用する構成を実クライアントで検証しました。
Kasu は LFS ポインターを通常の blob としてロスレスに保存し、実体は別サービスで管理します。
この構成で clone／push は動きますが、Kasu 自体の LFS endpoint と native の透過的な実体取得は実装しません。
`docs/plan/README.md` の「LFS 内容の追跡は非ゴール」という方針を継続します。

## 保存と取得

[Git LFS の仕様](https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md) では、Git オブジェクトに
SHA-256 とサイズを持つポインターを保存し、clean／smudge filter で実体との変換を行います。
LFS の SHA-256 は Git の SHA-1 や Kasu の BlobId とは別の識別子です。
Kasu の Git 取り込み・push・Metadata ReadFile・Objects RPC は、ポインターの bytes を扱います。
LFS の実体を native blob に置換すると Git の OID 往復を壊すため、自動展開しません。

```text
Git client -- Git pack(pointer) --> Kasu native storage
           -- LFS upload/data --> external LFS service
Git client <-- Git pack(pointer) -- Kasu native storage
           <-- LFS download/data -- external LFS service
```

`.gitattributes` と root の `.lfsconfig` を履歴に含めるか、クライアントに別途 `lfs.url` を設定します。
[Git LFS の設定仕様](https://github.com/git-lfs/git-lfs/blob/main/docs/man/git-lfs-config.adoc) により、
通常の Git config は `.lfsconfig` より優先します。

```sh
git lfs install
git lfs track '*.bin'
git config -f .lfsconfig lfs.url https://lfs.example.test/team/repo
git add .gitattributes .lfsconfig asset.bin
git commit -m 'Track external asset'
git push origin main
```

既存 Git repository を Kasu に取り込む場合、`kasu-ingest` が移すのはポインターまでです。
実体は元の LFS サービスに保持するか、移転元で実体を取得したうえで移転先へ転送します。
この評価では source に実体を持った状態で `git lfs push --all origin` を実行しました。

Kasu の `repo.git/info/lfs/objects/batch` は404を返します。
[Batch API](https://github.com/git-lfs/git-lfs/blob/main/docs/api/batch.md) の upload／download は
外部サービスへ送ります。Git の接続資格情報と LFS サービスの資格情報・認可は別途設定します。
今回の peer は loopback の無認証テスト専用で、外部サービスの認証・TLS・本番運用は未検証です。
Git SSH の forced command は `git-lfs-authenticate`／`git-lfs-transfer` を受け付けません。
SSH remote と LFS を併用する場合も外部 HTTP(S) endpoint の明示が必要ですが、
今回の LFS E2E は Git Smart HTTP が対象で、SSH＋LFS の組み合わせは未検証です。

## 実通信の結果

固定 Rust 1.90 の Linux arm64 container、Git 2.39.5、Git LFS 3.3.0
（Debian `3.3.0-1+deb12u2`）で検証しました。2 MiB の決定的なバイナリを使っています。

| 検証 | 結果 |
|---|---|
| native import → LFS 実体なしで clone | checkout が失敗。Git ポインター履歴は strict fsck が通過 |
| 外部 LFS へ既存実体を upload | SHA-256 と byte 全体が一致 |
| full／depth1／blob:none clone | 3ケースとも実体を取得し、内容一致・Git fsck・LFS fsck が通過 |
| 実体を変更して通常の Git push | pre-push が実体を upload、Kasu は新しいポインターを公開 |
| shallow client の pull | 新しい実体へ更新、内容一致・両 fsck が通過 |
| Metadata ReadFile | 最新ポインターと byte 単位で一致 |
| pre-push を省略した実体なしポインターの push | Kasu は Git 履歴として受理。外部実体の存在保証はしない |
| LFS サービスが503を返す状態 | skip-smudge clone と Git fsck は成功、LFS pull は失敗 |
| Kasu の LFS batch URL | 404。内蔵 LFS を装わない |

2,097,152 bytes の実体に対して初期ポインターは132 bytes です。
これは Git blob と LFS データのサイズ比較で、HTTP／pack 圧縮・ヘッダー・再送を含む帯域ベンチではありません。
外部 peer は各 upload の SHA-256 を検証し、実体2個の upload と clone／pull の download を記録します。
[結果・要求記録・ソースハッシュ](../crates/kasu-git/tests/fixtures/clients/lfs/) を保存しています。
`.lfsconfig` に動的な fixture port が入るため、Git commit OID は実行ごとに変わります。
実体の SHA-256 とポインターは固定です。

## 採用判断と制約

GitOps のマニフェスト配信と native の metadata 中心操作には、外部 LFS を必須にしません。
既存 LFS repository を使う利用者向けには、ポインターのロスレスな保存と外部サービスを使う方法を提供します。
この評価は外部 LFS 製品やクラウドサービスの選定を行うものではありません。

- Git ref の CAS と外部 LFS upload は単一 transaction になりません。upload 後の Git push 拒否は孤立実体を残し得ます。
- Git の到達可能性・strict fsck は LFS 実体の存在を保証しません。pre-push の省略、native API によるポインター作成、外部データの削除で実体を失えます。
- native CLI の checkout／diff／merge はポインターを扱います。LFS lock・実体の意味的 merge・透過的 smudge はありません。
- Kasu のパス ACL／Git 射影は外部 LFS 実体のアクセス制御へ自動反映されません。外部側で別途認可します。
- パス射影で root の `.lfsconfig`／`.gitattributes` が除外される場合、通常 clone と同じ LFS 設定にはなりません。この組み合わせは未検証です。
- 外部ストアの GC・バックアップ・容量・課金・Range／再開・巨大ファイル性能は評価していません。Kasu のバックアップだけでは実体を復元できません。

内蔵 LFS を導入するなら、Batch／Basic 転送だけでなく repo・ref・射影の認可、native と Git の
ポインター整合性、外部 upload と ref 公開の順序、保持／GC・バックアップを設計する必要があります。
現在はその実装を追加する根拠がなく、LFS 内容を追跡する非ゴールを変更しません。

## 再実行

```sh
docker build -f tools/lfs/Dockerfile -t kasu:lfs-test .
mkdir -p /tmp/kasu-lfs-evidence
docker run --rm --network none \
  --mount type=bind,source=/tmp/kasu-lfs-evidence,target=/evidence \
  kasu:lfs-test
python3 tools/lfs/record.py /tmp/kasu-lfs-evidence
```

`tools/lfs/run.py` は実 `kasu-ingest`／`kasu-server`／`git-lfs` を起動します。
別スレッドの in-memory LFS peer は batch と Basic upload／download に必要な fixture だけを実装し、
製品用の LFS サーバーではありません。プロセスの期限と停止処理を持ち、コンテナ外への接続はありません。
[CI](../.github/workflows/lfs.yml) も同じ手順を実行します。GitHub 上での実行結果は未確認です。
