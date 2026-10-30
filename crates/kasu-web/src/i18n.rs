//! English and Japanese messages, embedded at compile time. Keys are the
//! English text; a missing entry shows the key.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Lang {
    #[default]
    En,
    Ja,
}
impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ja => "ja",
        }
    }
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "en" => Some(Self::En),
            "ja" => Some(Self::Ja),
            _ => None,
        }
    }
    /// The first supported language of an Accept-Language header.
    pub fn negotiate(header: &str) -> Self {
        let mut best = (0.0f32, Self::En);
        for item in header.split(',') {
            let mut parts = item.trim().split(';');
            let tag = parts.next().unwrap_or_default().to_ascii_lowercase();
            let quality = parts
                .find_map(|p| p.trim().strip_prefix("q=").and_then(|q| q.parse().ok()))
                .unwrap_or(1.0);
            let lang = match tag.split('-').next() {
                Some("ja") => Self::Ja,
                Some("en") => Self::En,
                _ => continue,
            };
            if quality > best.0 {
                best = (quality, lang);
            }
        }
        best.1
    }
}
/// Translator handed to templates.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tr(pub Lang);
impl Tr {
    pub fn s<'a>(&self, key: &'a str) -> &'a str {
        if self.0 == Lang::En {
            return key;
        }
        MESSAGES
            .binary_search_by(|(k, _)| k.cmp(&key))
            .map(|i| MESSAGES[i].1)
            .unwrap_or(key)
    }
    pub fn lang(&self) -> &'static str {
        self.0.code()
    }
}

/// (English, Japanese), sorted by the English key.
static MESSAGES: &[(&str, &str)] = &[
    ("(no message)", "（メッセージなし）"),
    (
        "A repository with this name already exists.",
        "この名前のリポジトリは既に存在します。",
    ),
    ("Active", "有効"),
    ("Add a comment", "コメントを追加"),
    ("Add files", "ファイルを追加"),
    (
        "Add to this review as a new revision",
        "このレビューの新しいリビジョンとして追加",
    ),
    ("Added", "追加"),
    ("Administration", "管理"),
    ("Approvals", "承認"),
    ("Approve", "承認する"),
    ("Archived", "アーカイブ済み"),
    ("Author", "作者"),
    (
        "Base differs between these revisions.",
        "これらのリビジョンは base が異なります。",
    ),
    ("Binary file", "バイナリファイル"),
    ("Binary file changed", "バイナリファイルの変更"),
    ("Blame", "Blame"),
    ("Branch", "ブランチ"),
    ("Branch name", "ブランチ名"),
    ("Branches", "ブランチ"),
    ("Cancel", "キャンセル"),
    ("Changed files", "変更されたファイル"),
    ("Changes", "変更"),
    ("Changeset", "Changeset"),
    ("Changesets", "Changeset"),
    ("Choose files", "ファイルを選択"),
    ("Clone", "クローン"),
    ("Close review", "レビューを閉じる"),
    ("Closed", "クローズ"),
    ("Code", "コード"),
    ("Comment", "コメント"),
    ("Commit changes", "変更をコミット"),
    ("Commit message", "コミットメッセージ"),
    ("Compare", "比較"),
    ("Configuration", "設定"),
    (
        "Configuration is managed by the repositories file and cannot be changed here.",
        "設定はリポジトリファイルで管理されているため、ここでは変更できません。",
    ),
    ("Conflict", "競合"),
    ("Conflicting paths", "競合したパス"),
    ("Content", "内容"),
    ("Create", "作成"),
    (
        "Create a new branch and land there",
        "新しいブランチを作成して着地",
    ),
    ("Create a review", "レビューを作成"),
    ("Create branch", "ブランチを作成"),
    ("Create repository", "リポジトリを作成"),
    ("Create the first file", "最初のファイルを作成"),
    ("Created", "作成日時"),
    ("Default branch", "デフォルトブランチ"),
    ("Delete", "削除"),
    ("Delete file", "ファイルを削除"),
    ("Delete repository", "リポジトリを削除"),
    ("Deleted", "削除"),
    ("Description", "説明"),
    ("Destination", "着地先"),
    ("Diff view", "差分の表示"),
    ("Directory-limited view", "ディレクトリ限定ビュー"),
    ("Discard", "破棄"),
    ("Download", "ダウンロード"),
    ("Draft", "下書き"),
    ("Edit", "編集"),
    ("Edit file", "ファイルを編集"),
    (
        "Edited since you opened it; the server will rebase your change.",
        "編集を開始した後に変更されています。サーバーが変更を rebase します。",
    ),
    ("Email", "メール"),
    ("Empty repository", "空のリポジトリ"),
    ("Executable", "実行可能"),
    ("Files", "ファイル"),
    ("Forbidden", "アクセスが拒否されました"),
    ("Fork", "フォーク"),
    ("General", "一般"),
    ("Git commit", "Git コミット"),
    ("History", "履歴"),
    ("Invalid request", "不正なリクエスト"),
    ("JSON", "JSON"),
    (
        "Keep the old name as an alias",
        "旧名をエイリアスとして残す",
    ),
    ("Land directly on this branch", "このブランチに直接着地"),
    ("Language", "言語"),
    ("Latest change", "最新の変更"),
    ("Line endings", "改行コード"),
    ("Log in", "ログイン"),
    (
        "Log in with a personal access token",
        "個人アクセストークンでログイン",
    ),
    ("Log out", "ログアウト"),
    ("Merge", "マージ"),
    ("Merge bases", "マージベース"),
    ("Message", "メッセージ"),
    ("Method", "方式"),
    ("Name", "名前"),
    ("New branch", "新しいブランチ"),
    ("New file", "新規ファイル"),
    ("New name", "新しい名前"),
    ("New repository", "新しいリポジトリ"),
    ("New review", "新しいレビュー"),
    ("Next", "次へ"),
    ("No reviews.", "レビューはありません。"),
    ("Not found", "見つかりません"),
    ("Older revision", "古いリビジョン"),
    ("Open", "オープン"),
    ("Other refs", "その他の ref"),
    ("Overview", "概要"),
    ("Overwrite existing files", "既存のファイルを上書きする"),
    ("Parents", "親"),
    ("Path", "パス"),
    ("Permalink", "パーマリンク"),
    ("Personal access token", "個人アクセストークン"),
    ("Preferences", "設定"),
    ("Protected", "保護"),
    ("Publish", "公開"),
    ("Raw", "Raw"),
    ("Read-only", "読み取り専用"),
    ("Rebase onto target", "target に追従"),
    ("Refs", "Ref"),
    ("Rename", "改名"),
    ("Reopen", "再オープン"),
    ("Reply", "返信"),
    ("Repositories", "リポジトリ"),
    ("Repository", "リポジトリ"),
    ("Request changes", "変更を要求"),
    ("Resolve", "解決"),
    ("Resolved", "解決済み"),
    ("Retry", "再送信"),
    ("Review", "レビュー"),
    ("Review policy", "レビューポリシー"),
    ("Review required", "レビュー必須"),
    ("Reviews", "レビュー"),
    ("Revision", "リビジョン"),
    ("Save", "保存"),
    ("Save draft", "下書きを保存"),
    ("Settings", "設定"),
    ("Show source", "ソースを表示"),
    ("Side by side", "左右に並べる"),
    ("Sign-off name", "署名の名前"),
    ("Size", "サイズ"),
    ("Something went wrong", "エラーが発生しました"),
    ("Source ref", "元の ref"),
    ("Starting point", "起点"),
    ("State", "状態"),
    ("Submit", "Submit"),
    ("Submitted", "Submit 済み"),
    ("Symbolic link to", "シンボリックリンク:"),
    ("Tags", "タグ"),
    ("Target", "target"),
    (
        "The file is too large to display.",
        "ファイルが大きすぎるため表示できません。",
    ),
    ("The request was rejected:", "リクエストは拒否されました:"),
    (
        "The server is busy; try again later.",
        "サーバーが混雑しています。後でもう一度お試しください。",
    ),
    (
        "The upstream server failed.",
        "上流サーバーでエラーが発生しました。",
    ),
    (
        "This change conflicts with the current branch.",
        "この変更は現在のブランチと競合します。",
    ),
    (
        "This is a merge changeset.",
        "これはマージ Changeset です。",
    ),
    ("This repository is empty.", "このリポジトリは空です。"),
    (
        "This view is still being computed; reload in a few seconds.",
        "この画面はまだ計算中です。数秒後に再読み込みしてください。",
    ),
    ("Time zone", "タイムゾーン"),
    ("Title", "タイトル"),
    (
        "Type the repository name to confirm",
        "確認のためリポジトリ名を入力してください",
    ),
    ("Unified", "統合表示"),
    ("Unresolved", "未解決"),
    ("Update", "更新"),
    ("Upload", "アップロード"),
    ("Upload files", "ファイルをアップロード"),
    ("Usage", "使用量"),
    ("Votes", "投票"),
    ("Webhooks", "Webhook"),
    ("Write policy and limits", "書き込みポリシーと上限"),
    ("You need to log in.", "ログインが必要です。"),
    ("ahead of base", "base の子孫"),
    ("files", "ファイル"),
    ("gitlink", "gitlink"),
    ("just now", "たった今"),
    ("not ahead of base", "base の子孫ではない"),
    ("{n} days ago", "{n} 日前"),
    ("{n} hours ago", "{n} 時間前"),
    ("{n} minutes ago", "{n} 分前"),
    ("{n} months ago", "{n} か月前"),
    ("{n} years ago", "{n} 年前"),
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_is_sorted_and_negotiation_prefers_quality() {
        assert!(MESSAGES.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(Lang::negotiate("fr, ja;q=0.8, en;q=0.5"), Lang::Ja);
        assert_eq!(Lang::negotiate("en-US,ja;q=0.1"), Lang::En);
        assert_eq!(Tr(Lang::Ja).s("Log in"), "ログイン");
        assert_eq!(Tr(Lang::Ja).s("unknown key"), "unknown key");
    }
}
