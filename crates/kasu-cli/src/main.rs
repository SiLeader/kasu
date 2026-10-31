use clap::{Parser, Subcommand};
use kasu_cli::{Remote, Result, SparseProfile, WorkingCopy};
use kasu_proto::v1 as p;
use std::{ffi::OsString, io::Write, os::unix::ffi::OsStrExt, path::PathBuf};
use tracing::Instrument;
#[derive(Parser)]
#[command(name = "kasu", version, about = "Server-centered version control")]
struct Cli {
    #[arg(short = 'C', long, global = true, default_value = ".")]
    directory: PathBuf,
    #[arg(long, env = "KASU_TOKEN", global = true, hide_env_values = true)]
    token: Option<String>,
    #[arg(long, env = "KASU_OBJECTS_ENDPOINT", global = true)]
    objects: Option<String>,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, env = "KASU_AUTHOR_NAME", global = true)]
    author_name: Option<String>,
    #[arg(long, env = "KASU_AUTHOR_EMAIL", global = true)]
    author_email: Option<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    #[cfg(target_os = "linux")]
    Mount {
        revision: OsString,
        destination: PathBuf,
        #[arg(long)]
        backing: PathBuf,
        #[arg(long, env = "KASU_SERVER", default_value = "http://127.0.0.1:50051")]
        server: String,
        #[arg(long, env = "KASU_REPO")]
        repo: OsString,
        #[arg(long)]
        sparse: Vec<PathBuf>,
    },
    Checkout {
        revision: OsString,
        destination: PathBuf,
        #[arg(long, env = "KASU_SERVER", default_value = "http://127.0.0.1:50051")]
        server: String,
        #[arg(long, env = "KASU_REPO")]
        repo: OsString,
        #[arg(long)]
        sparse: Vec<PathBuf>,
    },
    /// Check out `repo` from `server` into `destination` (default: the last
    /// segment of the repository name) at its default branch.
    Clone {
        server: String,
        repo: String,
        destination: Option<PathBuf>,
        #[arg(long)]
        revision: Option<OsString>,
        #[arg(long)]
        sparse: Vec<PathBuf>,
    },
    /// Repository administration.
    Repo {
        #[arg(long, env = "KASU_SERVER", default_value = "http://127.0.0.1:50051")]
        server: String,
        #[command(subcommand)]
        action: RepoAction,
    },
    /// Reviews of Changeset stacks (see docs/review.md).
    Review {
        #[arg(long, env = "KASU_SERVER", default_value = "http://127.0.0.1:50051")]
        server: String,
        #[arg(long, env = "KASU_REPO")]
        repo: Option<String>,
        #[command(subcommand)]
        action: ReviewAction,
    },
    Status {
        #[arg(long)]
        watch: bool,
    },
    Diff,
    Commit {
        #[arg(short = 'm', long)]
        message: String,
    },
    Rebase {
        #[arg(long, default_value = "main")]
        onto: OsString,
    },
    Land {
        #[arg(long = "ref")]
        reference: Option<OsString>,
    },
    Sparse {
        #[command(subcommand)]
        action: SparseAction,
    },
    Log {
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Blame {
        path: PathBuf,
        #[arg(long)]
        follow_copies: bool,
        #[arg(long)]
        follow_moves: bool,
    },
    Show {
        revision: Option<OsString>,
        path: Option<PathBuf>,
    },
}
#[derive(Subcommand)]
enum RepoAction {
    /// List repositories visible to the caller.
    List {
        #[arg(long, default_value = "")]
        prefix: String,
    },
    Create {
        name: String,
        #[arg(long)]
        default_branch: Option<String>,
    },
    Show {
        name: String,
    },
}
#[derive(Subcommand)]
enum ReviewAction {
    List {
        /// open, submitted, closed or all.
        #[arg(long, default_value = "open")]
        state: String,
    },
    Show {
        number: u64,
    },
    /// Propose the given stack (oldest first; 64-hex Changeset IDs), or the
    /// working copy's current Changeset, for `target`.
    Create {
        #[arg(long, default_value = "refs/heads/main")]
        target: String,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        description: String,
        changesets: Vec<String>,
    },
    /// approve, request-changes or none.
    Vote {
        number: u64,
        value: String,
        #[arg(short = 'm', long, default_value = "")]
        message: String,
    },
    Comment {
        number: u64,
        message: String,
    },
    Update {
        number: u64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, conflicts_with = "reopen")]
        close: bool,
        #[arg(long)]
        reopen: bool,
    },
    /// Rebase the latest revision onto the target's tip.
    Rebase {
        number: u64,
    },
    Submit {
        number: u64,
        /// rebase or squash.
        #[arg(long, default_value = "rebase")]
        method: String,
    },
}
#[derive(Subcommand)]
enum SparseAction {
    Add { path: PathBuf },
    Remove { path: PathBuf },
}
fn profile_path(path: &std::path::Path) -> Vec<u8> {
    if path == std::path::Path::new(".") {
        vec![]
    } else {
        path.as_os_str().as_bytes().to_vec()
    }
}
fn emit(value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout().lock(), value)?;
    println!();
    Ok(())
}
fn status(copy: &WorkingCopy, json: bool) -> Result<()> {
    let changes = copy.changes()?;
    if json {
        return emit(&changes);
    }
    for change in changes {
        let kind = match (&change.before, &change.after) {
            (None, _) => 'A',
            (_, None) => 'D',
            _ => 'M',
        };
        println!(
            "{kind} {}",
            String::from_utf8(change.path.escape_ascii().collect())?
        );
    }
    Ok(())
}
fn identity(cli: &Cli) -> Result<p::Signature> {
    Ok(p::Signature {
        name: cli
            .author_name
            .as_ref()
            .ok_or("set KASU_AUTHOR_NAME or --author-name")?
            .as_bytes()
            .to_vec(),
        email: cli
            .author_email
            .as_ref()
            .ok_or("set KASU_AUTHOR_EMAIL or --author-email")?
            .as_bytes()
            .to_vec(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            .try_into()?,
        tz: b"+0000".to_vec(),
    })
}
async fn watch(cli: &Cli) -> Result<()> {
    use notify::Watcher;
    let copy = WorkingCopy::find(&cli.directory)?;
    let path = copy.path.clone();
    drop(copy);
    let admin = path.join(".kasu");
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.as_ref().map_or(true, |event| {
            event.paths.is_empty() || event.paths.iter().any(|p| !p.starts_with(&admin))
        }) {
            let _ = tx.try_send(());
        }
    })?;
    watcher.watch(&path, notify::RecursiveMode::Recursive)?;
    status(&WorkingCopy::open(&path)?, cli.json)?;
    loop {
        tokio::select! {_=tokio::signal::ctrl_c()=>break,event=rx.recv()=>{if event.is_none(){break;}
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            loop {match WorkingCopy::open(&path) {Ok(copy)=>{status(&copy,cli.json)?;break;},Err(error) if error.to_string().contains("locked by another")=>{
                tokio::select!{_=tokio::signal::ctrl_c()=>return Ok(()),_=tokio::time::sleep(std::time::Duration::from_millis(100))=>{}}
            },Err(error)=>return Err(error)}}
        }}
    }
    Ok(())
}
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let telemetry = match kasu_telemetry::init("kasu-cli") {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("kasu: {error}");
            std::process::exit(1);
        }
    };
    let span = kasu_telemetry::operation_span("cli");
    let result = run(&cli).instrument(span.clone()).await;
    kasu_telemetry::finish(
        &span,
        if result.is_ok() {
            tonic::Code::Ok
        } else {
            tonic::Code::Unknown
        },
    );
    drop(span);
    drop(telemetry);
    if let Err(error) = result {
        eprintln!("kasu: {error}");
        std::process::exit(1);
    }
}
async fn repo(cli: &Cli, server: &str, action: &RepoAction) -> Result<()> {
    let mut admin = kasu_cli::Admin::connect(server, cli.token.as_deref()).await?;
    let show = |r: &p::Repository| {
        let state = p::RepositoryState::try_from(r.state)
            .map(|s| s.as_str_name())
            .unwrap_or("UNKNOWN");
        println!(
            "{}\t{}\t{}",
            r.name,
            state,
            String::from_utf8_lossy(&r.default_branch)
        );
    };
    match action {
        RepoAction::List { prefix } => {
            let mut cursor = String::new();
            let mut all = Vec::new();
            loop {
                let q = admin.request(p::ListRepositoriesRequest {
                    prefix: prefix.clone(),
                    cursor,
                    limit: 1000,
                });
                let page = admin.client.list_repositories(q).await?.into_inner();
                all.extend(page.repositories);
                if page.cursor.is_empty() {
                    break;
                }
                cursor = page.cursor;
            }
            if cli.json {
                return emit(&all);
            }
            all.iter().for_each(show);
        }
        RepoAction::Create {
            name,
            default_branch,
        } => {
            let q = admin.request(p::CreateRepositoryRequest {
                name: name.clone(),
                default_branch: default_branch.clone().unwrap_or_default().into_bytes(),
                config_json: String::new(),
                idempotency_key: vec![],
            });
            let created = admin.client.create_repository(q).await?.into_inner();
            if cli.json {
                return emit(&created);
            }
            show(&created);
        }
        RepoAction::Show { name } => {
            let q = admin.request(p::GetRepositoryRequest { name: name.clone() });
            let repository = admin.client.get_repository(q).await?.into_inner();
            if cli.json {
                return emit(&repository);
            }
            show(&repository);
        }
    }
    Ok(())
}
fn decode_id(text: &str) -> Result<Vec<u8>> {
    let id = hex::decode(text)?;
    if id.len() != 32 {
        return Err(format!("{text} is not a 64-hex Changeset ID").into());
    }
    Ok(id)
}
fn random_key() -> Vec<u8> {
    let mut key = [0u8; 16];
    getrandom::fill(&mut key).expect("the operating system provides randomness");
    hex::encode(key).into_bytes()
}
async fn review(cli: &Cli, server: &str, repo: Option<&str>, action: &ReviewAction) -> Result<()> {
    let copy = WorkingCopy::open(&cli.directory).ok();
    let repo = match (repo, &copy) {
        (Some(repo), _) => repo.as_bytes().to_vec(),
        (None, Some(copy)) => copy.state.repo.clone(),
        (None, None) => {
            return Err("set --repo or KASU_REPO, or run inside a working copy".into());
        }
    };
    let mut reviews = kasu_cli::Reviews::connect(server, cli.token.as_deref()).await?;
    let get = |number: u64| p::GetReviewRequest {
        repo: repo.clone(),
        number,
    };
    let show = |r: &p::ReviewRecord| {
        let state = p::ReviewState::try_from(r.state)
            .map(|s| s.as_str_name().trim_start_matches("REVIEW_STATE_"))
            .unwrap_or("UNKNOWN");
        println!(
            "#{}\t{state}\t{}\t{}\t{}",
            r.number,
            String::from_utf8_lossy(&r.target),
            r.author,
            String::from_utf8_lossy(&r.title)
        );
    };
    let latest = |r: &p::ReviewRecord| r.revisions.last().map(|r| r.number).unwrap_or(1);
    match action {
        ReviewAction::List { state } => {
            let state = match state.as_str() {
                "open" => Some(p::ReviewState::Open as i32),
                "submitted" => Some(p::ReviewState::Submitted as i32),
                "closed" => Some(p::ReviewState::Closed as i32),
                "all" => None,
                other => return Err(format!("unknown state {other}").into()),
            };
            let mut cursor = 0;
            let mut all = Vec::new();
            loop {
                let q = reviews.request(p::ListReviewsRequest {
                    repo: repo.clone(),
                    state,
                    cursor,
                    limit: 500,
                    ..Default::default()
                });
                let page = reviews.client.list_reviews(q).await?.into_inner();
                all.extend(page.reviews);
                if page.cursor == 0 {
                    break;
                }
                cursor = page.cursor;
            }
            if cli.json {
                return emit(&all);
            }
            all.iter().for_each(show);
        }
        ReviewAction::Show { number } => {
            let record = reviews
                .client
                .get_review(reviews.request(get(*number)))
                .await?
                .into_inner();
            let status = reviews
                .client
                .get_submit_status(reviews.request(get(*number)))
                .await?
                .into_inner();
            if cli.json {
                return emit(&serde_json::json!({ "review": record, "status": status }));
            }
            show(&record);
            for revision in &record.revisions {
                let stack: Vec<String> = revision.changesets.iter().map(hex::encode).collect();
                println!(
                    "revision {}\t{}\t{}",
                    revision.number,
                    revision.created_by,
                    stack.join(" ")
                );
            }
            for vote in &record.votes {
                let value = p::VoteValue::try_from(vote.value)
                    .map(|v| v.as_str_name().trim_start_matches("VOTE_VALUE_"))
                    .unwrap_or("UNKNOWN");
                println!("vote\t{}\t{value}\tr{}", vote.account, vote.revision);
            }
            println!(
                "submit\t{}\t{}/{} approvals",
                if status.ready { "ready" } else { "not ready" },
                status.approvals,
                status.required_approvals
            );
            for unmet in &status.unmet {
                println!("unmet\t{unmet}");
            }
        }
        ReviewAction::Create {
            target,
            title,
            description,
            changesets,
        } => {
            let stack = if changesets.is_empty() {
                let copy = copy.ok_or("give Changeset IDs or run inside a working copy")?;
                vec![copy.state.id()?.0.to_vec()]
            } else {
                changesets
                    .iter()
                    .map(|c| decode_id(c))
                    .collect::<Result<_>>()?
            };
            let q = reviews.request(p::CreateReviewRequest {
                repo: repo.clone(),
                target: target.as_bytes().to_vec(),
                changesets: stack,
                title: title.as_bytes().to_vec(),
                description: description.as_bytes().to_vec(),
                idempotency_key: random_key(),
            });
            let created = reviews.client.create_review(q).await?.into_inner();
            if cli.json {
                return emit(&created);
            }
            show(&created);
        }
        ReviewAction::Vote {
            number,
            value,
            message,
        } => {
            let vote = match value.as_str() {
                "approve" => p::VoteValue::Approve,
                "request-changes" => p::VoteValue::RequestChanges,
                "none" => p::VoteValue::None,
                other => return Err(format!("unknown vote {other}").into()),
            };
            let record = reviews
                .client
                .get_review(reviews.request(get(*number)))
                .await?
                .into_inner();
            let q = reviews.request(p::PublishRequest {
                repo: repo.clone(),
                number: *number,
                revision: latest(&record),
                vote: Some(vote as i32),
                body: message.as_bytes().to_vec(),
                comment_ids: Vec::new(),
            });
            reviews.client.publish(q).await?;
        }
        ReviewAction::Comment { number, message } => {
            let record = reviews
                .client
                .get_review(reviews.request(get(*number)))
                .await?
                .into_inner();
            let q = reviews.request(p::PublishRequest {
                repo: repo.clone(),
                number: *number,
                revision: latest(&record),
                vote: None,
                body: message.as_bytes().to_vec(),
                comment_ids: Vec::new(),
            });
            reviews.client.publish(q).await?;
        }
        ReviewAction::Update {
            number,
            title,
            close,
            reopen,
        } => {
            let record = reviews
                .client
                .get_review(reviews.request(get(*number)))
                .await?
                .into_inner();
            let state = match (close, reopen) {
                (true, _) => Some(p::ReviewState::Closed as i32),
                (_, true) => Some(p::ReviewState::Open as i32),
                _ => None,
            };
            let q = reviews.request(p::UpdateReviewRequest {
                repo: repo.clone(),
                number: *number,
                expected_version: record.version,
                title: title.as_ref().map(|t| t.as_bytes().to_vec()),
                description: None,
                state,
            });
            let updated = reviews.client.update_review(q).await?.into_inner();
            if cli.json {
                return emit(&updated);
            }
            show(&updated);
        }
        ReviewAction::Rebase { number } => {
            let record = reviews
                .client
                .get_review(reviews.request(get(*number)))
                .await?
                .into_inner();
            let q = reviews.request(p::RebaseReviewRequest {
                repo: repo.clone(),
                number: *number,
                expected_revision: latest(&record),
                onto: vec![],
            });
            let rebased = reviews.client.rebase_review(q).await?.into_inner();
            if cli.json {
                emit(&rebased)?;
            }
            if let Some(conflict) = rebased.conflict {
                for c in &conflict.conflicts {
                    eprintln!("conflict\t{}", String::from_utf8_lossy(&c.path));
                }
                return Err(format!(
                    "changeset {} of the stack conflicts with the target",
                    rebased.conflict_index + 1
                )
                .into());
            }
        }
        ReviewAction::Submit { number, method } => {
            let method = match method.as_str() {
                "rebase" => p::SubmitMethod::Rebase,
                "squash" => p::SubmitMethod::Squash,
                other => return Err(format!("unknown method {other}").into()),
            };
            let record = reviews
                .client
                .get_review(reviews.request(get(*number)))
                .await?
                .into_inner();
            let q = reviews.request(p::SubmitRequest {
                repo: repo.clone(),
                number: *number,
                expected_revision: latest(&record),
                method: method as i32,
                idempotency_key: random_key(),
                ..Default::default()
            });
            let response = reviews.client.submit(q).await?.into_inner();
            if cli.json {
                emit(&response)?;
            } else {
                println!(
                    "{:?} {}",
                    p::SubmitOutcome::try_from(response.outcome)?,
                    response.landed.last().map(hex::encode).unwrap_or_default()
                );
            }
            if response.outcome != p::SubmitOutcome::Landed as i32 {
                let unmet = response
                    .status
                    .map(|s| s.unmet.join("; "))
                    .unwrap_or_default();
                return Err(
                    format!("review was not submitted: {} {unmet}", response.message).into(),
                );
            }
        }
    }
    Ok(())
}
async fn run(cli: &Cli) -> Result<()> {
    #[cfg(target_os = "linux")]
    if let Command::Mount {
        revision,
        destination,
        backing,
        server,
        repo,
        sparse,
    } = &cli.command
    {
        let destination = if destination.is_absolute() {
            destination.clone()
        } else {
            cli.directory.join(destination)
        };
        let backing = if backing.is_absolute() {
            backing.clone()
        } else {
            cli.directory.join(backing)
        };
        return kasu_cli::virtual_fs::mount(kasu_cli::virtual_fs::MountRequest {
            server,
            objects: cli.objects.as_deref().unwrap_or(server),
            repo: repo.as_bytes().to_vec(),
            revision: revision.as_bytes(),
            mountpoint: &destination,
            backing: &backing,
            sparse: SparseProfile::new(sparse.iter().map(|p| profile_path(p)).collect())?,
            token: cli.token.as_deref(),
        })
        .await;
    }

    if let Command::Checkout {
        revision,
        destination,
        server,
        repo,
        sparse,
    } = &cli.command
    {
        let objects = cli.objects.as_deref().unwrap_or(server);
        let mut remote = Remote::connect(
            server,
            objects,
            repo.as_bytes().to_vec(),
            cli.token.as_deref(),
        )
        .await?;
        let destination = if destination.is_absolute() {
            destination.clone()
        } else {
            cli.directory.join(destination)
        };
        let copy = WorkingCopy::checkout(
            &mut remote,
            server.clone(),
            objects.to_owned(),
            revision.as_bytes(),
            &destination,
            SparseProfile::new(sparse.iter().map(|p| profile_path(p)).collect())?,
        )
        .await?;
        println!("{}", copy.state.id()?);
        return Ok(());
    }
    if let Command::Repo { server, action } = &cli.command {
        return repo(cli, server, action).await;
    }
    if let Command::Review {
        server,
        repo,
        action,
    } = &cli.command
    {
        return review(cli, server, repo.as_deref(), action).await;
    }
    if let Command::Clone {
        server,
        repo,
        destination,
        revision,
        sparse,
    } = &cli.command
    {
        let destination = destination
            .clone()
            .unwrap_or_else(|| PathBuf::from(repo.rsplit('/').next().unwrap_or(repo.as_str())));
        let destination = if destination.is_absolute() {
            destination
        } else {
            cli.directory.join(destination)
        };
        let revision = match revision {
            Some(revision) => revision.as_bytes().to_vec(),
            None => match kasu_cli::Admin::connect(server, cli.token.as_deref()).await {
                Ok(mut admin) => admin
                    .default_branch(repo)
                    .await
                    .unwrap_or_else(|_| b"main".to_vec()),
                Err(_) => b"main".to_vec(),
            },
        };
        let objects = cli.objects.as_deref().unwrap_or(server);
        let mut remote = Remote::connect(
            server,
            objects,
            repo.as_bytes().to_vec(),
            cli.token.as_deref(),
        )
        .await?;
        let copy = WorkingCopy::checkout(
            &mut remote,
            server.clone(),
            objects.to_owned(),
            &revision,
            &destination,
            SparseProfile::new(sparse.iter().map(|p| profile_path(p)).collect())?,
        )
        .await?;
        println!("{}", copy.state.id()?);
        return Ok(());
    }
    if matches!(cli.command, Command::Status { watch: true }) {
        return watch(cli).await;
    }
    let mut copy = WorkingCopy::find(&cli.directory)?;
    match &cli.command {
        Command::Status { .. } => return status(&copy, cli.json),
        Command::Diff => {
            if cli.json {
                emit(&copy.changes()?)?;
            } else {
                copy.hydrate_diff(cli.token.as_deref()).await?;
                print!("{}", copy.diff_text()?);
            }
            return Ok(());
        }
        _ => {}
    }
    let mut remote = Remote::connect(
        &copy.state.server,
        cli.objects.as_deref().unwrap_or(&copy.state.objects),
        copy.state.repo.clone(),
        cli.token.as_deref(),
    )
    .await?;
    match &cli.command {
        Command::Commit { message } => {
            let id = copy
                .commit(&mut remote, message.as_bytes().to_vec(), identity(cli)?)
                .await?;
            println!("{id}");
        }
        Command::Rebase { onto } => {
            let signature = if copy.state.draft {
                Some(identity(cli)?)
            } else {
                None
            };
            if let Some(response) = copy.rebase(&mut remote, onto.as_bytes(), signature).await?
                && response.id.is_none()
            {
                emit(&response)?;
                return Err("rebase conflicted; details saved in .kasu/conflicts.json".into());
            }
            println!("{}", copy.state.id()?);
        }
        Command::Land { reference } => {
            let response = copy
                .land(&mut remote, reference.as_ref().map(|r| r.as_bytes()))
                .await?;
            if cli.json {
                emit(&response)?;
            } else {
                println!(
                    "{:?} {}",
                    p::LandOutcome::try_from(response.outcome)?,
                    response.id.as_ref().map(hex::encode).unwrap_or_default()
                );
            }
            if response.message == "review required" {
                return Err("the ref is protected: propose it with `kasu review create` and submit it once approved".into());
            }
            if response.outcome != p::LandOutcome::Landed as i32 {
                return Err("Land did not land; working copy was preserved".into());
            }
        }
        Command::Sparse { action } => {
            let (path, include) = match action {
                SparseAction::Add { path } => (path, true),
                SparseAction::Remove { path } => (path, false),
            };
            copy.sparse(&mut remote, profile_path(path), include)
                .await?;
        }
        Command::Log { limit, path } => {
            let q = remote.request(p::LogRequest {
                repo: remote.repo.clone(),
                start: copy.state.base.clone(),
                path: path.as_ref().map(|p| p.as_os_str().as_bytes().to_vec()),
                limit: *limit,
                cursor: vec![],
            });
            let response = remote.metadata.log(q).await?.into_inner();
            if cli.json {
                emit(&response)?;
            } else {
                for cs in response.changesets {
                    println!(
                        "{} {}",
                        hex::encode(cs.id),
                        String::from_utf8_lossy(&cs.message).escape_debug()
                    );
                }
            }
        }
        Command::Blame {
            path,
            follow_copies,
            follow_moves,
        } => {
            let q = remote.request(p::PathRequest {
                follow_copies: *follow_copies,
                follow_moves: *follow_moves,
                repo: remote.repo.clone(),
                id: copy.state.base.clone(),
                path: path.as_os_str().as_bytes().to_vec(),
            });
            emit(&remote.metadata.blame(q).await?.into_inner())?;
        }
        Command::Show { revision, path } => {
            let id = match revision {
                Some(revision) => remote.resolve(revision.as_bytes()).await?.0,
                None => copy.state.id()?,
            };
            if let Some(path) = path {
                let q = remote.request(p::PathRequest {
                    follow_copies: false,
                    follow_moves: false,
                    repo: remote.repo.clone(),
                    id: id.0.to_vec(),
                    path: path.as_os_str().as_bytes().to_vec(),
                });
                let response = remote.metadata.read_file(q).await?.into_inner();
                if cli.json {
                    emit(&response)?;
                } else {
                    std::io::stdout().lock().write_all(&response.content)?;
                }
            } else {
                let q = remote.request(p::ChangesetRequest {
                    repo: remote.repo.clone(),
                    id: id.0.to_vec(),
                });
                emit(&remote.metadata.get_changeset(q).await?.into_inner())?;
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}
