use super::{Ctx, Link, Page, ReadExt, Repo, Resolved, Result, WebError};
use crate::{
    render::{
        diff::{self, Row},
        markdown,
        text::{self, html, lossy},
        time,
    },
    rev,
};
use askama::Template;
use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use kasu_proto::v1 as p;
use prost::Message;

#[derive(Template)]
#[template(path = "repositories.html")]
struct RepositoriesPage {
    page: Page,
    prefix: String,
    items: Vec<Item>,
    next: Option<String>,
    can_create: bool,
}
pub struct Item {
    pub name: String,
    pub url: String,
    pub badge: String,
}
pub async fn repositories(ctx: &mut Ctx) -> Result<Response> {
    let prefix = ctx.q("prefix").unwrap_or_default().to_owned();
    let cursor = ctx.q("cursor").unwrap_or_default().to_owned();
    let listed = rpc!(
        ctx,
        admin.list_repositories(p::ListRepositoriesRequest {
            prefix: prefix.clone(),
            cursor: cursor.clone(),
            limit: 100,
        })
    )
    .await
    .read()?;
    let t = ctx.t;
    let can_create = match ctx.viewer.signed_in() || !ctx.app.upstream.delegated() {
        true => rpc!(ctx, identity.who_am_i(p::WhoAmIRequest {}))
            .await
            .is_ok_and(|me| me.operator || !me.create_repositories.is_empty()),
        false => false,
    };
    let items = listed
        .repositories
        .iter()
        .map(|r| Item {
            url: rev::repo_url(&r.name),
            badge: match p::RepositoryState::try_from(r.state) {
                Ok(p::RepositoryState::ReadOnly) => t.s("Read-only").into(),
                Ok(p::RepositoryState::Archived) => t.s("Archived").into(),
                _ => String::new(),
            },
            name: r.name.clone(),
        })
        .collect();
    let next = (!listed.cursor.is_empty()).then(|| {
        format!(
            "/?prefix={}&cursor={}",
            rev::segment(prefix.as_bytes()),
            rev::segment(listed.cursor.as_bytes())
        )
    });
    let page = RepositoriesPage {
        page: ctx.page(t.s("Repositories")),
        prefix,
        items,
        next,
        can_create,
    };
    Ok(ctx.html(&page, StatusCode::OK, false))
}

#[derive(Template)]
#[template(path = "tree.html")]
struct TreePage {
    page: Page,
    rows: Vec<EntryRow>,
    truncated: Option<usize>,
    latest: Option<LogRow>,
    readme: Option<String>,
    actions: Vec<Link>,
    filtered: bool,
    empty: Option<EmptyRepo>,
    /// The URL that adds each entry's latest change, when not yet shown.
    latest_url: Option<String>,
}
pub struct EntryRow {
    pub icon: &'static str,
    pub name: String,
    pub url: String,
    pub note: String,
    pub latest: Option<LogRow>,
}
/// Entries whose latest change is looked up; beyond this the column is empty.
const LATEST_ENTRIES: usize = 100;
/// How long the per-entry lookups may take together.
const LATEST_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
pub struct EmptyRepo {
    pub clone: Vec<Link>,
    pub create: Option<String>,
}
#[derive(Clone)]
pub struct LogRow {
    pub short: String,
    pub url: String,
    pub summary: String,
    pub author: String,
    pub when: String,
    pub iso: String,
    pub merge: bool,
    pub git: String,
}
pub fn log_row(ctx: &Ctx, repo: &Repo, cs: &p::Changeset) -> LogRow {
    let author = cs.author.clone().unwrap_or_default();
    LogRow {
        short: text::short(&cs.id),
        url: repo.url("changeset", Some(hex::encode(&cs.id).as_bytes()), None),
        summary: {
            let line = text::first_line(&cs.message);
            if line.is_empty() {
                ctx.t.s("(no message)").to_owned()
            } else {
                line
            }
        },
        author: lossy(&author.name),
        when: time::absolute(author.timestamp, &author.tz, ctx.prefs.tz.as_deref()),
        iso: time::iso(author.timestamp),
        merge: cs.parents.len() > 1,
        git: cs
            .raw_git_commit
            .as_ref()
            .map(|raw| git_oid(raw))
            .unwrap_or_default(),
    }
}
fn git_oid(raw: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hash = Sha1::new();
    hash.update(format!("commit {}\0", raw.len()));
    hash.update(raw);
    hex::encode(hash.finalize())
}

pub async fn repository(ctx: &mut Ctx, name: String) -> Result<Response> {
    let repo = ctx.repo(&name).await?;
    let branch = repo.record.default_branch.clone();
    match ctx.resolve(&repo, &branch).await {
        Ok(resolved) => tree_at(ctx, repo, resolved, Vec::new()).await,
        Err(WebError::NotFound) => {
            let permissions = ctx.permissions(&repo, &[repo.default_ref()]).await;
            let mut nav = ctx.nav(
                &repo,
                "code",
                None,
                permissions.as_ref().is_some_and(|p| p.admin),
            );
            let clone = nav.clone.clone();
            nav.tabs.truncate(nav.tabs.len());
            let mut page = ctx.page(&repo.name);
            page.repo = Some(nav);
            let writable = permissions.is_some_and(|p| p.write);
            let tree = TreePage {
                page,
                rows: Vec::new(),
                truncated: None,
                latest: None,
                readme: None,
                actions: Vec::new(),
                filtered: false,
                empty: Some(EmptyRepo {
                    clone,
                    create: writable.then(|| repo.url("new", Some(&branch), None)),
                }),
                latest_url: None,
            };
            Ok(ctx.html(&tree, StatusCode::OK, false))
        }
        Err(error) => Err(error),
    }
}
fn split_rev(rest: Vec<Vec<u8>>) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut rest = rest.into_iter();
    let rev = rest.next().ok_or(WebError::NotFound)?;
    let path = rev::join(&rest.collect::<Vec<_>>()).ok_or(WebError::NotFound)?;
    if !path.is_empty() && kasu_core::RepoPath::new(path.clone()).is_err() {
        return Err(WebError::BadRequest("invalid path".into()));
    }
    Ok((rev, path))
}
pub async fn tree(ctx: &mut Ctx, name: String, rest: Vec<Vec<u8>>) -> Result<Response> {
    let (rev, path) = split_rev(rest)?;
    let repo = ctx.repo(&name).await?;
    let resolved = ctx.resolve(&repo, &rev).await?;
    tree_at(ctx, repo, resolved, path).await
}
fn is_readme(name: &[u8]) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    match lower.as_slice() {
        b"readme.md" => Some(0),
        b"readme" => Some(1),
        _ if lower.starts_with(b"readme.") => Some(2),
        _ => None,
    }
}
async fn tree_at(ctx: &mut Ctx, repo: Repo, resolved: Resolved, path: Vec<u8>) -> Result<Response> {
    let listing = list_dir(ctx, &repo, &resolved.id, &path).await?;
    let rev = resolved.segment().to_vec();
    let join = |name: &[u8]| -> Vec<u8> {
        if path.is_empty() {
            name.to_vec()
        } else {
            [path.as_slice(), b"/", name].concat()
        }
    };
    let t = ctx.t;
    let mut entries = listing.entries.clone();
    entries.sort_by(|a, b| {
        let dir = |e: &p::DirEntry| !matches!(e.kind, Some(p::dir_entry::Kind::Tree(_)));
        dir(a).cmp(&dir(b)).then_with(|| a.name.cmp(&b.name))
    });
    let limit = ctx.app.config.file.limits.dir_entries;
    let truncated = (entries.len() > limit).then_some(entries.len());
    let mut readme: Option<(u8, Vec<u8>)> = None;
    let mut rows: Vec<EntryRow> = entries
        .iter()
        .take(limit)
        .map(|entry| {
            let full = join(&entry.name);
            let (icon, url, note) = match &entry.kind {
                Some(p::dir_entry::Kind::Tree(_)) => (
                    "📁",
                    repo.url("tree", Some(&rev), Some(&full)),
                    String::new(),
                ),
                Some(p::dir_entry::Kind::Entry(p::Entry {
                    kind: Some(p::entry::Kind::File(file)),
                })) => {
                    if let Some(rank) = is_readme(&entry.name)
                        && readme.as_ref().is_none_or(|(r, _)| rank < *r)
                    {
                        readme = Some((rank, full.clone()));
                    }
                    (
                        "📄",
                        repo.url("blob", Some(&rev), Some(&full)),
                        if file.executable {
                            t.s("Executable").to_owned()
                        } else {
                            String::new()
                        },
                    )
                }
                Some(p::dir_entry::Kind::Entry(p::Entry {
                    kind: Some(p::entry::Kind::Symlink(_)),
                })) => (
                    "🔗",
                    repo.url("blob", Some(&rev), Some(&full)),
                    "symlink".into(),
                ),
                Some(p::dir_entry::Kind::Entry(p::Entry {
                    kind: Some(p::entry::Kind::Gitlink(commit)),
                })) => (
                    "📦",
                    String::new(),
                    format!("{} {}", t.s("gitlink"), hex::encode(commit)),
                ),
                _ => ("?", String::new(), String::new()),
            };
            EntryRow {
                icon,
                name: lossy(&entry.name),
                url,
                note,
                latest: None,
            }
        })
        .collect();
    let latest = rpc!(
        ctx,
        metadata.log(p::LogRequest {
            repo: repo.bytes(),
            start: resolved.id.clone(),
            path: (!path.is_empty()).then(|| path.clone()),
            limit: 1,
            cursor: Vec::new(),
        })
    )
    .await
    .ok()
    .and_then(|log| log.changesets.first().map(|cs| log_row(ctx, &repo, cs)));
    let with_latest = ctx.q("latest") == Some("1");
    if with_latest {
        // Concurrent and bounded in time: entries whose history is still
        // being derived stay blank instead of holding the page.
        let mut tasks = tokio::task::JoinSet::new();
        for (index, entry) in entries
            .iter()
            .take(LATEST_ENTRIES.min(rows.len()))
            .enumerate()
        {
            let request = ctx.req(p::LogRequest {
                repo: repo.bytes(),
                start: resolved.id.clone(),
                path: Some(join(&entry.name)),
                limit: 1,
                cursor: Vec::new(),
            });
            let mut client = ctx.upstream().metadata();
            tasks.spawn(async move { (index, client.log(request).await) });
        }
        let deadline = tokio::time::Instant::now() + LATEST_BUDGET;
        while let Ok(Some(joined)) = tokio::time::timeout_at(deadline, tasks.join_next()).await {
            if let Ok((index, Ok(log))) = joined
                && let Some(cs) = log.into_inner().changesets.first()
            {
                rows[index].latest = Some(log_row(ctx, &repo, cs));
            }
        }
        tasks.abort_all();
    }
    let readme = match readme {
        Some((_, file)) => render_readme(ctx, &repo, &resolved, &file).await,
        None => None,
    };
    let branch_ref = resolved.reference.clone();
    let permissions = ctx
        .permissions(&repo, &branch_ref.iter().cloned().collect::<Vec<_>>())
        .await;
    let mut actions = Vec::new();
    if let Some(perms) = &permissions
        && perms.write
        && branch_ref
            .as_ref()
            .is_some_and(|r| r.starts_with(b"refs/heads/"))
    {
        actions.push(Link::new(
            t.s("New file"),
            repo.url("new", Some(&rev), Some(&path)),
        ));
        actions.push(Link::new(
            t.s("Upload files"),
            repo.url("upload", Some(&rev), Some(&path)),
        ));
    }
    let archive = repo.url("archive", Some(&rev), Some(&path));
    actions.push(Link::new("tar.gz", archive.clone()));
    actions.push(Link::new("zip", format!("{archive}?format=zip")));
    let admin = permissions.as_ref().is_some_and(|p| p.admin);
    let mut nav = ctx.nav(&repo, "code", Some(&resolved), admin);
    nav.crumbs = ctx.crumbs(&repo, &rev, &path);
    let mut page = ctx.page(if path.is_empty() {
        repo.name.clone()
    } else {
        lossy(&path)
    });
    page.repo = Some(nav);
    let tree = TreePage {
        page,
        rows,
        truncated,
        latest,
        readme,
        actions,
        filtered: listing.read_filtered,
        empty: None,
        latest_url: (!with_latest)
            .then(|| format!("{}?latest=1", repo.url("tree", Some(&rev), Some(&path)))),
    };
    let immutable = resolved.immutable();
    Ok(ctx.html(&tree, StatusCode::OK, immutable))
}
pub struct Listing {
    pub entries: Vec<p::DirEntry>,
    pub read_filtered: bool,
}
/// List a directory; cached by Changeset ID and the current read scope.
pub async fn list_dir(ctx: &Ctx, repo: &Repo, id: &[u8], path: &[u8]) -> Result<Listing> {
    let key = crate::cache::Cache::key(
        ctx.caller().act_as.as_deref(),
        "list_dir",
        &[&repo.record.key, &repo.cache_scope, id, path],
    );
    let root = cached(&ctx.app, key.clone(), || async {
        rpc!(
            ctx,
            metadata.get_root_tree(p::ChangesetRequest {
                repo: repo.bytes(),
                id: id.to_vec(),
            })
        )
        .await
    })
    .await;
    let read_filtered = root
        .as_ref()
        .map(|r: &p::RootTreeResponse| r.read_filtered)
        .unwrap_or(false);
    let key = crate::cache::Cache::key(
        ctx.caller().act_as.as_deref(),
        "entries",
        &[&repo.record.key, &repo.cache_scope, id, path],
    );
    let listing: p::ListDirResponse = cached(&ctx.app, key, || async {
        rpc!(
            ctx,
            metadata.list_dir(p::PathRequest {
                repo: repo.bytes(),
                id: id.to_vec(),
                path: path.to_vec(),
                ..Default::default()
            })
        )
        .await
    })
    .await
    .read()?;
    Ok(Listing {
        entries: listing.entries,
        read_filtered,
    })
}
/// Fetch an immutable response through the per-account cache.
pub async fn cached<T, F, Fut>(
    app: &crate::App,
    key: Vec<u8>,
    fetch: F,
) -> std::result::Result<T, tonic::Status>
where
    T: Message + Default,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, tonic::Status>>,
{
    use std::sync::atomic::Ordering;
    if let Some(bytes) = app.cache.get(&key)
        && let Ok(value) = T::decode(bytes.as_slice())
    {
        app.metrics.cache[0].fetch_add(1, Ordering::Relaxed);
        return Ok(value);
    }
    app.metrics.cache[1].fetch_add(1, Ordering::Relaxed);
    let value = fetch().await?;
    app.cache.put(key, value.encode_to_vec());
    Ok(value)
}
pub async fn read_file(
    ctx: &Ctx,
    repo: &Repo,
    id: &[u8],
    path: &[u8],
) -> std::result::Result<p::ReadFileResponse, tonic::Status> {
    let key = crate::cache::Cache::key(
        ctx.caller().act_as.as_deref(),
        "read_file",
        &[&repo.record.key, &repo.cache_scope, id, path],
    );
    cached(&ctx.app, key, || async {
        rpc!(
            ctx,
            metadata.read_file(p::PathRequest {
                repo: repo.bytes(),
                id: id.to_vec(),
                path: path.to_vec(),
                ..Default::default()
            })
        )
        .await
    })
    .await
}
pub async fn get_changeset(ctx: &Ctx, repo: &Repo, id: &[u8]) -> Result<p::Changeset> {
    let key = crate::cache::Cache::key(
        ctx.caller().act_as.as_deref(),
        "changeset",
        &[&repo.record.key, &repo.cache_scope, id],
    );
    cached(&ctx.app, key, || async {
        rpc!(
            ctx,
            metadata.get_changeset(p::ChangesetRequest {
                repo: repo.bytes(),
                id: id.to_vec(),
            })
        )
        .await
    })
    .await
    .read()
}
/// Markdown with links resolved against the file's directory at the same
/// revision.
pub fn markdown_at(repo: &Repo, rev: &[u8], file: &[u8], source: &[u8]) -> String {
    let dir: Vec<u8> = match file.iter().rposition(|b| *b == b'/') {
        Some(at) => file[..at].to_vec(),
        None => Vec::new(),
    };
    let rewrite = |url: &str, image: bool| -> Option<String> {
        let (target, fragment) = url
            .split_once('#')
            .map(|(a, b)| (a, Some(b)))
            .unwrap_or((url, None));
        let target = percent_encoding::percent_decode_str(target).collect::<Vec<u8>>();
        let mut parts: Vec<Vec<u8>> = if dir.is_empty() {
            Vec::new()
        } else {
            dir.split(|b| *b == b'/').map(<[u8]>::to_vec).collect()
        };
        for segment in target.split(|b| *b == b'/') {
            match segment {
                b"" | b"." => {}
                b".." => {
                    parts.pop()?;
                }
                s => parts.push(s.to_vec()),
            }
        }
        let path = parts.join(&b'/');
        let view = if image {
            "raw"
        } else if url.ends_with('/') {
            "tree"
        } else {
            "blob"
        };
        let mut link = repo.url(view, Some(rev), Some(&path));
        if let Some(fragment) = fragment {
            link.push('#');
            link.push_str(&rev::segment(fragment.as_bytes()));
        }
        Some(link)
    };
    markdown::render(source, Some(&rewrite))
}
async fn render_readme(ctx: &Ctx, repo: &Repo, resolved: &Resolved, file: &[u8]) -> Option<String> {
    let response = read_file(ctx, repo, &resolved.id, file).await.ok()?;
    if response.content.len() as u64 > ctx.app.config.file.limits.file_view_bytes {
        return None;
    }
    let lower = file.to_ascii_lowercase();
    Some(
        if lower.ends_with(b".md") || lower.ends_with(b".markdown") {
            markdown_at(repo, resolved.segment(), file, &response.content)
        } else {
            format!("<pre>{}</pre>", html(&response.content))
        },
    )
}

#[derive(Template)]
#[template(path = "blob.html")]
struct BlobPage {
    page: Page,
    path: String,
    size: String,
    git_oid: String,
    executable: bool,
    lines: Vec<String>,
    markdown: Option<String>,
    image: Option<String>,
    binary: bool,
    too_large: bool,
    symlink: Option<Link>,
    gitlink: Option<String>,
    actions: Vec<Link>,
    source_toggle: Option<Link>,
}
fn image_type(path: &[u8]) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    [
        (".png", "image/png"),
        (".jpg", "image/jpeg"),
        (".jpeg", "image/jpeg"),
        (".gif", "image/gif"),
        (".webp", "image/webp"),
    ]
    .into_iter()
    .find(|(ext, _)| lower.ends_with(ext.as_bytes()))
    .map(|(_, t)| t)
}
/// The entry at `path` from its parent listing.
pub async fn entry_at(ctx: &Ctx, repo: &Repo, id: &[u8], path: &[u8]) -> Result<p::Entry> {
    let (parent, name) = match path.iter().rposition(|b| *b == b'/') {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => (&b""[..], path),
    };
    let listing = list_dir(ctx, repo, id, parent).await?;
    match listing
        .entries
        .into_iter()
        .find(|e| e.name == name)
        .and_then(|e| e.kind)
    {
        Some(p::dir_entry::Kind::Entry(entry)) => Ok(entry),
        _ => Err(WebError::NotFound),
    }
}
pub async fn blob_size(ctx: &Ctx, repo: &Repo, blob: &[u8]) -> Result<p::BlobMetadata> {
    let response = rpc!(
        ctx,
        metadata.get_blob_metadata(p::ObjectRequest {
            repo: repo.bytes(),
            ids: vec![blob.to_vec()],
        })
    )
    .await
    .read()?;
    response.blobs.into_iter().next().ok_or(WebError::NotFound)
}
pub async fn blob(ctx: &mut Ctx, name: String, rest: Vec<Vec<u8>>) -> Result<Response> {
    let (rev, path) = split_rev(rest)?;
    if path.is_empty() {
        return Err(WebError::NotFound);
    }
    let repo = ctx.repo(&name).await?;
    let resolved = ctx.resolve(&repo, &rev).await?;
    let entry = entry_at(ctx, &repo, &resolved.id, &path).await?;
    let t = ctx.t;
    let limits = ctx.app.config.file.limits.clone();
    let mut page_data = BlobPage {
        page: ctx.page(lossy(&path)),
        path: lossy(&path),
        size: String::new(),
        git_oid: String::new(),
        executable: false,
        lines: Vec::new(),
        markdown: None,
        image: None,
        binary: false,
        too_large: false,
        symlink: None,
        gitlink: None,
        actions: Vec::new(),
        source_toggle: None,
    };
    let raw_url = repo.url("raw", Some(&rev), Some(&path));
    let mut text_file = false;
    match entry.kind {
        Some(p::entry::Kind::Gitlink(commit)) => page_data.gitlink = Some(hex::encode(commit)),
        Some(p::entry::Kind::Symlink(_)) => {
            let file = read_file(ctx, &repo, &resolved.id, &path).await.read()?;
            let target = file.content;
            let dir = path
                .iter()
                .rposition(|b| *b == b'/')
                .map(|at| path[..at].to_vec())
                .unwrap_or_default();
            let mut parts: Vec<Vec<u8>> = if dir.is_empty() {
                Vec::new()
            } else {
                dir.split(|b| *b == b'/').map(<[u8]>::to_vec).collect()
            };
            let mut inside = !target.starts_with(b"/");
            for segment in target.split(|b| *b == b'/') {
                match segment {
                    b"" | b"." => {}
                    b".." => inside &= parts.pop().is_some(),
                    s => parts.push(s.to_vec()),
                }
            }
            let url = if inside && !parts.is_empty() {
                repo.url("blob", Some(&rev), Some(&parts.join(&b'/')))
            } else {
                String::new()
            };
            page_data.symlink = Some(Link::new(lossy(&target), url));
        }
        Some(p::entry::Kind::File(file)) => {
            page_data.executable = file.executable;
            let metadata = blob_size(ctx, &repo, &file.blob).await?;
            page_data.size = text::size(metadata.size);
            page_data.git_oid = hex::encode(&metadata.git_oid);
            if metadata.size > limits.file_view_bytes {
                page_data.too_large = true;
            } else if image_type(&path).is_some() {
                page_data.image = Some(raw_url.clone());
            } else {
                let content = read_file(ctx, &repo, &resolved.id, &path)
                    .await
                    .read()?
                    .content;
                if !text::is_text(&content) {
                    page_data.binary = true;
                } else {
                    text_file = true;
                    let lower = path.to_ascii_lowercase();
                    let is_markdown = lower.ends_with(b".md") || lower.ends_with(b".markdown");
                    if is_markdown && ctx.q("source").is_none() {
                        page_data.markdown = Some(markdown_at(&repo, &rev, &path, &content));
                        page_data.source_toggle = Some(Link::new(
                            t.s("Show source"),
                            format!("{}?source=1", repo.url("blob", Some(&rev), Some(&path))),
                        ));
                    } else {
                        if is_markdown {
                            page_data.source_toggle = Some(Link::new(
                                t.s("Preview"),
                                repo.url("blob", Some(&rev), Some(&path)),
                            ));
                        }
                        page_data.lines = code_lines(ctx, &path, &content).await;
                    }
                }
            }
        }
        None => return Err(WebError::NotFound),
    }
    page_data.actions.push(Link::new(t.s("Raw"), raw_url));
    page_data.actions.push(Link::new(
        t.s("Blame"),
        repo.url("blame", Some(&rev), Some(&path)),
    ));
    page_data.actions.push(Link::new(
        t.s("History"),
        repo.url("log", Some(&rev), Some(&path)),
    ));
    let permissions = ctx
        .permissions(
            &repo,
            &resolved.reference.iter().cloned().collect::<Vec<_>>(),
        )
        .await;
    if permissions.as_ref().is_some_and(|p| p.write)
        && resolved
            .reference
            .as_ref()
            .is_some_and(|r| r.starts_with(b"refs/heads/"))
    {
        if text_file {
            page_data.actions.push(Link::new(
                t.s("Edit"),
                repo.url("edit", Some(&rev), Some(&path)),
            ));
        }
        page_data.actions.push(Link::new(
            t.s("Delete"),
            repo.url("delete", Some(&rev), Some(&path)),
        ));
    }
    let mut nav = ctx.nav(
        &repo,
        "code",
        Some(&resolved),
        permissions.as_ref().is_some_and(|p| p.admin),
    );
    nav.crumbs = ctx.crumbs(&repo, &rev, &path);
    page_data.page.repo = Some(nav);
    let immutable = resolved.immutable();
    Ok(ctx.html(&page_data, StatusCode::OK, immutable))
}
/// Escaped (and, within limits, highlighted) lines of a text file.
pub async fn code_lines(ctx: &Ctx, path: &[u8], content: &[u8]) -> Vec<String> {
    let limits = &ctx.app.config.file.limits;
    let lines = text::lines(content);
    if content.len() as u64 <= limits.highlight_bytes
        && lines.len() <= limits.highlight_lines
        && let Ok(source) = std::str::from_utf8(content)
        && let Some(highlighted) = ctx.app.highlighter.lines(path, source.to_owned()).await
        && highlighted.len() == lines.len()
    {
        return highlighted;
    }
    lines.iter().map(|l| html(l)).collect()
}

pub async fn raw(ctx: &mut Ctx, name: String, rest: Vec<Vec<u8>>) -> Result<Response> {
    let (rev, path) = split_rev(rest)?;
    if path.is_empty() {
        return Err(WebError::NotFound);
    }
    let repo = ctx.repo(&name).await?;
    let resolved = ctx.resolve(&repo, &rev).await?;
    let file = read_file(ctx, &repo, &resolved.id, &path).await.read()?;
    let image = image_type(&path).filter(|_| {
        matches!(
            file.entry.as_ref().and_then(|e| e.kind.as_ref()),
            Some(p::entry::Kind::File(_))
        )
    });
    let content_type = match image {
        Some(kind) => kind,
        None if text::is_text(&file.content) => "text/plain; charset=utf-8",
        None => "application/octet-stream",
    };
    let filename = path.rsplit(|b| *b == b'/').next().unwrap_or_default();
    let ascii: String = lossy(filename)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let disposition = format!(
        "{}; filename=\"{ascii}\"; filename*=UTF-8''{}",
        if image.is_some() {
            "inline"
        } else {
            "attachment"
        },
        rev::segment(filename)
    );
    let mut response = (StatusCode::OK, file.content).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if resolved.immutable() {
            "private, max-age=3600"
        } else {
            "private, no-cache"
        }),
    );
    Ok(response)
}

#[derive(Template)]
#[template(path = "blame.html")]
struct BlamePage {
    page: Page,
    groups: Vec<BlameGroup>,
    too_large: bool,
    toggles: Vec<Link>,
}
pub struct BlameGroup {
    pub origin: LogRow,
    pub previous: String,
    pub lines: Vec<(u64, String)>,
}
pub async fn blame(ctx: &mut Ctx, name: String, rest: Vec<Vec<u8>>) -> Result<Response> {
    let (rev, path) = split_rev(rest)?;
    let repo = ctx.repo(&name).await?;
    let resolved = ctx.resolve(&repo, &rev).await?;
    let moves = ctx.q("moves") == Some("1");
    let copies = ctx.q("copies") == Some("1");
    let file = read_file(ctx, &repo, &resolved.id, &path).await.read()?;
    let t = ctx.t;
    let lines = text::lines(&file.content);
    let too_large =
        lines.len() > ctx.app.config.file.limits.blame_lines || !text::is_text(&file.content);
    let mut groups: Vec<BlameGroup> = Vec::new();
    if !too_large {
        let blame = rpc!(
            ctx,
            metadata.blame(p::PathRequest {
                repo: repo.bytes(),
                id: resolved.id.clone(),
                path: path.clone(),
                follow_copies: copies,
                follow_moves: moves || copies,
            })
        )
        .await
        .read()?;
        let highlighted = code_lines(ctx, &path, &file.content).await;
        let mut origins: std::collections::HashMap<Vec<u8>, p::Changeset> = Default::default();
        for (index, origin) in blame.lines.iter().enumerate() {
            let line = highlighted.get(index).cloned().unwrap_or_default();
            match groups.last_mut() {
                Some(group) if group.origin.url.ends_with(&hex::encode(&origin.changeset)) => {
                    group.lines.push((index as u64 + 1, line));
                }
                _ => {
                    if !origins.contains_key(&origin.changeset) {
                        let cs = get_changeset(ctx, &repo, &origin.changeset).await?;
                        origins.insert(origin.changeset.clone(), cs);
                    }
                    let cs = &origins[&origin.changeset];
                    let previous = cs
                        .parents
                        .first()
                        .map(|parent| {
                            repo.url(
                                "blame",
                                Some(hex::encode(parent).as_bytes()),
                                Some(&origin.path),
                            )
                        })
                        .unwrap_or_default();
                    groups.push(BlameGroup {
                        origin: log_row(ctx, &repo, cs),
                        previous,
                        lines: vec![(index as u64 + 1, line)],
                    });
                }
            }
        }
    }
    let base = repo.url("blame", Some(&rev), Some(&path));
    let toggles = vec![
        Link {
            label: "moves".into(),
            url: if moves {
                base.clone()
            } else {
                format!("{base}?moves=1")
            },
            on: moves,
        },
        Link {
            label: "copies".into(),
            url: if copies {
                base.clone()
            } else {
                format!("{base}?copies=1")
            },
            on: copies,
        },
    ];
    let permissions = ctx.permissions(&repo, &[]).await;
    let mut nav = ctx.nav(
        &repo,
        "code",
        Some(&resolved),
        permissions.is_some_and(|p| p.admin),
    );
    nav.crumbs = ctx.crumbs(&repo, &rev, &path);
    let mut page = ctx.page(format!("{} {}", t.s("Blame"), lossy(&path)));
    page.repo = Some(nav);
    let immutable = resolved.immutable();
    Ok(ctx.html(
        &BlamePage {
            page,
            groups,
            too_large,
            toggles,
        },
        StatusCode::OK,
        immutable,
    ))
}

#[derive(Template)]
#[template(path = "log.html")]
struct LogPage {
    page: Page,
    rows: Vec<LogRow>,
    next: Option<String>,
    path: String,
}
pub async fn log(ctx: &mut Ctx, name: String, rest: Vec<Vec<u8>>) -> Result<Response> {
    let (rev, path) = split_rev(rest)?;
    let repo = ctx.repo(&name).await?;
    let resolved = ctx.resolve(&repo, &rev).await?;
    let cursor = hex::decode(ctx.q("cursor").unwrap_or_default())
        .map_err(|_| WebError::BadRequest("invalid cursor".into()))?;
    let log = rpc!(
        ctx,
        metadata.log(p::LogRequest {
            repo: repo.bytes(),
            start: resolved.id.clone(),
            path: (!path.is_empty()).then(|| path.clone()),
            limit: 50,
            cursor: cursor.clone(),
        })
    )
    .await
    .read()?;
    let rows = log
        .changesets
        .iter()
        .map(|cs| log_row(ctx, &repo, cs))
        .collect();
    let next = (!log.cursor.is_empty()).then(|| {
        format!(
            "{}?cursor={}",
            repo.url("log", Some(&rev), Some(&path)),
            hex::encode(&log.cursor)
        )
    });
    let permissions = ctx.permissions(&repo, &[]).await;
    let mut nav = ctx.nav(
        &repo,
        "log",
        Some(&resolved),
        permissions.is_some_and(|p| p.admin),
    );
    nav.crumbs = ctx.crumbs(&repo, &rev, &path);
    let mut page = ctx.page(format!("{} {}", ctx.t.s("History"), repo.name));
    page.repo = Some(nav);
    let immutable = resolved.immutable() && cursor.is_empty();
    Ok(ctx.html(
        &LogPage {
            page,
            rows,
            next,
            path: lossy(&path),
        },
        StatusCode::OK,
        immutable,
    ))
}

/// One file of a diff, ready to render.
pub struct FileDiffData {
    pub path: Vec<u8>,
    pub status: &'static str,
    pub binary: bool,
    /// None when collapsed (too many changed lines).
    pub rows: Option<Vec<Row>>,
}
fn status_of(file: &p::FileDiff) -> &'static str {
    let kind = |e: &Option<p::Entry>| {
        e.as_ref()
            .and_then(|e| e.kind.as_ref())
            .map(std::mem::discriminant)
    };
    match (&file.before, &file.after) {
        (None, Some(_)) => "Added",
        (Some(_), None) => "Deleted",
        _ if kind(&file.before) != kind(&file.after) => "Type changed",
        (
            Some(p::Entry {
                kind: Some(p::entry::Kind::File(a)),
            }),
            Some(p::Entry {
                kind: Some(p::entry::Kind::File(b)),
            }),
        ) if a.blob == b.blob && a.executable != b.executable => "Mode changed",
        _ => "Changed",
    }
}
/// The files changed between two changesets: one page of the file list, with
/// content for the files of that page.
pub async fn diff_files(
    ctx: &Ctx,
    repo: &Repo,
    base: &[u8],
    head: &[u8],
    filter: &[Vec<u8>],
    page: usize,
    expand: Option<&[u8]>,
) -> Result<(Vec<FileDiffData>, usize)> {
    let limits = ctx.app.config.file.limits.clone();
    let listing = rpc!(
        ctx,
        metadata.diff(p::DiffRequest {
            repo: repo.bytes(),
            base: base.to_vec(),
            head: head.to_vec(),
            paths: filter.to_vec(),
            content: false,
        })
    )
    .await
    .read()?;
    let total = listing.files.len();
    let shown: Vec<Vec<u8>> = listing
        .files
        .iter()
        .skip(page * limits.diff_files)
        .take(limits.diff_files)
        .map(|f| f.path.clone())
        .collect();
    if shown.is_empty() {
        return Ok((Vec::new(), total));
    }
    let content = rpc!(
        ctx,
        metadata.diff(p::DiffRequest {
            repo: repo.bytes(),
            base: base.to_vec(),
            head: head.to_vec(),
            paths: shown.clone(),
            content: true,
        })
    )
    .await
    .read()?;
    let mut files = Vec::new();
    for file in content
        .files
        .into_iter()
        .filter(|f| shown.contains(&f.path))
    {
        let changed: usize = file
            .edits
            .iter()
            .map(|e| e.before.len() + e.after.len())
            .sum();
        let collapsed = changed > limits.diff_lines && expand != Some(file.path.as_slice());
        let rows = if file.binary || collapsed {
            None
        } else {
            let after = match &file.after {
                Some(p::Entry {
                    kind: Some(p::entry::Kind::File(_)),
                }) => read_file(ctx, repo, head, &file.path)
                    .await
                    .map(|r| r.content)
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let lines = text::lines(&after);
            Some(diff::rows(&file.edits, &lines, 3))
        };
        files.push(FileDiffData {
            status: status_of(&file),
            binary: file.binary,
            path: file.path,
            rows,
        });
    }
    Ok((files, total))
}
pub struct FileView {
    pub path: String,
    pub status: String,
    pub html: String,
    pub collapsed: bool,
    pub load: String,
    pub anchor: String,
}
pub fn file_view(
    ctx: &Ctx,
    file: &FileDiffData,
    index: usize,
    load: String,
    after: &dyn Fn(usize, &Row) -> String,
) -> FileView {
    let t = ctx.t;
    let anchor = format!("f{index}");
    let html = match &file.rows {
        _ if file.binary => format!("<p class=\"muted\">{}</p>", t.s("Binary file changed")),
        Some(rows) => {
            let body = if ctx.prefs.split {
                diff::split(rows, &anchor, after)
            } else {
                diff::unified(rows, &anchor, after)
            };
            format!("<table class=\"diff\">{body}</table>")
        }
        None => String::new(),
    };
    FileView {
        path: lossy(&file.path),
        status: t.s(file.status).to_owned(),
        collapsed: file.rows.is_none() && !file.binary,
        html,
        load,
        anchor,
    }
}

#[derive(Template)]
#[template(path = "changeset.html")]
struct ChangesetPage {
    page: Page,
    heading: String,
    message: String,
    people: Vec<(String, String)>,
    parents: Vec<Link>,
    git: String,
    filtered: bool,
    notes: Vec<String>,
    files: Vec<FileView>,
    pages: Vec<Link>,
    actions: Vec<Link>,
    reviews: Vec<Link>,
}
pub async fn changeset(ctx: &mut Ctx, name: String, rest: Vec<Vec<u8>>) -> Result<Response> {
    let id = rest
        .first()
        .and_then(|s| hex::decode(s).ok())
        .filter(|id| id.len() == 32)
        .ok_or(WebError::NotFound)?;
    let repo = ctx.repo(&name).await?;
    let cs = get_changeset(ctx, &repo, &id).await?;
    let t = ctx.t;
    let parent_index: usize = ctx.q("parent").and_then(|p| p.parse().ok()).unwrap_or(0);
    let filter: Vec<Vec<u8>> = ctx
        .q("path")
        .map(|p| vec![p.as_bytes().to_vec()])
        .unwrap_or_default();
    let page_number: usize = ctx.q("page").and_then(|p| p.parse().ok()).unwrap_or(0);
    let expand = ctx.q("expand").map(|p| p.as_bytes().to_vec());
    let mut notes = Vec::new();
    if cs.parents.len() > 1 {
        notes.push(t.s("This is a merge changeset.").to_owned());
    }
    let (files, total) = match cs.parents.get(parent_index) {
        Some(parent) => {
            diff_files(
                ctx,
                &repo,
                parent,
                &id,
                &filter,
                page_number,
                expand.as_deref(),
            )
            .await?
        }
        None => {
            // A root changeset: list its changes without content.
            let files = cs
                .changes
                .iter()
                .map(|c| FileDiffData {
                    path: c.path.clone(),
                    status: if c.entry.is_some() {
                        "Added"
                    } else {
                        "Deleted"
                    },
                    binary: false,
                    rows: Some(Vec::new()),
                })
                .collect::<Vec<_>>();
            let total = files.len();
            (files, total)
        }
    };
    let id_segment = hex::encode(&id);
    let self_url = repo.url("changeset", Some(id_segment.as_bytes()), None);
    let views = files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let load = format!(
                "{self_url}?parent={parent_index}&expand={}",
                rev::segment(&f.path)
            );
            file_view(ctx, f, i, load, &|_, _| String::new())
        })
        .collect();
    let pages = paging(
        &format!("{self_url}?parent={parent_index}&"),
        total,
        ctx.app.config.file.limits.diff_files,
        page_number,
    );
    let signature = |s: &Option<p::Signature>| {
        let s = s.clone().unwrap_or_default();
        format!(
            "{} <{}> {}",
            lossy(&s.name),
            lossy(&s.email),
            time::absolute(s.timestamp, &s.tz, ctx.prefs.tz.as_deref())
        )
    };
    let parents = cs
        .parents
        .iter()
        .enumerate()
        .map(|(i, parent)| Link {
            label: text::short(parent),
            url: repo.url("changeset", Some(hex::encode(parent).as_bytes()), None),
            on: i == parent_index,
        })
        .collect();
    let mut actions = vec![Link::new(
        t.s("Code"),
        repo.url("tree", Some(id_segment.as_bytes()), None),
    )];
    if cs.parents.len() > 1 {
        for i in 0..cs.parents.len() {
            actions.push(Link {
                label: format!("{} {}", t.s("Parents"), i + 1),
                url: format!("{self_url}?parent={i}"),
                on: i == parent_index,
            });
        }
    }
    let reviews = reviews_containing(ctx, &repo, &id).await;
    let permissions = ctx.permissions(&repo, &[]).await;
    let mut nav = ctx.nav(&repo, "log", None, permissions.is_some_and(|p| p.admin));
    nav.crumbs = vec![Link::new(text::short(&id), String::new())];
    let mut page = ctx.page(text::first_line(&cs.message));
    page.repo = Some(nav);
    let body = ChangesetPage {
        page,
        heading: text::first_line(&cs.message),
        message: lossy(&cs.message),
        people: vec![
            (t.s("Author").to_owned(), signature(&cs.author)),
            ("Committer".to_owned(), signature(&cs.committer)),
        ],
        parents,
        git: cs
            .raw_git_commit
            .as_ref()
            .map(|raw| git_oid(raw))
            .unwrap_or_default(),
        filtered: cs.read_filtered,
        notes,
        files: views,
        pages,
        actions,
        reviews,
    };
    if ctx.q("expand").is_some() {
        return Ok(ctx.html(&FragmentPage { files: body.files }, StatusCode::OK, true));
    }
    Ok(ctx.html(&body, StatusCode::OK, true))
}
#[derive(Template)]
#[template(path = "fragment.html")]
pub struct FragmentPage {
    pub files: Vec<FileView>,
}
pub fn paging(base: &str, total: usize, per_page: usize, current: usize) -> Vec<Link> {
    let pages = total.div_ceil(per_page.max(1));
    if pages <= 1 {
        return Vec::new();
    }
    (0..pages)
        .map(|n| Link {
            label: (n + 1).to_string(),
            url: format!("{base}page={n}"),
            on: n == current,
        })
        .collect()
}
async fn reviews_containing(ctx: &Ctx, repo: &Repo, id: &[u8]) -> Vec<Link> {
    let Ok(list) = rpc!(
        ctx,
        review.list_reviews(p::ListReviewsRequest {
            repo: repo.bytes(),
            state: Some(p::ReviewState::Open as i32),
            limit: 100,
            ..Default::default()
        })
    )
    .await
    else {
        return Vec::new();
    };
    list.reviews
        .iter()
        .filter(|r| {
            r.revisions
                .last()
                .is_some_and(|rev| rev.changesets.iter().any(|c| c == id))
        })
        .map(|r| {
            Link::new(
                format!("#{} {}", r.number, lossy(&r.title)),
                repo.url("reviews", Some(r.number.to_string().as_bytes()), None),
            )
        })
        .collect()
}

#[derive(Template)]
#[template(path = "compare.html")]
struct ComparePage {
    page: Page,
    base: String,
    head: String,
    bases: Vec<Link>,
    ancestor: bool,
    files: Vec<FileView>,
    pages: Vec<Link>,
    review: Option<String>,
    two_dot: bool,
}
pub async fn compare(ctx: &mut Ctx, name: String, rest: Vec<Vec<u8>>) -> Result<Response> {
    let spec = rest.first().ok_or(WebError::NotFound)?.clone();
    let text_spec = String::from_utf8(spec.clone())
        .map_err(|_| WebError::BadRequest("invalid comparison".into()))?;
    let (base_rev, head_rev, two_dot) = match text_spec.split_once("...") {
        Some((a, b)) => (a.to_owned(), b.to_owned(), false),
        None => match text_spec.split_once("..") {
            Some((a, b)) => (a.to_owned(), b.to_owned(), true),
            None => return Err(WebError::BadRequest("use <base>...<head>".into())),
        },
    };
    let repo = ctx.repo(&name).await?;
    let base = ctx.resolve(&repo, base_rev.as_bytes()).await?;
    let head = ctx.resolve(&repo, head_rev.as_bytes()).await?;
    let compare = p::CompareRequest {
        repo: repo.bytes(),
        base: base.id.clone(),
        head: head.id.clone(),
    };
    let merge_bases = rpc!(ctx, metadata.merge_base(compare.clone()))
        .await
        .read()?;
    let ancestor = rpc!(ctx, metadata.is_ancestor(compare.clone()))
        .await
        .read()?
        .is_ancestor;
    let from = if two_dot {
        base.id.clone()
    } else {
        merge_bases
            .ids
            .first()
            .cloned()
            .unwrap_or_else(|| base.id.clone())
    };
    let page_number: usize = ctx.q("page").and_then(|p| p.parse().ok()).unwrap_or(0);
    let expand = ctx.q("expand").map(|p| p.as_bytes().to_vec());
    let (files, total) = diff_files(
        ctx,
        &repo,
        &from,
        &head.id,
        &[],
        page_number,
        expand.as_deref(),
    )
    .await?;
    let self_url = repo.url("compare", Some(&spec), None);
    let views = files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            file_view(
                ctx,
                f,
                i,
                format!("{self_url}?expand={}", rev::segment(&f.path)),
                &|_, _| String::new(),
            )
        })
        .collect::<Vec<_>>();
    if ctx.q("expand").is_some() {
        return Ok(ctx.html(&FragmentPage { files: views }, StatusCode::OK, false));
    }
    let permissions = ctx.permissions(&repo, &[]).await;
    let review = (permissions.as_ref().is_some_and(|p| p.write && p.review)
        && head.reference.is_some()
        && base.reference.is_some())
    .then(|| {
        format!(
            "{}?target={}&head={}",
            repo.url("reviews", Some(b"new"), None),
            rev::segment(base.reference.as_deref().unwrap_or_default()),
            hex::encode(&head.id)
        )
    });
    let mut nav = ctx.nav(&repo, "log", None, permissions.is_some_and(|p| p.admin));
    nav.crumbs = vec![Link::new(text_spec.clone(), String::new())];
    let mut page = ctx.page(format!("{} {text_spec}", ctx.t.s("Compare")));
    page.repo = Some(nav);
    let pages = paging(
        &format!("{self_url}?"),
        total,
        ctx.app.config.file.limits.diff_files,
        page_number,
    );
    let body = ComparePage {
        page,
        base: base.label(),
        head: head.label(),
        bases: merge_bases
            .ids
            .iter()
            .map(|id| {
                Link::new(
                    text::short(id),
                    repo.url("changeset", Some(hex::encode(id).as_bytes()), None),
                )
            })
            .collect(),
        ancestor,
        files: views,
        pages,
        review,
        two_dot,
    };
    let immutable = base.immutable() && head.immutable();
    Ok(ctx.html(&body, StatusCode::OK, immutable))
}

pub async fn git(ctx: &mut Ctx, name: String, rest: Vec<Vec<u8>>) -> Result<Response> {
    let oid = rest
        .first()
        .and_then(|s| hex::decode(s).ok())
        .filter(|o| o.len() == 20)
        .ok_or(WebError::NotFound)?;
    let repo = ctx.repo(&name).await?;
    let found = rpc!(
        ctx,
        metadata.find_changeset_by_git_oid(p::GitOidRequest {
            repo: repo.bytes(),
            oid: oid.clone()
        })
    )
    .await
    .read()?;
    let id = found.id.ok_or(WebError::NotFound)?;
    let url = repo.url("changeset", Some(hex::encode(id).as_bytes()), None);
    Ok(ctx.redirect(&url))
}

/// A tar.gz (default) or zip snapshot of a directory, streamed from the
/// server.
pub async fn archive(ctx: &mut Ctx, name: String, rest: Vec<Vec<u8>>) -> Result<Response> {
    let (rev, path) = split_rev(rest)?;
    let repo = ctx.repo(&name).await?;
    let resolved = ctx.resolve(&repo, &rev).await?;
    let (format, extension, content_type) = match ctx.q("format") {
        Some("zip") => (p::ArchiveFormat::Zip, "zip", "application/zip"),
        _ => (p::ArchiveFormat::TarGz, "tar.gz", "application/gzip"),
    };
    let base = repo.name.rsplit('/').next().unwrap_or("archive");
    let label = format!("{base}-{}", text::short(&resolved.id));
    let stream = ctx
        .upstream()
        .archive()
        .get_archive(ctx.req(p::ArchiveRequest {
            repo: repo.bytes(),
            id: resolved.id.clone(),
            path: path.clone(),
            format: format as i32,
            prefix: format!("{label}/").into_bytes(),
        }))
        .await
        .read()?
        .into_inner();
    use tokio_stream::StreamExt;
    let body = axum::body::Body::from_stream(stream.map(|chunk| {
        chunk
            .map(|c| axum::body::Bytes::from(c.data))
            .map_err(|status| std::io::Error::other(status.message().to_owned()))
    }));
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(value) =
        HeaderValue::from_str(&format!("attachment; filename=\"{label}.{extension}\""))
    {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    Ok(response)
}
