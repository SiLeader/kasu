use kasu_core::{RepoKey, RepoName, Result, invalid};
use kasu_derived::Engine;
use kasu_git::{GitIndex, ProjectionId, register_git};
use kasu_ingest::Importer;
use kasu_store::{
    Repository, RepositoryRecord, RepositoryStore, RepositoryWrite, Storage, storage_error,
};
use std::path::PathBuf;

const USAGE: &str =
    "usage: kasu-ingest [--create] <git-repository> <kasu-directory> <repository-name>";

fn now() -> Result<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(storage_error)?
        .as_secs()
        .try_into()
        .map_err(storage_error)
}
/// The key to import into. An unregistered name is created with `--create`;
/// an empty registry registers it as a legacy repository (one release of
/// compatibility with installations that predate the registry).
fn repository_key(store: &dyn RepositoryStore, name: &RepoName, create: bool) -> Result<RepoKey> {
    let now = now()?;
    if let Some(entry) = store.resolve_repository_name(name, now)? {
        return Ok(entry.key);
    }
    let key = if create {
        RepoKey::generate()?
    } else if store.list_repositories(None, 1)?.is_empty() {
        RepoKey::legacy(name)
    } else {
        return Err(invalid(format!(
            "repository {name} is not registered; pass --create to create it"
        )));
    };
    let record = RepositoryRecord::new(key, name.clone(), b"kasu-ingest", now);
    match store.create_repository(&record)? {
        RepositoryWrite::Applied(record) => Ok(record.key),
        // Another importer registered the name concurrently.
        _ => store
            .resolve_repository_name(name, now)?
            .map(|entry| entry.key)
            .ok_or_else(|| invalid("repository registration conflicted")),
    }
}
fn run() -> Result<()> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    let create = args.first().is_some_and(|a| a == "--create");
    if create {
        args.remove(0);
    }
    if args.len() != 3 {
        return Err(invalid(USAGE));
    }
    let destination = PathBuf::from(&args[1]);
    let Storage {
        database: meta,
        blobs,
    } = Storage::from_env(&destination)?;
    let name = RepoName::from_bytes(args[2].as_encoded_bytes())?;
    let key = repository_key(meta.as_ref(), &name, create)?;
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let importer = Importer {
        engine: &engine,
        index: GitIndex {
            repo: Repository {
                meta: meta.as_ref(),
                blobs: blobs.as_ref(),
            },
            projection: ProjectionId::full(key.as_bytes()),
        },
    };
    let report = importer.import(&args[0])?;
    importer.publish(meta.as_ref(), &report, b"kasu-ingest", now()?)?;
    println!(
        "Imported {} objects, {} commits, {} refs into {name}",
        report.objects,
        report.commits,
        report.refs.len()
    );
    Ok(())
}
fn main() {
    let telemetry = match kasu_telemetry::init("kasu-ingest") {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let span = kasu_telemetry::operation_span("ingest");
    let result = {
        let _entered = span.enter();
        run()
    };
    kasu_telemetry::finish_operation(&span, result.is_ok());
    drop(span);
    drop(telemetry);
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
