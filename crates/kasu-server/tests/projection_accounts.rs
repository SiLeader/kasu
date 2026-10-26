use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn command(root: &Path, config: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kasu-server"));
    cmd.args([
        root.as_os_str(),
        std::ffi::OsStr::new("repo"),
        std::ffi::OsStr::new("127.0.0.1:0"),
        std::ffi::OsStr::new("127.0.0.1:0"),
    ]);
    for name in [
        "KASU_TOKEN",
        "KASU_POSTGRES_DSN_FILE",
        "KASU_S3_BUCKET",
        "KASU_OBJECT_TOKEN_FILE",
        "KASU_WRITE_POLICY_FILE",
        "KASU_WEBHOOKS_FILE",
        "KASU_SERVER_LIMITS_FILE",
    ] {
        cmd.env_remove(name);
    }
    cmd.env("KASU_PROJECTION_ACCOUNTS_FILE", config)
        .env("RUST_LOG", "info");
    cmd
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_binary_loads_projection_only_configuration_and_invalid_config_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("reader.pat"), b"binary-reader\n").unwrap();
    let config = dir.path().join("accounts.json");
    std::fs::write(
        &config,
        br#"{"accounts":[{"name":"reader","token_file":"reader.pat","directories":["deploy"]}]}"#,
    )
    .unwrap();
    let log = dir.path().join("server.log");
    let mut server = Server(
        command(&dir.path().join("data"), &config)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let address = loop {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(&log).unwrap()
        );
        let text = std::fs::read_to_string(&log).unwrap();
        if let Some(address) = text
            .split("http=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
        {
            break address.to_owned();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server did not listen"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let client = reqwest::Client::new();
    let projected =
        format!("http://{address}/repo/-/path/deploy.git/info/refs?service=git-upload-pack");
    assert_eq!(
        client
            .get(&projected)
            .basic_auth("any", Some("binary-reader"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(client.get(&projected).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .get(format!(
                "http://{address}/repo.git/info/refs?service=git-upload-pack"
            ))
            .bearer_auth("binary-reader")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .post(format!("http://{address}/v1/refs"))
            .bearer_auth("binary-reader")
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&kasu_proto::v1::ListRefsRequest::default()).unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    drop(server);
    // Reusing the operator token would silently give the reader broad access.
    // Startup must reject that configuration, including in release binaries.
    let output = command(&dir.path().join("invalid"), &config)
        .env("KASU_TOKEN", "binary-reader")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("binary-reader"));
    std::fs::write(
        &config,
        br#"{"accounts":[{"name":"r","token_file":"missing.pat","directories":["deploy"]}]}"#,
    )
    .unwrap();
    assert!(
        !command(&dir.path().join("missing"), &config)
            .output()
            .unwrap()
            .status
            .success()
    );
}
