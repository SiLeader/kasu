use super::{Ctx, Fields, Page, ReadExt, Repo, Resolved, Result, WebError, browse};
use crate::{
    auth::Identity,
    render::text::{self, lossy},
    rev,
};
use askama::Template;
use axum::{http::StatusCode, response::Response};
use kasu_proto::v1 as p;

#[derive(Template)]
#[template(path = "refs.html")]
struct RefsPage {
    page: Page,
    groups: Vec<RefGroup>,
    filter: String,
    csrf: String,
    can_create: bool,
    default_from: String,
}
struct RefGroup {
    title: String,
    rows: Vec<RefRow>,
}
struct RefRow {
    name: String,
    full: String,
    url: String,
    tip: String,
    tip_url: String,
    when: String,
    protected: bool,
    delete: Option<(String, u64)>,
}
pub async fn refs(ctx: &mut Ctx, name: String, rest: Vec<Vec<u8>>, post: bool) -> Result<Response> {
    if !rest.is_empty() {
        return Err(WebError::NotFound);
    }
    let repo = ctx.repo(&name).await?;
    if post {
        return change_ref(ctx, repo).await;
    }
    let listed = rpc!(
        ctx,
        metadata.list_refs(p::ListRefsRequest {
            repo: repo.bytes(),
            prefix: Vec::new()
        })
    )
    .await
    .read()?;
    let filter = ctx.q("q").unwrap_or_default().to_owned();
    let refs: Vec<p::Ref> = listed
        .refs
        .into_iter()
        .filter(|r| filter.is_empty() || lossy(&r.name).contains(&filter))
        .collect();
    let names: Vec<Vec<u8>> = refs.iter().take(256).map(|r| r.name.clone()).collect();
    let permissions = ctx.permissions(&repo, &names).await;
    let t = ctx.t;
    let mut groups = vec![
        RefGroup {
            title: t.s("Branches").into(),
            rows: Vec::new(),
        },
        RefGroup {
            title: t.s("Tags").into(),
            rows: Vec::new(),
        },
        RefGroup {
            title: t.s("Other refs").into(),
            rows: Vec::new(),
        },
    ];
    for (index, reference) in refs.iter().enumerate() {
        let Some(target) = &reference.target else {
            continue;
        };
        let (group, short) = if let Some(s) = reference.name.strip_prefix(b"refs/heads/") {
            (0, s)
        } else if let Some(s) = reference.name.strip_prefix(b"refs/tags/") {
            (1, s)
        } else {
            (2, reference.name.as_slice())
        };
        let when = if index < 200 {
            browse::get_changeset(ctx, &repo, target)
                .await
                .ok()
                .and_then(|cs| cs.committer)
                .map(|s| crate::render::time::absolute(s.timestamp, &s.tz, ctx.prefs.tz.as_deref()))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let permission = permissions
            .as_ref()
            .and_then(|p| p.refs.iter().find(|r| r.name == reference.name));
        let protected = permission.is_some_and(|p| p.review_required || p.review_bypass);
        groups[group].rows.push(RefRow {
            name: lossy(short),
            full: hex::encode(&reference.name),
            url: repo.url(
                "tree",
                Some(if group == 2 {
                    reference.name.as_slice()
                } else {
                    short
                }),
                None,
            ),
            tip: text::short(target),
            tip_url: repo.url("changeset", Some(hex::encode(target).as_bytes()), None),
            when,
            protected,
            delete: permission
                .filter(|p| p.delete && !p.review_required && group == 0)
                .map(|_| (hex::encode(target), reference.version)),
        });
    }
    let can_create = permissions.as_ref().is_some_and(|p| p.write);
    let mut page = ctx.page(format!("{} {}", t.s("Refs"), repo.name));
    page.repo = Some(ctx.nav(
        &repo,
        "refs",
        None,
        permissions.as_ref().is_some_and(|p| p.admin),
    ));
    let body = RefsPage {
        page,
        groups,
        filter,
        csrf: ctx.csrf("refs"),
        can_create,
        default_from: lossy(&repo.record.default_branch),
    };
    Ok(ctx.html(&body, StatusCode::OK, false))
}
fn branch_ref(name: &str) -> Result<Vec<u8>> {
    let name = name.trim();
    let full = [b"refs/heads/".as_slice(), name.as_bytes()].concat();
    if name.is_empty()
        || name.contains(['\\', ' ', '~', '^', ':', '?', '*', '['])
        || name.contains("..")
    {
        return Err(WebError::BadRequest("invalid branch name".into()));
    }
    Ok(full)
}
async fn change_ref(ctx: &mut Ctx, repo: Repo) -> Result<Response> {
    ctx.require_account()?;
    let fields = ctx.form("refs").await?;
    let name = match fields.get("action") {
        Some("create") => {
            let name = branch_ref(&fields.text("name"))?;
            let from = ctx
                .resolve(&repo, fields.text("from").trim().as_bytes())
                .await
                .map_err(|_| WebError::BadRequest("unknown starting point".into()))?;
            let current = rpc!(
                ctx,
                metadata.resolve_ref(p::RefRequest {
                    repo: repo.bytes(),
                    name: name.clone()
                })
            )
            .await
            .read()?;
            if current.target.is_some() {
                return Err(WebError::Conflict("the branch already exists".into()));
            }
            let updated = ctx
                .upstream()
                .writer()
                .update_ref(ctx.wreq(p::UpdateRefRequest {
                    repo: repo.bytes(),
                    name: name.clone(),
                    expected: Some(p::Ref {
                        name: name.clone(),
                        target: None,
                        version: current.version,
                    }),
                    target: Some(from.id),
                    force: false,
                }))
                .await
                .wrote()?
                .into_inner();
            if !updated.applied {
                return Err(WebError::Conflict("the branch changed meanwhile".into()));
            }
            name
        }
        Some("delete") => {
            let name = fields.hex("ref")?;
            let target = fields.hex("target")?;
            let version = fields
                .text("version")
                .parse()
                .map_err(|_| WebError::BadRequest("invalid version".into()))?;
            let updated = ctx
                .upstream()
                .writer()
                .update_ref(ctx.wreq(p::UpdateRefRequest {
                    repo: repo.bytes(),
                    name: name.clone(),
                    expected: Some(p::Ref {
                        name: name.clone(),
                        target: Some(target),
                        version,
                    }),
                    target: None,
                    force: false,
                }))
                .await
                .wrote()?
                .into_inner();
            if !updated.applied {
                return Err(WebError::Conflict(
                    "the ref changed since the page was shown".into(),
                ));
            }
            name
        }
        _ => return Err(WebError::BadRequest("unknown action".into())),
    };
    tracing::info!(account = ?ctx.viewer.account(), repository = %repo.name, reference = %lossy(&name), "ref changed through the web UI");
    let url = repo.url("refs", None, None);
    Ok(ctx.redirect(&url))
}

#[derive(Template)]
#[template(path = "edit.html")]
pub struct EditPage {
    pub page: Page,
    pub mode: &'static str,
    pub action: String,
    pub csrf: String,
    pub key: String,
    pub at: String,
    pub base: String,
    pub original_path: String,
    pub original_blob: String,
    pub path: String,
    pub path_editable: bool,
    pub content: String,
    pub crlf: bool,
    pub final_newline: bool,
    pub executable: bool,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub destination: String,
    pub direct_allowed: bool,
    pub review_allowed: bool,
    pub new_branch: String,
    pub branch: String,
    pub review_number: String,
    pub error: Option<String>,
    pub conflicts: Vec<String>,
    pub changed_warning: bool,
    pub retry: bool,
    pub cancel: String,
}
/// Defaults for the signature of Web commits: proxy headers, then the
/// account profile, then `<account>@<default_email_domain>`.
async fn signature_defaults(ctx: &Ctx) -> (String, String) {
    if let Identity::Proxy {
        name,
        email,
        account,
        ..
    } = &ctx.viewer.identity
        && let (Some(name), Some(email)) = (
            name.clone().or_else(|| Some(account.clone())),
            email.clone(),
        )
    {
        return (name, email);
    }
    let profile = rpc!(ctx, identity.who_am_i(p::WhoAmIRequest {})).await.ok();
    let account = ctx.viewer.account().unwrap_or("anonymous").to_owned();
    let name = profile
        .as_ref()
        .map(|p| lossy(&p.display_name))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| account.clone());
    let email = profile
        .as_ref()
        .map(|p| lossy(&p.email))
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| format!("{account}@{}", ctx.app.config.file.default_email_domain));
    (name, email)
}
pub async fn write(
    ctx: &mut Ctx,
    name: String,
    view: String,
    rest: Vec<Vec<u8>>,
    post: bool,
) -> Result<Response> {
    ctx.require_account()?;
    let mut rest = rest.into_iter();
    let rev = rest.next().ok_or(WebError::NotFound)?;
    let path = rev::join(&rest.collect::<Vec<_>>()).ok_or(WebError::NotFound)?;
    let repo = ctx.repo(&name).await?;
    let resolved = ctx.resolve(&repo, &rev).await?;
    let branch = resolved
        .reference
        .clone()
        .filter(|r| r.starts_with(b"refs/heads/"));
    let review = ctx.q("review").and_then(|n| n.parse::<u64>().ok());
    if branch.is_none() && review.is_none() {
        return Err(WebError::BadRequest(
            "edit a branch, not a fixed changeset".into(),
        ));
    }
    match (view.as_str(), post) {
        ("upload", true) => upload(ctx, repo, resolved, path).await,
        (_, true) => submit(ctx, repo, resolved, view, path).await,
        ("edit", false) => edit_form(ctx, &repo, &resolved, path, review).await,
        ("new", false) => new_form(ctx, &repo, &resolved, path, review).await,
        ("upload", false) => upload_form(ctx, &repo, &resolved, path).await,
        ("delete", false) => delete_form(ctx, &repo, &resolved, path).await,
        _ => Err(WebError::NotFound),
    }
}
async fn form_base(
    ctx: &Ctx,
    repo: &Repo,
    resolved: &Resolved,
    mode: &'static str,
    path: &[u8],
    review: Option<u64>,
) -> Result<EditPage> {
    let branch = resolved.reference.clone().unwrap_or_default();
    let permissions = ctx.permissions(repo, std::slice::from_ref(&branch)).await;
    let protected = permissions
        .as_ref()
        .and_then(|p| p.refs.first())
        .is_some_and(|r| r.review_required);
    let review_allowed = permissions.as_ref().is_some_and(|p| p.review);
    let (author_name, author_email) = signature_defaults(ctx).await;
    let t = ctx.t;
    let mut page = ctx.page(t.s(match mode {
        "edit" => "Edit file",
        "new" => "New file",
        "delete" => "Delete file",
        _ => "Upload files",
    }));
    let mut nav = ctx.nav(
        repo,
        "code",
        Some(resolved),
        permissions.as_ref().is_some_and(|p| p.admin),
    );
    nav.crumbs = ctx.crumbs(repo, resolved.segment(), path);
    page.repo = Some(nav);
    let segment = resolved.segment().to_vec();
    let mut action = repo.url(mode, Some(&segment), Some(path));
    if let Some(number) = review {
        action.push_str(&format!("?review={number}"));
    }
    Ok(EditPage {
        page,
        mode,
        action,
        csrf: ctx.csrf("write"),
        key: Ctx::idempotency_key(),
        at: ctx.now.to_string(),
        base: hex::encode(&resolved.id),
        original_path: hex::encode(path),
        original_blob: String::new(),
        path: lossy(path),
        path_editable: std::str::from_utf8(path).is_ok(),
        content: String::new(),
        crlf: false,
        final_newline: true,
        executable: false,
        message: String::new(),
        author_name,
        author_email,
        destination: if review.is_some() {
            "revision".into()
        } else if protected {
            "review".into()
        } else {
            "direct".into()
        },
        direct_allowed: !protected,
        review_allowed,
        new_branch: String::new(),
        branch: branch
            .strip_prefix(b"refs/heads/")
            .map(lossy)
            .unwrap_or_default(),
        review_number: review.map(|n| n.to_string()).unwrap_or_default(),
        error: None,
        conflicts: Vec::new(),
        changed_warning: false,
        retry: false,
        cancel: repo.url(
            if mode == "edit" || mode == "delete" {
                "blob"
            } else {
                "tree"
            },
            Some(&segment),
            Some(path),
        ),
    })
}
async fn edit_form(
    ctx: &mut Ctx,
    repo: &Repo,
    resolved: &Resolved,
    path: Vec<u8>,
    review: Option<u64>,
) -> Result<Response> {
    let entry = browse::entry_at(ctx, repo, &resolved.id, &path).await?;
    let Some(p::entry::Kind::File(file)) = entry.kind else {
        return Err(WebError::BadRequest(
            "only regular files are edited here".into(),
        ));
    };
    let metadata = browse::blob_size(ctx, repo, &file.blob).await?;
    if metadata.size > ctx.app.config.file.limits.upload_bytes {
        return Err(WebError::BadRequest("the file is too large to edit".into()));
    }
    let content = browse::read_file(ctx, repo, &resolved.id, &path)
        .await
        .read()?
        .content;
    let Ok(text) = String::from_utf8(content.clone()) else {
        return Err(WebError::BadRequest(
            "only UTF-8 text files are edited here".into(),
        ));
    };
    let mut form = form_base(ctx, repo, resolved, "edit", &path, review).await?;
    form.crlf = text.contains("\r\n");
    form.final_newline = text.ends_with('\n') || text.is_empty();
    form.executable = file.executable;
    form.content = text.replace("\r\n", "\n");
    if form.final_newline {
        form.content.pop();
    }
    form.original_blob = hex::encode(&file.blob);
    form.message = format!("Update {}", lossy(&path));
    Ok(ctx.html(&form, StatusCode::OK, false))
}
async fn new_form(
    ctx: &mut Ctx,
    repo: &Repo,
    resolved: &Resolved,
    dir: Vec<u8>,
    review: Option<u64>,
) -> Result<Response> {
    let mut form = form_base(ctx, repo, resolved, "new", &dir, review).await?;
    form.path = if dir.is_empty() {
        String::new()
    } else {
        format!("{}/", lossy(&dir))
    };
    form.original_path = String::new();
    form.message = "Add a file".into();
    Ok(ctx.html(&form, StatusCode::OK, false))
}
async fn delete_form(
    ctx: &mut Ctx,
    repo: &Repo,
    resolved: &Resolved,
    path: Vec<u8>,
) -> Result<Response> {
    if path.is_empty() {
        return Err(WebError::BadRequest(
            "the repository root cannot be deleted".into(),
        ));
    }
    let mut form = form_base(ctx, repo, resolved, "delete", &path, None).await?;
    form.message = format!("Delete {}", lossy(&path));
    Ok(ctx.html(&form, StatusCode::OK, false))
}
async fn upload_form(
    ctx: &mut Ctx,
    repo: &Repo,
    resolved: &Resolved,
    dir: Vec<u8>,
) -> Result<Response> {
    let mut form = form_base(ctx, repo, resolved, "upload", &dir, None).await?;
    form.message = "Upload files".into();
    Ok(ctx.html(&form, StatusCode::OK, false))
}
/// Every file below `path` (a file is itself).
async fn files_below(ctx: &Ctx, repo: &Repo, id: &[u8], path: &[u8]) -> Result<Vec<Vec<u8>>> {
    if browse::entry_at(ctx, repo, id, path).await.is_ok() {
        return Ok(vec![path.to_vec()]);
    }
    let mut out = Vec::new();
    let mut pending = vec![path.to_vec()];
    while let Some(dir) = pending.pop() {
        for entry in browse::list_dir(ctx, repo, id, &dir).await?.entries {
            let full = [dir.as_slice(), b"/", &entry.name].concat();
            match entry.kind {
                Some(p::dir_entry::Kind::Tree(_)) => pending.push(full),
                Some(_) => out.push(full),
                None => {}
            }
            if out.len() > 10_000 {
                return Err(WebError::BadRequest(
                    "too many files to delete at once".into(),
                ));
            }
        }
    }
    Ok(out)
}
/// What a write form asks for, after validation.
struct Commit {
    changes: Vec<p::FileChange>,
    blobs: Vec<Vec<u8>>,
}
fn file_change(path: &[u8], content: &[u8], executable: bool) -> p::FileChange {
    p::FileChange {
        path: path.to_vec(),
        entry: Some(p::Entry {
            kind: Some(p::entry::Kind::File(p::File {
                blob: kasu_core::ObjectId::hash(content).0.to_vec(),
                executable,
            })),
        }),
    }
}
fn checked_path(text: &str) -> Result<Vec<u8>> {
    let path = text.trim().trim_matches('/').as_bytes().to_vec();
    kasu_core::RepoPath::new(path.clone())
        .map_err(|_| WebError::BadRequest("invalid path".into()))?;
    Ok(path)
}
async fn submit(
    ctx: &mut Ctx,
    repo: Repo,
    resolved: Resolved,
    view: String,
    path: Vec<u8>,
) -> Result<Response> {
    let fields = ctx.form("write").await?;
    let limit = ctx.app.config.file.limits.upload_bytes;
    let base = fields.hex("base")?;
    let commit = match view.as_str() {
        "edit" | "new" => {
            let original = fields.hex("original_path")?;
            let target = if fields.get("path").is_some() {
                checked_path(&fields.text("path"))?
            } else {
                original.clone()
            };
            let mut content = fields.text("content").replace("\r\n", "\n");
            while content.ends_with('\n') {
                content.pop();
            }
            if fields.flag("final_newline") {
                content.push('\n');
            }
            if fields.get("line_endings") == Some("crlf") {
                content = content.replace('\n', "\r\n");
            }
            if content.len() as u64 > limit {
                return Err(WebError::BadRequest(
                    "the file exceeds the size limit".into(),
                ));
            }
            if (view == "new" || target != original)
                && browse::entry_at(ctx, &repo, &base, &target).await.is_ok()
            {
                return rerender(
                    ctx,
                    &repo,
                    &resolved,
                    &fields,
                    &view,
                    "a file already exists at this path".into(),
                    Vec::new(),
                    StatusCode::CONFLICT,
                )
                .await;
            }
            let mut changes = vec![file_change(
                &target,
                content.as_bytes(),
                fields.flag("executable"),
            )];
            if view == "edit" && target != original {
                changes.push(p::FileChange {
                    path: original,
                    entry: None,
                });
            }
            Commit {
                changes,
                blobs: vec![content.into_bytes()],
            }
        }
        "delete" => {
            let files = files_below(ctx, &repo, &base, &path).await?;
            Commit {
                changes: files
                    .into_iter()
                    .map(|path| p::FileChange { path, entry: None })
                    .collect(),
                blobs: Vec::new(),
            }
        }
        _ => return Err(WebError::NotFound),
    };
    land(ctx, repo, resolved, fields, view, commit).await
}
async fn upload(ctx: &mut Ctx, repo: Repo, resolved: Resolved, dir: Vec<u8>) -> Result<Response> {
    let mut multipart = ctx.multipart().await?;
    let limits = ctx.app.config.file.limits.clone();
    let mut pairs = Vec::new();
    let mut files: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut total = 0u64;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| WebError::BadRequest("invalid upload".into()))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "files" {
            let file_name = field.file_name().unwrap_or_default().to_owned();
            let data = super::field_bytes(field, limits.upload_bytes).await?;
            if file_name.is_empty() && data.is_empty() {
                continue;
            }
            total += data.len() as u64;
            if total > limits.upload_total_bytes {
                return Err(WebError::BadRequest(
                    "the upload exceeds the total size limit".into(),
                ));
            }
            let base_name = file_name.rsplit(['/', '\\']).next().unwrap_or_default();
            let full = if dir.is_empty() {
                base_name.as_bytes().to_vec()
            } else {
                [dir.as_slice(), b"/", base_name.as_bytes()].concat()
            };
            kasu_core::RepoPath::new(full.clone())
                .map_err(|_| WebError::BadRequest("invalid file name".into()))?;
            files.push((full, data.to_vec()));
        } else {
            let value = super::field_bytes(field, 64 * 1024).await?;
            pairs.push((name, String::from_utf8_lossy(&value).into_owned()));
        }
    }
    let fields = Fields(pairs);
    ctx.check("write", fields.get("csrf"))?;
    if files.is_empty() {
        return Err(WebError::BadRequest("choose at least one file".into()));
    }
    let base = fields.hex("base")?;
    if !fields.flag("overwrite") {
        for (path, _) in &files {
            if browse::entry_at(ctx, &repo, &base, path).await.is_ok() {
                return Err(WebError::Conflict(format!(
                    "{} exists; confirm overwriting",
                    lossy(path)
                )));
            }
        }
    }
    let commit = Commit {
        changes: files
            .iter()
            .map(|(path, data)| file_change(path, data, false))
            .collect(),
        blobs: files.into_iter().map(|(_, data)| data).collect(),
    };
    land(ctx, repo, resolved, fields, "upload".into(), commit).await
}
#[allow(clippy::too_many_arguments)]
async fn rerender(
    ctx: &mut Ctx,
    repo: &Repo,
    resolved: &Resolved,
    fields: &Fields,
    view: &str,
    error: String,
    conflicts: Vec<String>,
    status: StatusCode,
) -> Result<Response> {
    let mode = match view {
        "edit" => "edit",
        "new" => "new",
        "delete" => "delete",
        _ => "upload",
    };
    let original = fields.hex("original_path").unwrap_or_default();
    let review = fields.get("review").and_then(|n| n.parse().ok());
    let mut form = form_base(ctx, repo, resolved, mode, &original, review).await?;
    // Keep what the user typed, including the idempotency key.
    form.key = fields.text("key");
    form.at = fields.text("at");
    form.base = fields.text("base");
    form.original_path = fields.text("original_path");
    form.original_blob = fields.text("original_blob");
    if let Some(path) = fields.get("path") {
        form.path = path.to_owned();
    }
    form.content = fields.text("content").replace("\r\n", "\n");
    form.crlf = fields.get("line_endings") == Some("crlf");
    form.final_newline = fields.flag("final_newline");
    form.executable = fields.flag("executable");
    form.message = fields.text("message");
    form.author_name = fields.text("author_name");
    form.author_email = fields.text("author_email");
    form.destination = fields.text("destination");
    form.new_branch = fields.text("new_branch");
    form.retry = status == StatusCode::SERVICE_UNAVAILABLE;
    form.changed_warning = status == StatusCode::PRECONDITION_FAILED;
    form.error = Some(error);
    form.conflicts = conflicts;
    Ok(ctx.html(&form, status, false))
}
async fn land(
    ctx: &mut Ctx,
    repo: Repo,
    resolved: Resolved,
    fields: Fields,
    view: String,
    commit: Commit,
) -> Result<Response> {
    let message = fields.text("message");
    if message.trim().is_empty() {
        return rerender(
            ctx,
            &repo,
            &resolved,
            &fields,
            &view,
            "a commit message is required".into(),
            Vec::new(),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    let key = fields.text("key");
    if key.len() != 32 {
        return Err(WebError::BadRequest("invalid form key".into()));
    }
    // The form's issue time keeps a resubmission byte-identical.
    let at: i64 = fields
        .text("at")
        .parse()
        .map_err(|_| WebError::BadRequest("invalid form time".into()))?;
    let base = fields.hex("base")?;
    // Warn when the file changed on the branch since the form was opened.
    if view == "edit" && !fields.flag("confirmed") && resolved.id != base {
        let original = fields.hex("original_path")?;
        let current = browse::entry_at(ctx, &repo, &resolved.id, &original)
            .await
            .ok();
        let current_blob = match current.and_then(|e| e.kind) {
            Some(p::entry::Kind::File(f)) => hex::encode(f.blob),
            _ => String::new(),
        };
        let own = commit
            .blobs
            .first()
            .map(|data| hex::encode(kasu_core::ObjectId::hash(data).0))
            .unwrap_or_default();
        // Our own earlier submission of this form is not someone else's change.
        if current_blob != fields.text("original_blob") && current_blob != own {
            let warning = ctx
                .t
                .s("Edited since you opened it; the server will rebase your change.")
                .to_owned();
            return rerender(
                ctx,
                &repo,
                &resolved,
                &fields,
                &view,
                warning,
                Vec::new(),
                StatusCode::PRECONDITION_FAILED,
            )
            .await;
        }
    }
    let signature = p::Signature {
        name: fields.text("author_name").trim().as_bytes().to_vec(),
        email: fields.text("author_email").trim().as_bytes().to_vec(),
        timestamp: at,
        tz: b"+0000".to_vec(),
    };
    if signature.name.is_empty()
        || signature.email.is_empty()
        || signature.email.contains(&b'>')
        || signature.name.contains(&b'<')
    {
        return Err(WebError::BadRequest("a name and email are required".into()));
    }
    let uploads: Vec<p::BlobUpload> = commit
        .blobs
        .iter()
        .map(|data| p::BlobUpload {
            repo: repo.bytes(),
            id: kasu_core::ObjectId::hash(data).0.to_vec(),
            offset: 0,
            data: data.clone(),
            last: true,
        })
        .collect();
    if !uploads.is_empty() {
        ctx.upstream()
            .writer()
            .upload_blobs(ctx.wreq(tokio_stream::iter(uploads)))
            .await
            .wrote()?;
    }
    let mut changes = commit.changes;
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    if changes.windows(2).any(|w| w[0].path == w[1].path) {
        return Err(WebError::BadRequest("a path appears twice".into()));
    }
    let changeset = ctx
        .upstream()
        .writer()
        .create_changeset(ctx.wreq(p::CreateChangesetRequest {
            repo: repo.bytes(),
            parents: vec![base],
            changes,
            metadata: Some(p::CommitMetadata {
                author: Some(signature.clone()),
                committer: Some(signature),
                message: message.replace("\r\n", "\n").into_bytes(),
            }),
        }))
        .await
        .wrote()?
        .into_inner();
    let branch = resolved.reference.clone().unwrap_or_default();
    let destination = fields.text("destination");
    let title = message.lines().next().unwrap_or_default().to_owned();
    match destination.as_str() {
        "review" => {
            let review = ctx
                .upstream()
                .review()
                .create_review(
                    ctx.wreq(p::CreateReviewRequest {
                        repo: repo.bytes(),
                        target: branch,
                        changesets: vec![changeset.id],
                        title: title.into_bytes(),
                        description: message
                            .split_once('\n')
                            .map(|(_, rest)| rest.trim().as_bytes().to_vec())
                            .unwrap_or_default(),
                        idempotency_key: key.into_bytes(),
                    }),
                )
                .await
                .wrote()?
                .into_inner();
            let url = repo.url("reviews", Some(review.number.to_string().as_bytes()), None);
            Ok(ctx.redirect(&url))
        }
        "revision" => {
            let number: u64 = fields
                .text("review")
                .parse()
                .map_err(|_| WebError::BadRequest("invalid review".into()))?;
            let current = rpc!(
                ctx,
                review.get_review(p::GetReviewRequest {
                    repo: repo.bytes(),
                    number
                })
            )
            .await
            .read()?;
            let latest = current.revisions.last().ok_or(WebError::NotFound)?;
            let mut stack = latest.changesets.clone();
            if !stack.contains(&changeset.id) {
                stack.push(changeset.id.clone());
                ctx.upstream()
                    .review()
                    .add_revision(ctx.wreq(p::AddRevisionRequest {
                        repo: repo.bytes(),
                        number,
                        expected_revision: latest.number,
                        changesets: stack,
                    }))
                    .await
                    .wrote()?;
            }
            let url = repo.url("reviews", Some(number.to_string().as_bytes()), None);
            Ok(ctx.redirect(&url))
        }
        "branch" | "direct" => {
            let target = if destination == "branch" {
                let name = branch_ref(&fields.text("new_branch"))?;
                let current = rpc!(
                    ctx,
                    metadata.resolve_ref(p::RefRequest {
                        repo: repo.bytes(),
                        name: name.clone()
                    })
                )
                .await
                .read()?;
                if current.target.is_some() {
                    return rerender(
                        ctx,
                        &repo,
                        &resolved,
                        &fields,
                        &view,
                        "the branch already exists".into(),
                        Vec::new(),
                        StatusCode::CONFLICT,
                    )
                    .await;
                }
                name
            } else {
                branch
            };
            let landed = ctx
                .upstream()
                .writer()
                .land(ctx.wreq(p::LandRequest {
                    repo: repo.bytes(),
                    id: changeset.id.clone(),
                    name: target.clone(),
                    idempotency_key: key.into_bytes(),
                    ..Default::default()
                }))
                .await
                .wrote()?
                .into_inner();
            tracing::info!(account = ?ctx.viewer.account(), repository = %repo.name, outcome = landed.outcome, "web write landed");
            match p::LandOutcome::try_from(landed.outcome) {
                Ok(p::LandOutcome::Landed) => {
                    let id = landed.id.unwrap_or(changeset.id);
                    let segment = hex::encode(&id);
                    let shown = match view.as_str() {
                        "delete" | "upload" => repo.url("tree", Some(segment.as_bytes()), None),
                        _ => {
                            let file = commit_path(&fields);
                            repo.url("blob", Some(segment.as_bytes()), Some(&file))
                        }
                    };
                    Ok(ctx.redirect(&shown))
                }
                Ok(p::LandOutcome::Conflict) => {
                    let conflicts = landed
                        .conflict
                        .map(|c| c.conflicts.iter().map(|c| lossy(&c.path)).collect())
                        .unwrap_or_default();
                    rerender(
                        ctx,
                        &repo,
                        &resolved,
                        &fields,
                        &view,
                        ctx.t
                            .s("This change conflicts with the current branch.")
                            .into(),
                        conflicts,
                        StatusCode::CONFLICT,
                    )
                    .await
                }
                Ok(p::LandOutcome::Retry) => {
                    rerender(
                        ctx,
                        &repo,
                        &resolved,
                        &fields,
                        &view,
                        landed.message,
                        Vec::new(),
                        StatusCode::SERVICE_UNAVAILABLE,
                    )
                    .await
                }
                _ => Err(WebError::Forbidden(landed.message)),
            }
        }
        _ => Err(WebError::BadRequest("unknown destination".into())),
    }
}
fn commit_path(fields: &Fields) -> Vec<u8> {
    match fields.get("path") {
        Some(path) => path.trim().trim_matches('/').as_bytes().to_vec(),
        None => fields.hex("original_path").unwrap_or_default(),
    }
}
