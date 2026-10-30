use super::{Ctx, Link, Page, ReadExt, Repo, Result, WebError, browse};
use crate::render::{
    diff::{self, Row},
    markdown,
    text::{self, html, lossy},
    time,
};
use askama::Template;
use axum::{http::StatusCode, response::Response};
use kasu_proto::v1 as p;
use std::collections::BTreeMap;

fn state_label(t: crate::i18n::Tr, state: i32) -> String {
    t.s(match p::ReviewState::try_from(state) {
        Ok(p::ReviewState::Submitted) => "Submitted",
        Ok(p::ReviewState::Closed) => "Closed",
        _ => "Open",
    })
    .to_owned()
}
fn short_ref(name: &[u8]) -> String {
    lossy(name.strip_prefix(b"refs/heads/").unwrap_or(name))
}
pub async fn reviews(
    ctx: &mut Ctx,
    name: String,
    rest: Vec<Vec<u8>>,
    post: bool,
) -> Result<Response> {
    let repo = ctx.repo(&name).await?;
    let parts: Vec<String> = rest
        .iter()
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    match (
        parts
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        post,
    ) {
        ([], false) => list(ctx, repo).await,
        (["new"], false) => new_form(ctx, repo).await,
        (["new"], true) => create(ctx, repo).await,
        ([number], false) => overview(ctx, repo, parse(number)?).await,
        ([number, "files"], false) => files(ctx, repo, parse(number)?).await,
        ([number, "changesets"], false) => changesets(ctx, repo, parse(number)?).await,
        ([number, action], true) => act(ctx, repo, parse(number)?, action).await,
        _ => Err(WebError::NotFound),
    }
}
fn parse(number: &str) -> Result<u64> {
    number
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .ok_or(WebError::NotFound)
}

#[derive(Template)]
#[template(path = "reviews.html")]
struct ListPage {
    page: Page,
    rows: Vec<ListRow>,
    filters: Vec<Link>,
    next: Option<String>,
    create: Option<String>,
}
struct ListRow {
    number: u64,
    url: String,
    title: String,
    target: String,
    author: String,
    state: String,
    approvals: String,
    updated: String,
}
async fn list(ctx: &mut Ctx, repo: Repo) -> Result<Response> {
    let t = ctx.t;
    let state = match ctx.q("state") {
        Some("all") => None,
        Some("submitted") => Some(p::ReviewState::Submitted as i32),
        Some("closed") => Some(p::ReviewState::Closed as i32),
        _ => Some(p::ReviewState::Open as i32),
    };
    let target = ctx
        .q("target")
        .map(|t| t.as_bytes().to_vec())
        .unwrap_or_default();
    let author = ctx.q("author").unwrap_or_default().to_owned();
    let cursor: u64 = ctx.q("cursor").and_then(|c| c.parse().ok()).unwrap_or(0);
    let listed = rpc!(
        ctx,
        review.list_reviews(p::ListReviewsRequest {
            repo: repo.bytes(),
            state,
            target: target.clone(),
            author: author.clone(),
            cursor,
            limit: 50,
        })
    )
    .await
    .read()?;
    let mut rows = Vec::new();
    for review in &listed.reviews {
        let status = rpc!(
            ctx,
            review.get_submit_status(p::GetReviewRequest {
                repo: repo.bytes(),
                number: review.number
            })
        )
        .await
        .ok();
        rows.push(ListRow {
            number: review.number,
            url: repo.url("reviews", Some(review.number.to_string().as_bytes()), None),
            title: lossy(&review.title),
            target: short_ref(&review.target),
            author: review.author.clone(),
            state: state_label(t, review.state),
            approvals: status
                .map(|s| format!("{}/{}", s.approvals, s.required_approvals))
                .unwrap_or_default(),
            updated: time::absolute(review.updated_at, b"+0000", ctx.prefs.tz.as_deref()),
        });
    }
    let base = repo.url("reviews", None, None);
    let filters = [
        ("open", "Open"),
        ("submitted", "Submitted"),
        ("closed", "Closed"),
        ("all", "All"),
    ]
    .into_iter()
    .map(|(key, label)| Link {
        label: t.s(label).to_owned(),
        url: format!("{base}?state={key}"),
        on: ctx.q("state").unwrap_or("open") == key,
    })
    .collect();
    let next = (listed.cursor != 0).then(|| {
        format!(
            "{base}?state={}&cursor={}",
            ctx.q("state").unwrap_or("open"),
            listed.cursor
        )
    });
    let permissions = ctx.permissions(&repo, &[]).await;
    let create = permissions
        .as_ref()
        .is_some_and(|p| p.write && p.review)
        .then(|| repo.url("reviews", Some(b"new"), None));
    let mut page = ctx.page(format!("{} {}", t.s("Reviews"), repo.name));
    page.repo = Some(ctx.nav(&repo, "reviews", None, permissions.is_some_and(|p| p.admin)));
    Ok(ctx.html(
        &ListPage {
            page,
            rows,
            filters,
            next,
            create,
        },
        StatusCode::OK,
        false,
    ))
}

#[derive(Template)]
#[template(path = "review_new.html")]
struct NewPage {
    page: Page,
    csrf: String,
    key: String,
    target: String,
    head: String,
    candidates: Vec<(String, String, String)>,
    title: String,
    description: String,
    error: Option<String>,
}
/// The first-parent chain from `head` down to (excluding) the target tip or
/// a changeset already on it, at most 64 long, newest first.
async fn stack_candidates(
    ctx: &Ctx,
    repo: &Repo,
    target: &[u8],
    head: &[u8],
) -> Result<Vec<p::Changeset>> {
    let tip = rpc!(
        ctx,
        metadata.resolve_ref(p::RefRequest {
            repo: repo.bytes(),
            name: target.to_vec()
        })
    )
    .await
    .read()?
    .target;
    let mut out = Vec::new();
    let mut current = Some(head.to_vec());
    while let Some(id) = current {
        if out.len() >= 64 || Some(&id) == tip.as_ref() {
            break;
        }
        if let Some(tip) = &tip {
            let on_target = rpc!(
                ctx,
                metadata.is_ancestor(p::CompareRequest {
                    repo: repo.bytes(),
                    base: id.clone(),
                    head: tip.clone()
                })
            )
            .await
            .read()?;
            if on_target.is_ancestor {
                break;
            }
        }
        let cs = browse::get_changeset(ctx, repo, &id).await?;
        current = cs.parents.first().cloned();
        out.push(cs);
    }
    Ok(out)
}
async fn new_form(ctx: &mut Ctx, repo: Repo) -> Result<Response> {
    ctx.require_account()?;
    let target = ctx
        .q("target")
        .map(str::to_owned)
        .unwrap_or_else(|| lossy(&repo.default_ref()));
    let head_text = ctx.q("head").unwrap_or_default().to_owned();
    let head = if head_text.is_empty() {
        None
    } else {
        Some(ctx.resolve(&repo, head_text.as_bytes()).await?.id)
    };
    let candidates = match &head {
        Some(head) => stack_candidates(ctx, &repo, target.as_bytes(), head).await?,
        None => Vec::new(),
    };
    let title = candidates
        .last()
        .map(|cs| text::first_line(&cs.message))
        .unwrap_or_default();
    let t = ctx.t;
    let mut page = ctx.page(t.s("New review"));
    page.repo = Some(ctx.nav(&repo, "reviews", None, false));
    let body = NewPage {
        page,
        csrf: ctx.csrf("review"),
        key: Ctx::idempotency_key(),
        target,
        head: head_text,
        candidates: candidates
            .iter()
            .map(|cs| {
                (
                    hex::encode(&cs.id),
                    text::short(&cs.id),
                    text::first_line(&cs.message),
                )
            })
            .collect(),
        title,
        description: String::new(),
        error: None,
    };
    Ok(ctx.html(&body, StatusCode::OK, false))
}
async fn create(ctx: &mut Ctx, repo: Repo) -> Result<Response> {
    ctx.require_account()?;
    let fields = ctx.form("review").await?;
    let target = fields.text("target");
    let target = if target.starts_with("refs/") {
        target
    } else {
        format!("refs/heads/{target}")
    };
    let head = hex::decode(fields.text("base_after"))
        .map_err(|_| WebError::BadRequest("choose where the stack starts".into()))?;
    let tip = ctx.resolve(&repo, fields.text("head").as_bytes()).await?.id;
    // Walk from the head down to the chosen first changeset.
    let candidates = stack_candidates(ctx, &repo, target.as_bytes(), &tip).await?;
    let end = candidates
        .iter()
        .position(|cs| cs.id == head)
        .ok_or_else(|| {
            WebError::BadRequest("the first changeset is not on the head's chain".into())
        })?;
    let stack: Vec<Vec<u8>> = candidates[..=end]
        .iter()
        .rev()
        .map(|cs| cs.id.clone())
        .collect();
    let review = ctx
        .upstream()
        .review()
        .create_review(
            ctx.wreq(p::CreateReviewRequest {
                repo: repo.bytes(),
                target: target.into_bytes(),
                changesets: stack,
                title: fields.text("title").into_bytes(),
                description: fields
                    .text("description")
                    .replace("\r\n", "\n")
                    .into_bytes(),
                idempotency_key: fields.text("key").into_bytes(),
            }),
        )
        .await
        .wrote()?
        .into_inner();
    let url = repo.url("reviews", Some(review.number.to_string().as_bytes()), None);
    Ok(ctx.redirect(&url))
}

struct Loaded {
    review: p::ReviewRecord,
    status: Option<p::SubmitStatus>,
    comments: Vec<p::ReviewComment>,
    permissions: Option<p::Permissions>,
}
async fn load(ctx: &Ctx, repo: &Repo, number: u64) -> Result<Loaded> {
    let review = rpc!(
        ctx,
        review.get_review(p::GetReviewRequest {
            repo: repo.bytes(),
            number
        })
    )
    .await
    .read()?;
    let status = rpc!(
        ctx,
        review.get_submit_status(p::GetReviewRequest {
            repo: repo.bytes(),
            number
        })
    )
    .await
    .ok();
    let comments = rpc!(
        ctx,
        review.list_comments(p::ListCommentsRequest {
            repo: repo.bytes(),
            number
        })
    )
    .await
    .read()?
    .comments;
    let permissions = ctx
        .permissions(repo, std::slice::from_ref(&review.target))
        .await;
    Ok(Loaded {
        review,
        status,
        comments,
        permissions,
    })
}
fn review_tabs(repo: &Repo, number: u64, tab: &str, t: crate::i18n::Tr) -> Vec<Link> {
    let base = repo.url("reviews", Some(number.to_string().as_bytes()), None);
    [
        ("overview", "Overview", base.clone()),
        ("files", "Files", format!("{base}/files")),
        ("changesets", "Changesets", format!("{base}/changesets")),
    ]
    .into_iter()
    .map(|(key, label, url)| Link {
        label: t.s(label).to_owned(),
        url,
        on: key == tab,
    })
    .collect()
}
pub struct CommentView {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub when: String,
    pub draft: bool,
    pub deleted: bool,
    pub mine: bool,
    pub older: bool,
}
pub struct Thread {
    pub root: CommentView,
    pub replies: Vec<CommentView>,
    pub resolved: bool,
    pub location: String,
}
fn comment_view(ctx: &Ctx, comment: &p::ReviewComment, older: bool) -> CommentView {
    CommentView {
        id: comment.id,
        author: comment.author.clone(),
        body: if comment.deleted {
            String::new()
        } else {
            markdown::render(&comment.body, None)
        },
        when: time::absolute(comment.created_at, b"+0000", ctx.prefs.tz.as_deref()),
        draft: comment.draft,
        deleted: comment.deleted,
        mine: ctx.viewer.account() == Some(comment.author.as_str()),
        older,
    }
}
fn threads(
    ctx: &Ctx,
    comments: &[p::ReviewComment],
    filter: impl Fn(&p::ReviewComment) -> bool,
) -> Vec<Thread> {
    comments
        .iter()
        .filter(|c| c.parent.is_none() && filter(c))
        .map(|root| Thread {
            root: comment_view(ctx, root, false),
            replies: comments
                .iter()
                .filter(|c| c.parent == Some(root.id))
                .map(|c| comment_view(ctx, c, false))
                .collect(),
            resolved: root.resolved,
            location: root
                .anchor
                .as_ref()
                .map(|a| format!("{}:{} (r{})", lossy(&a.path), a.line, root.revision))
                .unwrap_or_default(),
        })
        .collect()
}

#[derive(Template)]
#[template(path = "review.html")]
struct OverviewPage {
    page: Page,
    review: Summary,
    description: String,
    unmet: Vec<String>,
    ready: bool,
    approvals: String,
    methods: Vec<(i32, &'static str)>,
    votes: Vec<(String, String, String)>,
    timeline: Vec<(String, String)>,
    threads: Vec<Thread>,
    drafts: usize,
    csrf: String,
    key: String,
    can_vote: bool,
    can_submit: bool,
    can_edit: bool,
    can_rebase: bool,
    open: bool,
    closed: bool,
    notice: Option<String>,
    edit_url: Option<String>,
}
pub struct Summary {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub target: String,
    pub author: String,
    pub revision: u32,
    pub version: u64,
    pub tabs: Vec<Link>,
}
fn summary(ctx: &Ctx, repo: &Repo, review: &p::ReviewRecord, tab: &str) -> Summary {
    Summary {
        number: review.number,
        title: lossy(&review.title),
        state: state_label(ctx.t, review.state),
        target: short_ref(&review.target),
        author: review.author.clone(),
        revision: review.revisions.last().map(|r| r.number).unwrap_or(0),
        version: review.version,
        tabs: review_tabs(repo, review.number, tab, ctx.t),
    }
}
async fn overview(ctx: &mut Ctx, repo: Repo, number: u64) -> Result<Response> {
    overview_with(ctx, repo, number, None, StatusCode::OK).await
}
async fn overview_with(
    ctx: &mut Ctx,
    repo: Repo,
    number: u64,
    notice: Option<String>,
    code: StatusCode,
) -> Result<Response> {
    let loaded = load(ctx, &repo, number).await?;
    let review = &loaded.review;
    let t = ctx.t;
    let me = ctx.viewer.account().map(str::to_owned);
    let open = review.state == p::ReviewState::Open as i32;
    let mut timeline: Vec<(i64, String)> = Vec::new();
    for revision in &review.revisions {
        timeline.push((
            revision.created_at,
            format!(
                "{} {} — {} ({} changesets)",
                t.s("Revision"),
                revision.number,
                revision.created_by,
                revision.changesets.len()
            ),
        ));
    }
    for vote in &review.votes {
        let value = match p::VoteValue::try_from(vote.value) {
            Ok(p::VoteValue::Approve) => t.s("Approve"),
            Ok(p::VoteValue::RequestChanges) => t.s("Request changes"),
            _ => "—",
        };
        timeline.push((
            vote.at,
            format!("{}: {value} (r{})", vote.account, vote.revision),
        ));
    }
    for comment in loaded.comments.iter().filter(|c| !c.draft) {
        timeline.push((
            comment.created_at,
            format!("{}: {}", comment.author, t.s("Comment")),
        ));
    }
    if let Some(submitted) = &review.submitted {
        timeline.push((
            submitted.at,
            format!("{} — {}", t.s("Submitted"), submitted.by),
        ));
    }
    timeline.sort_by_key(|(at, _)| *at);
    let status = loaded.status.clone().unwrap_or_default();
    let permissions = loaded.permissions.as_ref();
    let can_participate = ctx.viewer.signed_in() && permissions.is_some_and(|p| p.review);
    let admin = permissions.is_some_and(|p| p.admin);
    let author = me.as_deref() == Some(review.author.as_str());
    let edit_url = (open && (author || admin) && permissions.is_some_and(|p| p.write)).then(|| {
        let head = review
            .revisions
            .last()
            .and_then(|r| r.changesets.last())
            .map(hex::encode)
            .unwrap_or_default();
        format!(
            "{}?review={number}",
            repo.url("new", Some(head.as_bytes()), None)
        )
    });
    let mut page = ctx.page(format!("#{number} {}", lossy(&review.title)));
    page.repo = Some(ctx.nav(&repo, "reviews", None, admin));
    let body = OverviewPage {
        review: summary(ctx, &repo, review, "overview"),
        description: markdown::render(&review.description, None),
        unmet: status.unmet.clone(),
        ready: status.ready,
        approvals: format!("{}/{}", status.approvals, status.required_approvals),
        methods: status
            .methods
            .iter()
            .map(|m| {
                (
                    *m,
                    if *m == p::SubmitMethod::Squash as i32 {
                        "SQUASH"
                    } else {
                        "REBASE"
                    },
                )
            })
            .collect(),
        votes: review
            .votes
            .iter()
            .map(|v| {
                (
                    v.account.clone(),
                    p::VoteValue::try_from(v.value)
                        .map(|v| v.as_str_name())
                        .unwrap_or("?")
                        .trim_start_matches("VOTE_VALUE_")
                        .to_owned(),
                    format!("r{}", v.revision),
                )
            })
            .collect(),
        timeline: timeline
            .into_iter()
            .map(|(at, text)| (time::absolute(at, b"+0000", ctx.prefs.tz.as_deref()), text))
            .collect(),
        threads: threads(ctx, &loaded.comments, |c| {
            !c.draft || Some(&c.author) == me.as_ref()
        }),
        drafts: loaded
            .comments
            .iter()
            .filter(|c| c.draft && Some(&c.author) == me.as_ref())
            .count(),
        csrf: ctx.csrf("review"),
        key: Ctx::idempotency_key(),
        can_vote: can_participate && open,
        can_submit: can_participate && open && status.ready,
        can_edit: can_participate && (author || admin),
        can_rebase: can_participate && open && permissions.is_some_and(|p| p.write),
        open,
        closed: review.state == p::ReviewState::Closed as i32,
        notice,
        edit_url,
        page,
    };
    Ok(ctx.html(&body, code, false))
}

#[derive(Template)]
#[template(path = "review_files.html")]
struct FilesPage {
    page: Page,
    review: Summary,
    revisions: Vec<Link>,
    against: Vec<Link>,
    notice: Option<String>,
    files: Vec<browse::FileView>,
    pages: Vec<Link>,
    older: Vec<Thread>,
    general: Vec<Thread>,
}
/// Comment form and threads attached to diff rows.
struct Annotator {
    by_line: BTreeMap<(Vec<u8>, i32, u64), Vec<Thread>>,
    form_at: Option<(Vec<u8>, i32, u64)>,
    csrf: String,
    action: String,
    revision: u32,
    can_comment: bool,
    link_base: String,
}
impl Annotator {
    fn render(&self, path: &[u8], row: &Row, t: crate::i18n::Tr) -> String {
        let Some((side, line)) = diff::anchor(row) else {
            return String::new();
        };
        let key = (path.to_vec(), side as i32, line);
        let mut out = String::new();
        if let Some(threads) = self.by_line.get(&key) {
            for thread in threads {
                out.push_str(&thread_html(
                    thread,
                    t,
                    &self.csrf,
                    &self.action,
                    self.can_comment,
                ));
            }
        }
        if self.can_comment {
            if self.form_at.as_ref() == Some(&key) {
                out.push_str(&format!(
                    "<form method=\"post\" action=\"{}/comment\" class=\"stack\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"revision\" value=\"{}\"><input type=\"hidden\" name=\"path\" value=\"{}\"><input type=\"hidden\" name=\"side\" value=\"{}\"><input type=\"hidden\" name=\"line\" value=\"{line}\"><textarea name=\"body\" required aria-label=\"{}\"></textarea><div><button type=\"submit\">{}</button></div></form>",
                    self.action, html(self.csrf.as_bytes()), self.revision, hex::encode(path), side as i32, t.s("Comment"), t.s("Save draft")
                ));
            } else {
                out.push_str(&format!(
                    "<a class=\"muted\" href=\"{}&amp;comment={}:{}:{line}#c-{}-{}-{line}\" id=\"c-{}-{}-{line}\">+ {}</a>",
                    html(self.link_base.as_bytes()), hex::encode(path), side as i32, hex::encode(path), side as i32, hex::encode(path), side as i32, t.s("Comment")
                ));
            }
        }
        out
    }
}
fn thread_html(
    thread: &Thread,
    t: crate::i18n::Tr,
    csrf: &str,
    action: &str,
    can_comment: bool,
) -> String {
    let comment = |c: &CommentView| {
        format!(
            "<div class=\"comment\"><span class=\"who\">{}</span> <span class=\"muted\">{}</span>{}{}<div class=\"markdown-inline\">{}</div></div>",
            html(c.author.as_bytes()),
            html(c.when.as_bytes()),
            if c.draft {
                format!(" <span class=\"badge warn\">{}</span>", t.s("Draft"))
            } else {
                String::new()
            },
            if c.older {
                format!(" <span class=\"badge\">{}</span>", t.s("Older revision"))
            } else {
                String::new()
            },
            if c.deleted {
                format!("<em class=\"muted\">{}</em>", t.s("Deleted"))
            } else {
                c.body.clone()
            }
        )
    };
    let mut out = format!(
        "<div class=\"thread{}\">",
        if thread.resolved { " resolved" } else { "" }
    );
    out.push_str(&comment(&thread.root));
    for reply in &thread.replies {
        out.push_str(&comment(reply));
    }
    if can_comment && !thread.root.draft {
        out.push_str(&format!(
            "<form method=\"post\" action=\"{action}/comment\" class=\"bar\"><input type=\"hidden\" name=\"csrf\" value=\"{csrf}\"><input type=\"hidden\" name=\"parent\" value=\"{id}\"><input type=\"text\" name=\"body\" required aria-label=\"{reply}\"><button type=\"submit\">{reply}</button></form><form method=\"post\" action=\"{action}/resolve\" class=\"inline-form\"><input type=\"hidden\" name=\"csrf\" value=\"{csrf}\"><input type=\"hidden\" name=\"id\" value=\"{id}\"><input type=\"hidden\" name=\"resolved\" value=\"{flag}\"><button type=\"submit\">{label}</button></form>",
            id = thread.root.id,
            reply = t.s("Reply"),
            flag = if thread.resolved { "" } else { "1" },
            label = if thread.resolved { t.s("Unresolved") } else { t.s("Resolve") },
        ));
    }
    out.push_str("</div>");
    out
}
/// The content of line `line` (1-based) of `path` at `id`, if it exists.
async fn line_at(ctx: &Ctx, repo: &Repo, id: &[u8], path: &[u8], line: u64) -> Option<Vec<u8>> {
    let file = browse::read_file(ctx, repo, id, path).await.ok()?;
    text::lines(&file.content)
        .get(line.checked_sub(1)? as usize)
        .map(|l| l.to_vec())
}
async fn files(ctx: &mut Ctx, repo: Repo, number: u64) -> Result<Response> {
    let loaded = load(ctx, &repo, number).await?;
    let review = &loaded.review;
    let t = ctx.t;
    let latest = review.revisions.last().ok_or(WebError::NotFound)?.number;
    let k: u32 = ctx.q("rev").and_then(|r| r.parse().ok()).unwrap_or(latest);
    let revision = review
        .revisions
        .iter()
        .find(|r| r.number == k)
        .ok_or(WebError::NotFound)?;
    let against = ctx
        .q("against")
        .and_then(|r| r.parse::<u32>().ok())
        .and_then(|j| review.revisions.iter().find(|r| r.number == j));
    let head = revision
        .changesets
        .last()
        .cloned()
        .ok_or(WebError::NotFound)?;
    let mut notice = None;
    let base = match against {
        Some(earlier) if earlier.base == revision.base => earlier
            .changesets
            .last()
            .cloned()
            .ok_or(WebError::NotFound)?,
        Some(_) => {
            notice = Some(t.s("Base differs between these revisions.").to_owned());
            revision.base.clone()
        }
        None => revision.base.clone(),
    };
    if base.is_empty() {
        return Err(WebError::BadRequest(
            "the stack starts at a root changeset".into(),
        ));
    }
    let page_number: usize = ctx.q("page").and_then(|p| p.parse().ok()).unwrap_or(0);
    let expand = ctx.q("expand").map(|p| p.as_bytes().to_vec());
    let (diffs, total) = browse::diff_files(
        ctx,
        &repo,
        &base,
        &head,
        &[],
        page_number,
        expand.as_deref(),
    )
    .await?;
    let me = ctx.viewer.account().map(str::to_owned);
    let can_comment =
        ctx.viewer.signed_in() && loaded.permissions.as_ref().is_some_and(|p| p.review);
    // Place anchored comments of the cumulative diff: this revision's on their
    // lines; older ones too when that line is unchanged, else listed apart.
    let mut by_line: BTreeMap<(Vec<u8>, i32, u64), Vec<Thread>> = BTreeMap::new();
    let mut older = Vec::new();
    let visible = |c: &p::ReviewComment| !c.draft || Some(&c.author) == me.as_ref();
    let all = threads(ctx, &loaded.comments, |c| {
        visible(c)
            && c.anchor
                .as_ref()
                .is_some_and(|a| a.changeset_index.is_none())
    });
    for (thread, root) in all.into_iter().zip(loaded.comments.iter().filter(|c| {
        c.parent.is_none()
            && visible(c)
            && c.anchor
                .as_ref()
                .is_some_and(|a| a.changeset_index.is_none())
    })) {
        let anchor = root.anchor.clone().unwrap_or_default();
        let key = (anchor.path.clone(), anchor.side, anchor.line);
        if root.revision == k || against.is_some() {
            by_line.entry(key).or_default().push(thread);
            continue;
        }
        let then = review.revisions.iter().find(|r| r.number == root.revision);
        let unchanged = match then {
            Some(then) if anchor.side == p::AnchorSide::Head as i32 => {
                let old = line_at(
                    ctx,
                    &repo,
                    then.changesets
                        .last()
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                    &anchor.path,
                    anchor.line,
                )
                .await;
                old.is_some() && old == line_at(ctx, &repo, &head, &anchor.path, anchor.line).await
            }
            _ => false,
        };
        let mut thread = thread;
        thread.root.older = true;
        if unchanged {
            by_line.entry(key).or_default().push(thread);
        } else {
            older.push(thread);
        }
    }
    let base_url = repo.url("reviews", Some(number.to_string().as_bytes()), None);
    let link_base = format!("{base_url}/files?rev={k}");
    let form_at = ctx.q("comment").and_then(|c| {
        let mut parts = c.split(':');
        Some((
            hex::decode(parts.next()?).ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        ))
    });
    let annotator = Annotator {
        by_line,
        form_at,
        csrf: ctx.csrf("review"),
        action: base_url.clone(),
        revision: k,
        can_comment: can_comment && against.is_none(),
        link_base: link_base.clone(),
    };
    let views = diffs
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let path = f.path.clone();
            browse::file_view(
                ctx,
                f,
                i,
                format!("{link_base}&expand={}", crate::rev::segment(&f.path)),
                &|_, row| annotator.render(&path, row, t),
            )
        })
        .collect::<Vec<_>>();
    if ctx.q("expand").is_some() {
        return Ok(ctx.html(
            &browse::FragmentPage { files: views },
            StatusCode::OK,
            false,
        ));
    }
    let revisions = review
        .revisions
        .iter()
        .map(|r| Link {
            label: format!("r{}", r.number),
            url: format!("{base_url}/files?rev={}", r.number),
            on: r.number == k && against.is_none(),
        })
        .collect();
    let against_links = review
        .revisions
        .iter()
        .filter(|r| r.number < k)
        .map(|r| Link {
            label: format!("r{}…r{k}", r.number),
            url: format!("{base_url}/files?rev={k}&against={}", r.number),
            on: against.is_some_and(|a| a.number == r.number),
        })
        .collect();
    let general = threads(ctx, &loaded.comments, |c| {
        visible(c)
            && c.anchor
                .as_ref()
                .is_some_and(|a| a.changeset_index.is_some())
    });
    let mut page = ctx.page(format!("#{number} {}", t.s("Files")));
    page.repo = Some(ctx.nav(
        &repo,
        "reviews",
        None,
        loaded.permissions.as_ref().is_some_and(|p| p.admin),
    ));
    let pages = browse::paging(
        &format!("{link_base}&"),
        total,
        ctx.app.config.file.limits.diff_files,
        page_number,
    );
    let body = FilesPage {
        review: summary(ctx, &repo, review, "files"),
        revisions,
        against: against_links,
        notice,
        files: views,
        pages,
        older,
        general,
        page,
    };
    Ok(ctx.html(&body, StatusCode::OK, false))
}

#[derive(Template)]
#[template(path = "review_changesets.html")]
struct ChangesetsPage {
    page: Page,
    review: Summary,
    rows: Vec<(Link, String, String)>,
    files: Vec<browse::FileView>,
    pages: Vec<Link>,
}
async fn changesets(ctx: &mut Ctx, repo: Repo, number: u64) -> Result<Response> {
    let loaded = load(ctx, &repo, number).await?;
    let review = &loaded.review;
    let revision = review.revisions.last().ok_or(WebError::NotFound)?;
    let index: usize = ctx.q("index").and_then(|i| i.parse().ok()).unwrap_or(0);
    let base_url = format!(
        "{}/changesets",
        repo.url("reviews", Some(number.to_string().as_bytes()), None)
    );
    let mut rows = Vec::new();
    for (i, id) in revision.changesets.iter().enumerate() {
        let cs = browse::get_changeset(ctx, &repo, id).await?;
        let author = cs.author.clone().unwrap_or_default();
        rows.push((
            Link {
                label: text::short(id),
                url: format!("{base_url}?index={i}"),
                on: i == index,
            },
            text::first_line(&cs.message),
            lossy(&author.name),
        ));
    }
    let selected = revision.changesets.get(index).ok_or(WebError::NotFound)?;
    let parent = if index == 0 {
        revision.base.clone()
    } else {
        revision.changesets[index - 1].clone()
    };
    let (files, pages) = if parent.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let page_number: usize = ctx.q("page").and_then(|p| p.parse().ok()).unwrap_or(0);
        let (diffs, total) =
            browse::diff_files(ctx, &repo, &parent, selected, &[], page_number, None).await?;
        (
            diffs
                .iter()
                .enumerate()
                .map(|(i, f)| browse::file_view(ctx, f, i, String::new(), &|_, _| String::new()))
                .collect(),
            browse::paging(
                &format!("{base_url}?index={index}&"),
                total,
                ctx.app.config.file.limits.diff_files,
                page_number,
            ),
        )
    };
    let mut page = ctx.page(format!("#{number} {}", ctx.t.s("Changesets")));
    page.repo = Some(ctx.nav(
        &repo,
        "reviews",
        None,
        loaded.permissions.as_ref().is_some_and(|p| p.admin),
    ));
    let body = ChangesetsPage {
        review: summary(ctx, &repo, review, "changesets"),
        rows,
        files,
        pages,
        page,
    };
    Ok(ctx.html(&body, StatusCode::OK, false))
}

async fn act(ctx: &mut Ctx, repo: Repo, number: u64, action: &str) -> Result<Response> {
    ctx.require_account()?;
    let fields = ctx.form("review").await?;
    let base_url = repo.url("reviews", Some(number.to_string().as_bytes()), None);
    let review = rpc!(
        ctx,
        review.get_review(p::GetReviewRequest {
            repo: repo.bytes(),
            number
        })
    )
    .await
    .read()?;
    let latest = review.revisions.last().map(|r| r.number).unwrap_or(0);
    let back = fields
        .get("back")
        .filter(|b| b.starts_with(&base_url))
        .map(str::to_owned);
    let client = || ctx.upstream().review();
    match action {
        "comment" => {
            let parent = fields.get("parent").and_then(|p| p.parse().ok());
            let anchor = match (fields.get("path"), fields.get("line")) {
                (Some(path), Some(line)) if parent.is_none() => Some(p::Anchor {
                    path: hex::decode(path)
                        .map_err(|_| WebError::BadRequest("invalid path".into()))?,
                    side: fields.text("side").parse().unwrap_or(0),
                    line: line
                        .parse()
                        .map_err(|_| WebError::BadRequest("invalid line".into()))?,
                    changeset_index: fields.get("index").and_then(|i| i.parse().ok()),
                }),
                _ => None,
            };
            let revision = fields
                .get("revision")
                .and_then(|r| r.parse().ok())
                .unwrap_or(latest);
            client()
                .save_draft(ctx.wreq(p::SaveDraftRequest {
                    repo: repo.bytes(),
                    number,
                    revision,
                    parent,
                    anchor,
                    body: fields.text("body").replace("\r\n", "\n").into_bytes(),
                }))
                .await
                .wrote()?;
            // Replies are published at once; line comments stay drafts.
            if parent.is_some() {
                client()
                    .publish(ctx.wreq(p::PublishRequest {
                        repo: repo.bytes(),
                        number,
                        revision: latest,
                        vote: None,
                        body: Vec::new(),
                    }))
                    .await
                    .wrote()?;
            }
            let back = back.unwrap_or_else(|| {
                if fields.get("path").is_some() {
                    format!("{base_url}/files?rev={revision}")
                } else {
                    base_url.clone()
                }
            });
            Ok(ctx.redirect(&back))
        }
        "publish" => {
            let vote = match fields.get("vote") {
                Some("approve") => Some(p::VoteValue::Approve as i32),
                Some("request_changes") => Some(p::VoteValue::RequestChanges as i32),
                Some("none") => Some(p::VoteValue::None as i32),
                _ => None,
            };
            let result = client()
                .publish(ctx.wreq(p::PublishRequest {
                    repo: repo.bytes(),
                    number,
                    revision: latest,
                    vote,
                    body: fields.text("body").replace("\r\n", "\n").into_bytes(),
                }))
                .await;
            match result {
                Ok(_) => Ok(ctx.redirect(&base_url)),
                Err(status) if status.code() == tonic::Code::InvalidArgument => {
                    overview_with(
                        ctx,
                        repo,
                        number,
                        Some(status.message().into()),
                        StatusCode::BAD_REQUEST,
                    )
                    .await
                }
                Err(status) => Err(WebError::write(status)),
            }
        }
        "resolve" => {
            client()
                .resolve_thread(
                    ctx.wreq(p::ResolveThreadRequest {
                        repo: repo.bytes(),
                        number,
                        id: fields
                            .text("id")
                            .parse()
                            .map_err(|_| WebError::BadRequest("invalid comment".into()))?,
                        resolved: fields.flag("resolved"),
                    }),
                )
                .await
                .wrote()?;
            Ok(ctx.redirect(&back.unwrap_or(base_url)))
        }
        "edit-comment" => {
            client()
                .edit_comment(
                    ctx.wreq(p::EditCommentRequest {
                        repo: repo.bytes(),
                        number,
                        id: fields
                            .text("id")
                            .parse()
                            .map_err(|_| WebError::BadRequest("invalid comment".into()))?,
                        body: fields.text("body").replace("\r\n", "\n").into_bytes(),
                        delete: fields.flag("delete"),
                    }),
                )
                .await
                .wrote()?;
            Ok(ctx.redirect(&back.unwrap_or(base_url)))
        }
        "update" => {
            let state = match fields.get("state") {
                Some("close") => Some(p::ReviewState::Closed as i32),
                Some("reopen") => Some(p::ReviewState::Open as i32),
                _ => None,
            };
            let title = fields.get("title").map(|t| t.as_bytes().to_vec());
            let description = fields
                .get("description")
                .map(|d| d.replace("\r\n", "\n").into_bytes());
            let result = client()
                .update_review(ctx.wreq(p::UpdateReviewRequest {
                    repo: repo.bytes(),
                    number,
                    expected_version: fields.text("version").parse().unwrap_or(0),
                    title,
                    description,
                    state,
                }))
                .await;
            match result {
                Ok(_) => Ok(ctx.redirect(&base_url)),
                Err(status) if status.code() == tonic::Code::Aborted => {
                    overview_with(
                        ctx,
                        repo,
                        number,
                        Some(status.message().into()),
                        StatusCode::CONFLICT,
                    )
                    .await
                }
                Err(status) => Err(WebError::write(status)),
            }
        }
        "rebase" => {
            let rebased = client()
                .rebase_review(ctx.wreq(p::RebaseReviewRequest {
                    repo: repo.bytes(),
                    number,
                    expected_revision: latest,
                    onto: Vec::new(),
                }))
                .await
                .wrote()?
                .into_inner();
            match rebased.conflict {
                Some(conflict) => {
                    let paths: Vec<String> =
                        conflict.conflicts.iter().map(|c| lossy(&c.path)).collect();
                    let message = format!(
                        "{} #{}: {}",
                        ctx.t.s("Conflict"),
                        rebased.conflict_index + 1,
                        paths.join(", ")
                    );
                    overview_with(ctx, repo, number, Some(message), StatusCode::CONFLICT).await
                }
                None => Ok(ctx.redirect(&base_url)),
            }
        }
        "submit" => {
            let method = if fields.get("method") == Some("SQUASH") {
                p::SubmitMethod::Squash
            } else {
                p::SubmitMethod::Rebase
            };
            let expected: u32 = fields.text("revision").parse().unwrap_or(latest);
            let submitted = client()
                .submit(ctx.wreq(p::SubmitRequest {
                    repo: repo.bytes(),
                    number,
                    expected_revision: expected,
                    method: method as i32,
                    idempotency_key: fields.text("key").into_bytes(),
                    ..Default::default()
                }))
                .await
                .wrote()?
                .into_inner();
            tracing::info!(account = ?ctx.viewer.account(), repository = %repo.name, number, outcome = submitted.outcome, "review submit through the web UI");
            match p::SubmitOutcome::try_from(submitted.outcome) {
                Ok(p::SubmitOutcome::Landed) => Ok(ctx.redirect(&base_url)),
                Ok(p::SubmitOutcome::Conflict) => {
                    let paths: Vec<String> = submitted
                        .conflict
                        .map(|c| c.conflicts.iter().map(|c| lossy(&c.path)).collect())
                        .unwrap_or_default();
                    let message = format!(
                        "{} #{}: {}. {}",
                        ctx.t.s("Conflict"),
                        submitted.conflict_index + 1,
                        paths.join(", "),
                        ctx.t.s("Rebase onto target")
                    );
                    overview_with(ctx, repo, number, Some(message), StatusCode::CONFLICT).await
                }
                _ => {
                    let mut message = submitted.message;
                    if let Some(status) = submitted.status {
                        for unmet in status.unmet {
                            message.push_str("; ");
                            message.push_str(&unmet);
                        }
                    }
                    overview_with(ctx, repo, number, Some(message), StatusCode::CONFLICT).await
                }
            }
        }
        _ => Err(WebError::NotFound),
    }
}
