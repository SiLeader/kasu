use kasu_core::{Result, invalid};
use kasu_derived::Engine;
use kasu_git::{GitIndex, ProjectionId, register_git};
use kasu_ingest::Importer;
use kasu_store::{Repository, Storage, storage_error};
use std::path::PathBuf;

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 3 {
        return Err(invalid(
            "usage: kasu-ingest <git-repository> <kasu-directory> <repository-name>",
        ));
    }
    let destination = PathBuf::from(&args[1]);
    let Storage {
        database: meta,
        blobs,
    } = Storage::from_env(&destination)?;
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let importer = Importer {
        engine: &engine,
        index: GitIndex {
            repo: Repository {
                meta: meta.as_ref(),
                blobs: blobs.as_ref(),
            },
            projection: ProjectionId::full(args[2].as_encoded_bytes()),
        },
    };
    let report = importer.import(&args[0])?;
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(storage_error)?
        .as_secs()
        .try_into()
        .map_err(storage_error)?;
    importer.publish(meta.as_ref(), &report, b"kasu-ingest", time)?;
    println!(
        "Imported {} objects, {} commits, {} refs",
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
