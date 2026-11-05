use kasu_server::*;
use kasu_store::*;
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::Arc,
};

const PAT: &str = "operator-pat-value";

struct Env {
    config: PathBuf,
    credentials: PathBuf,
    extra: Vec<(String, String)>,
}

impl Env {
    fn new(dir: &Path) -> Self {
        Self {
            config: dir.join("config"),
            credentials: dir.join("credentials.json"),
            extra: Vec::new(),
        }
    }
}

async fn kasu(env: &Env, args: &[&str], stdin: Option<&str>, token: Option<&str>) -> Output {
    let (config, credentials, extra) = (
        env.config.clone(),
        env.credentials.clone(),
        env.extra.clone(),
    );
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let stdin = stdin.map(str::to_owned);
    let token = token.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kasu"));
        command
            .args(args)
            .env("KASU_CONFIG_DIR", config)
            .env("KASU_CREDENTIALS_FILE", credentials)
            .env_remove("KASU_TOKEN")
            .env_remove("KASU_SERVER")
            .env_remove("KASU_CREDENTIAL_HELPER")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .envs(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(token) = token {
            command.env("KASU_TOKEN", token);
        }
        let mut child = command.spawn().unwrap();
        let mut pipe = child.stdin.take().unwrap();
        if let Some(text) = stdin {
            pipe.write_all(text.as_bytes()).unwrap();
        }
        drop(pipe);
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

async fn serve(temp: &Path) -> String {
    let meta = Arc::new(SqliteStore::memory().unwrap());
    let blobs = Arc::new(FsBlobStore::open(temp.join("objects")).unwrap());
    let backend = Arc::new(
        Backend::with_registry(
            meta.clone(),
            blobs,
            meta.clone(),
            meta.clone(),
            Some(PAT.into()),
        )
        .with_registry_ttl(std::time::Duration::from_millis(1)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(AdminService::new(backend.clone()).into_server())
            .add_service(IdentityService::new(backend).into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_stores_a_verified_token_that_later_commands_use() {
    let temp = tempfile::tempdir().unwrap();
    let url = serve(temp.path()).await;
    let env = Env::new(temp.path());

    // Without credentials the failure explains how to log in.
    let denied = kasu(&env, &["repo", "--server", &url, "list"], None, None).await;
    assert!(!denied.status.success());
    let message = text(&denied.stderr);
    assert!(
        message.contains("authentication required") && message.contains("kasu login"),
        "{message}"
    );

    // A wrong PAT is neither accepted nor stored.
    let bad = kasu(
        &env,
        &["login", &url, "--helper", "file", "--token-stdin"],
        Some("wrong-pat\n"),
        None,
    )
    .await;
    assert!(!bad.status.success());
    assert!(!env.credentials.exists(), "{}", text(&bad.stderr));

    // Reading from a non-terminal requires --token-stdin.
    let refused = kasu(&env, &["login", &url, "--helper", "file"], Some(PAT), None).await;
    assert!(!refused.status.success() && text(&refused.stderr).contains("--token-stdin"));

    let login = kasu(
        &env,
        &["login", &url, "--helper", "file", "--token-stdin"],
        Some(&format!("{PAT}\n")),
        None,
    )
    .await;
    assert!(login.status.success(), "{}", text(&login.stderr));
    assert!(text(&login.stdout).contains("(stored in file)"));
    assert!(text(&login.stderr).contains("plain text"));
    assert!(!text(&login.stdout).contains(PAT) && !text(&login.stderr).contains(PAT));

    // The config file never holds the secret.
    let config = std::fs::read_to_string(env.config.join("config.json")).unwrap();
    assert!(!config.contains(PAT) && config.contains("\"file\""));
    assert_eq!(
        std::fs::metadata(&env.credentials)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let created = kasu(
        &env,
        &["repo", "--server", &url, "create", "team/app"],
        None,
        None,
    )
    .await;
    assert!(created.status.success(), "{}", text(&created.stderr));

    // KASU_TOKEN keeps precedence: a wrong value is not replaced by the stored PAT.
    let overridden = kasu(
        &env,
        &["repo", "--server", &url, "list"],
        None,
        Some("wrong"),
    )
    .await;
    assert!(!overridden.status.success());

    let logout = kasu(&env, &["logout", &url], None, None).await;
    assert!(logout.status.success(), "{}", text(&logout.stderr));
    assert!(text(&logout.stdout).contains("Logged out of"));
    let again = kasu(&env, &["logout", &url], None, None).await;
    assert!(again.status.success(), "{}", text(&again.stderr));
    assert!(text(&again.stdout).contains("No stored credentials"));
    let after = kasu(&env, &["repo", "--server", &url, "list"], None, None).await;
    assert!(!after.status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_verify_stores_without_asking_the_server() {
    let temp = tempfile::tempdir().unwrap();
    let env = Env::new(temp.path());
    let login = kasu(
        &env,
        &[
            "login",
            "http://127.0.0.1:9",
            "--helper",
            "file",
            "--token-stdin",
            "--no-verify",
            "--json",
        ],
        Some("unchecked-pat"),
        None,
    )
    .await;
    assert!(login.status.success(), "{}", text(&login.stderr));
    let json: serde_json::Value = serde_json::from_slice(&login.stdout).unwrap();
    assert_eq!(json["server"], "http://127.0.0.1:9");
    assert_eq!(json["helper"], "file");
    assert!(!text(&login.stdout).contains("unchecked-pat"));
    assert!(
        std::fs::read_to_string(&env.credentials)
            .unwrap()
            .contains("unchecked-pat")
    );

    // Unreachable servers are an error unless verification is skipped.
    let unreachable = kasu(
        &env,
        &[
            "login",
            "http://127.0.0.1:9",
            "--helper",
            "file",
            "--token-stdin",
        ],
        Some("p"),
        None,
    )
    .await;
    assert!(!unreachable.status.success() && text(&unreachable.stderr).contains("--no-verify"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn helpers_are_not_consulted_when_a_token_is_given() {
    let temp = tempfile::tempdir().unwrap();
    let url = serve(temp.path()).await;
    let marker = temp.path().join("called");
    let script = temp.path().join("helper");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncat >/dev/null\ntouch {}\necho '{{\"status\":\"not_found\"}}'\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut env = Env::new(temp.path());
    std::fs::create_dir_all(&env.config).unwrap();
    std::fs::write(
        env.config.join("config.json"),
        format!(
            r#"{{"version":1,"credential_helper":"probe","helpers":{{"probe":{{"path":{:?}}}}}}}"#,
            script
        ),
    )
    .unwrap();

    let with_token = kasu(&env, &["repo", "--server", &url, "list"], None, Some(PAT)).await;
    assert!(with_token.status.success(), "{}", text(&with_token.stderr));
    assert!(
        !marker.exists(),
        "the helper ran although KASU_TOKEN was set"
    );

    let without = kasu(&env, &["repo", "--server", &url, "list"], None, None).await;
    assert!(!without.status.success());
    assert!(
        marker.exists(),
        "the helper should be consulted without a token"
    );

    // KASU_CREDENTIAL_HELPER=none disables lookups entirely.
    std::fs::remove_file(&marker).unwrap();
    env.extra
        .push(("KASU_CREDENTIAL_HELPER".into(), "none".into()));
    let disabled = kasu(&env, &["repo", "--server", &url, "list"], None, None).await;
    assert!(!disabled.status.success());
    assert!(!marker.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_subcommand_speaks_the_helper_protocol() {
    let temp = tempfile::tempdir().unwrap();
    let env = Env::new(temp.path());
    let store =
        r#"{"version":1,"server":"https://kasu.example.com","account":"alice","secret":"tok"}"#;
    let out = kasu(&env, &["credential", "file", "store"], Some(store), None).await;
    assert_eq!(
        text(&out.stdout),
        "{\"status\":\"ok\"}\n",
        "{}",
        text(&out.stderr)
    );
    let get = kasu(
        &env,
        &["credential", "file", "get"],
        Some(r#"{"version":1,"server":"https://kasu.example.com:443"}"#),
        None,
    )
    .await;
    let get: serde_json::Value = serde_json::from_slice(&get.stdout).unwrap();
    assert_eq!(get["secret"], "tok");
    let external = kasu(&env, &["credential", "custom", "get"], Some("{}"), None).await;
    assert!(!external.status.success());
}

/// Writes an external helper script and returns its path.
fn helper_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn write_config(env: &Env, json: &str) {
    std::fs::create_dir_all(&env.config).unwrap();
    std::fs::write(env.config.join("config.json"), json).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unusable_stored_pats_are_skipped_and_errors_keep_their_reason() {
    let temp = tempfile::tempdir().unwrap();
    let url = serve(temp.path()).await;
    let env = Env::new(temp.path());
    let key = kasu_credential::ServerKey::parse(&url).unwrap().to_string();
    write_config(
        &env,
        &format!(r#"{{"version":1,"servers":{{"{key}":{{"credential_helper":"file"}}}}}}"#),
    );
    std::fs::write(
        &env.credentials,
        format!(r#"{{"version":1,"credentials":{{"{key}":{{"secret":"{PAT} "}}}}}}"#),
    )
    .unwrap();
    std::fs::set_permissions(&env.credentials, std::fs::Permissions::from_mode(0o600)).unwrap();

    let out = kasu(&env, &["repo", "--server", &url, "list"], None, None).await;
    assert!(!out.status.success());
    let message = text(&out.stderr);
    assert!(
        message.contains("unusable") && message.contains("continuing without credentials"),
        "{message}"
    );
    // The server's own error is printed before the login hint.
    let lines: Vec<&str> = message.lines().collect();
    let hint = lines
        .iter()
        .position(|l| l.contains("authentication required"))
        .unwrap_or_else(|| panic!("{message}"));
    assert!(
        lines[..hint]
            .iter()
            .any(|l| l.starts_with("kasu: ") && !l.contains("warning")),
        "{message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(target_os = "linux")]
async fn logout_without_dbus_only_ignores_an_unconfigured_default() {
    let server = "http://127.0.0.1:9";
    for source in ["default", "explicit", "environment", "global", "server"] {
        let temp = tempfile::tempdir().unwrap();
        let mut env = Env::new(temp.path());
        let mut args = vec!["logout", server, "--json"];
        match source {
            "explicit" => args.extend(["--helper", "secretservice"]),
            "environment" => env
                .extra
                .push(("KASU_CREDENTIAL_HELPER".into(), "secretservice".into())),
            "global" => write_config(&env, r#"{"version":1,"credential_helper":"secretservice"}"#),
            "server" => write_config(
                &env,
                r#"{"version":1,"servers":{"http://127.0.0.1:9":{"credential_helper":"secretservice"}}}"#,
            ),
            _ => {}
        }
        let out = kasu(&env, &args, None, None).await;
        let message = text(&out.stderr);
        assert_eq!(
            out.status.success(),
            source == "default",
            "{source}: {message}"
        );
        assert!(message.contains("no D-Bus session"), "{source}: {message}");
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(json[0]["removed"], false);
        if source == "default" {
            assert!(json[0].get("error").is_none());
        } else {
            assert!(
                json[0]["error"]
                    .as_str()
                    .unwrap()
                    .contains("no D-Bus session")
            );
        }
        if source == "server" {
            let config = std::fs::read_to_string(env.config.join("config.json")).unwrap();
            assert!(config.contains(server), "{config}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_all_continues_past_a_failing_helper() {
    let temp = tempfile::tempdir().unwrap();
    let env = Env::new(temp.path());
    // The unavailable helper sorts first, so the loop must go on past it.
    write_config(
        &env,
        r#"{"version":1,"servers":{
            "http://127.0.0.1:1":{"credential_helper":"missing"},
            "http://127.0.0.1:2":{"credential_helper":"file"}}}"#,
    );
    let login = kasu(
        &env,
        &[
            "login",
            "http://127.0.0.1:2",
            "--helper",
            "file",
            "--token-stdin",
            "--no-verify",
        ],
        Some("pat-two"),
        None,
    )
    .await;
    assert!(login.status.success(), "{}", text(&login.stderr));

    let out = kasu(&env, &["logout", "--all"], None, None).await;
    assert!(!out.status.success());
    assert!(
        text(&out.stdout).contains("Logged out of http://127.0.0.1:2"),
        "{}",
        text(&out.stdout)
    );
    assert!(text(&out.stderr).contains("http://127.0.0.1:1"));
    assert!(
        !std::fs::read_to_string(&env.credentials)
            .unwrap()
            .contains("pat-two")
    );
    let config = std::fs::read_to_string(env.config.join("config.json")).unwrap();
    assert!(
        config.contains("127.0.0.1:1") && !config.contains("127.0.0.1:2"),
        "{config}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_reuses_the_recorded_helper_and_moves_pats_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    let env = Env::new(temp.path());
    let stored = temp.path().join("rec.stored");
    let rec = helper_script(
        temp.path(),
        "rec",
        &format!(
            r#"case "$1" in
store) cat > {0}; echo '{{"status":"ok"}}';;
erase) cat >/dev/null; if [ -e {0} ]; then rm {0}; echo '{{"status":"ok"}}'; else echo '{{"status":"not_found"}}'; fi;;
*) cat >/dev/null; echo '{{"status":"not_found"}}';;
esac"#,
            stored.display()
        ),
    );
    write_config(
        &env,
        &format!(r#"{{"version":1,"helpers":{{"rec":{{"path":{rec:?}}}}}}}"#),
    );
    let server = "http://127.0.0.1:9";
    let login = |helper: Option<&'static str>, pat: &'static str| {
        let env = &env;
        async move {
            let mut args = vec!["login", server, "--token-stdin", "--no-verify"];
            if let Some(helper) = helper {
                args.extend(["--helper", helper]);
            }
            let out = kasu(env, &args, Some(pat), None).await;
            assert!(out.status.success(), "{}", text(&out.stderr));
            text(&out.stdout)
        }
    };

    login(Some("file"), "pat-in-file").await;
    assert!(
        std::fs::read_to_string(&env.credentials)
            .unwrap()
            .contains("pat-in-file")
    );

    // Switching helpers removes the copy from the previous one.
    assert!(
        login(Some("rec"), "pat-in-rec")
            .await
            .contains("(stored in rec)")
    );
    assert!(
        !std::fs::read_to_string(&env.credentials)
            .unwrap()
            .contains("pat-in-file")
    );
    assert!(
        std::fs::read_to_string(&stored)
            .unwrap()
            .contains("pat-in-rec")
    );

    // A plain re-login stays with the recorded helper.
    assert!(login(None, "pat-again").await.contains("(stored in rec)"));
    assert!(
        std::fs::read_to_string(&stored)
            .unwrap()
            .contains("pat-again")
    );

    let out = kasu(&env, &["logout", server], None, None).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!stored.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_returned_by_a_helper_are_masked_in_its_stderr() {
    let temp = tempfile::tempdir().unwrap();
    let env = Env::new(temp.path());
    let leak = helper_script(
        temp.path(),
        "leak",
        r#"cat >/dev/null
echo "debug: returning tok-returned-secret" >&2
echo '{"status":"ok","secret":"tok-returned-secret"}'"#,
    );
    write_config(
        &env,
        &format!(
            r#"{{"version":1,"credential_helper":"leak","helpers":{{"leak":{{"path":{leak:?}}}}}}}"#
        ),
    );
    let out = kasu(
        &env,
        &["repo", "--server", "http://127.0.0.1:9", "list"],
        None,
        None,
    )
    .await;
    let message = text(&out.stderr);
    assert!(message.contains("debug: returning ***"), "{message}");
    assert!(!message.contains("tok-returned-secret"), "{message}");
}
