use std::{
    io::{BufReader, BufWriter, Write},
    path::PathBuf,
    time::Duration,
};

#[tokio::main]
async fn main() {
    let telemetry = match kasu_telemetry::init("kasu-git-ssh") {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("kasu-git-ssh: {error}");
            std::process::exit(1);
        }
    };
    let span = kasu_telemetry::operation_span("git_ssh");
    let timeout = std::env::var("KASU_SSH_TIMEOUT_SECS")
        .unwrap_or_else(|_| "300".into())
        .parse::<u64>();
    let timeout = match timeout {
        Ok(n @ 1..=3600) => Duration::from_secs(n),
        _ => {
            eprintln!("kasu-git-ssh: invalid session timeout");
            std::process::exit(1);
        }
    };
    let task = {
        let _entered = span.enter();
        kasu_telemetry::blocking(|| -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            let args: Vec<_> = std::env::args_os().skip(1).collect();
            // The repository comes from the client's command. The older form
            // also names one repository, and serves only that repository.
            let (root, repository, credential) = match args.as_slice() {
                [root, credential] => (root, None, credential),
                [root, repository, credential] => (
                    root,
                    Some(repository.as_encoded_bytes().to_vec()),
                    credential,
                ),
                _ => {
                    return Err(
                        "usage: kasu-git-ssh <kasu-directory> [repository-name] <credential-file>"
                            .into(),
                    );
                }
            };
            let original = std::env::var_os("SSH_ORIGINAL_COMMAND")
                .ok_or("SSH_ORIGINAL_COMMAND is required")?;
            let token = kasu_object::read_token(&PathBuf::from(credential))?;
            let mut config = kasu_server::load_backend(&PathBuf::from(root), repository)?;
            if let Some(record) = &config.legacy {
                config.backend = config.backend.with_pinned_repository(record.key.clone());
            }
            let protocol = std::env::var("GIT_PROTOCOL").ok();
            let mut input = BufReader::with_capacity(64 * 1024, std::io::stdin());
            let mut output = BufWriter::with_capacity(64 * 1024, std::io::stdout());
            let result = kasu_server::serve_git_ssh(
                &config.backend,
                &format!("Bearer {token}"),
                original.as_encoded_bytes(),
                protocol.as_deref(),
                &mut input,
                &mut output,
            );
            if result.is_err() {
                let _ = kasu_git::write_packet(&mut output, b"ERR kasu SSH request failed\n");
                let _ = output.flush();
            }
            result.map_err(Into::into)
        })
    };
    let result = match tokio::time::timeout(timeout, task).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(error.into()),
        Err(_) => {
            eprintln!("kasu-git-ssh: session deadline exceeded");
            std::process::exit(124);
        }
    };
    kasu_telemetry::finish_operation(&span, result.is_ok());
    drop(span);
    drop(telemetry);
    if let Err(error) = result {
        eprintln!("kasu-git-ssh: {error}");
        std::process::exit(1);
    }
}
