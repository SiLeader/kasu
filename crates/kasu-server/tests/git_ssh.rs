use kasu_core::*;
use kasu_derived::Engine;
use kasu_git::*;
use kasu_ingest::Importer;
use kasu_store::*;
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
};

fn run(mut cmd: Command) -> Output {
    let result = cmd.output().unwrap();
    assert!(
        result.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    result
}
struct Fixture {
    temp: tempfile::TempDir,
    source: PathBuf,
    store: PathBuf,
    env: Vec<(&'static str, String)>,
    ssh: PathBuf,
    reader_ssh: PathBuf,
    writer_ssh: PathBuf,
    remote: String,
    sshd: Option<Child>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.sshd {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}
impl Fixture {
    fn new(real: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = root.join("source");
        let store = root.join("store");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&store).unwrap();
        let operator = root.join("operator.token");
        let reader = root.join("reader.token");
        fs::write(&operator, "operator-token\n").unwrap();
        fs::write(&reader, "reader-token\n").unwrap();
        let writer = root.join("writer.token");
        fs::write(&writer, "writer-token\n").unwrap();
        fs::write(root.join("writers.json"), serde_json::json!({"accounts":[{"name":"deploy-writer","token_file":"writer.token","write_policy":{"allowed_path_prefixes":["deploy"],"allowed_ref_patterns":["refs/heads/main"]}}]}).to_string()).unwrap();
        fs::write(root.join("accounts.json"),serde_json::json!({"accounts":[{"name":"reader","token_file":"reader.token","directories":["deploy"]}]}).to_string()).unwrap();
        fs::write(root.join("policy.json"),serde_json::json!({"allow_delete":true,"force_ref_patterns":["refs/heads/automation/*"]}).to_string()).unwrap();
        let mut env = vec![
            ("PATH", std::env::var("PATH").unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
            ("GIT_AUTHOR_NAME", "SSH fixture".into()),
            ("GIT_AUTHOR_EMAIL", "ssh@kasu.invalid".into()),
            ("GIT_COMMITTER_NAME", "SSH fixture".into()),
            ("GIT_COMMITTER_EMAIL", "ssh@kasu.invalid".into()),
            ("GIT_AUTHOR_DATE", "2026-10-24T00:00:00Z".into()),
            ("GIT_COMMITTER_DATE", "2026-10-24T00:00:00Z".into()),
            ("KASU_TOKEN_FILE", operator.to_str().unwrap().into()),
            (
                "KASU_ACCOUNTS_FILE",
                root.join("writers.json").to_str().unwrap().into(),
            ),
            (
                "KASU_PROJECTION_ACCOUNTS_FILE",
                root.join("accounts.json").to_str().unwrap().into(),
            ),
            (
                "KASU_WRITE_POLICY_FILE",
                root.join("policy.json").to_str().unwrap().into(),
            ),
        ];
        if let Some(capture) = std::env::var_os("KASU_CAPTURE_DIR") {
            let capture = PathBuf::from(capture);
            fs::create_dir_all(&capture).unwrap();
            env.push((
                "GIT_TRACE_PACKET",
                capture
                    .join(if real {
                        "git-ssh-openssh.trace"
                    } else {
                        "git-ssh-stdio.trace"
                    })
                    .to_str()
                    .unwrap()
                    .to_owned(),
            ));
        }
        let ssh = root.join("ssh-client");
        let reader_ssh = root.join("ssh-reader");
        let writer_ssh = root.join("ssh-writer");
        let mut f = Self {
            temp,
            source,
            store,
            env,
            ssh,
            reader_ssh,
            writer_ssh,
            remote: "ssh://git@kasu.invalid/repo.git".into(),
            sshd: None,
        };
        f.git(&f.source, &["init", "--template=", "--initial-branch=main"]);
        fs::create_dir(f.source.join("deploy")).unwrap();
        fs::write(f.source.join("private.txt"), b"private root content\n").unwrap();
        for n in 0..4 {
            fs::write(
                f.source.join("deploy/manifest.yaml"),
                format!("revision: {n}\n"),
            )
            .unwrap();
            f.git(&f.source, &["add", "."]);
            f.git(&f.source, &["commit", "-m", &format!("fixture {n}")]);
        }
        f.git(
            &f.source,
            &["tag", "-a", "release", "-m", "annotated release"],
        );
        f.publish();
        if real {
            f.real_ssh();
        } else {
            for (wrapper, credential) in [
                (&f.ssh, &operator),
                (&f.reader_ssh, &reader),
                (&f.writer_ssh, &writer),
            ] {
                executable(
                    wrapper,
                    &format!(
                        "#!/bin/sh\nwhile [ \"$#\" -gt 1 ]; do shift; done\nexport SSH_ORIGINAL_COMMAND=\"$1\"\nexec {} {} repo {}\n",
                        quote(Path::new(env!("CARGO_BIN_EXE_kasu-git-ssh"))),
                        quote(&f.store),
                        quote(credential)
                    ),
                );
            }
        }
        f
    }
    fn command(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.env_clear();
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c
    }
    fn git_result(&self, path: &Path, args: &[&str]) -> Output {
        let mut c = self.command("git");
        c.arg("-C")
            .arg(path)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .env("GIT_SSH_COMMAND", &self.ssh)
            .env("GIT_SSH_VARIANT", "ssh");
        c.output().unwrap()
    }
    fn git(&self, path: &Path, args: &[&str]) -> String {
        let o = self.git_result(path, args);
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap().trim().to_owned()
    }
    fn publish(&self) {
        let meta = SqliteStore::open(self.store.join("meta.db")).unwrap();
        let blobs = FsBlobStore::open(self.store.join("objects")).unwrap();
        let mut engine = Engine::standard();
        register_git(&mut engine);
        let importer = Importer {
            engine: &engine,
            index: GitIndex {
                repo: Repository {
                    meta: &meta,
                    blobs: &blobs,
                },
                projection: ProjectionId::full(b"repo".as_slice()),
            },
        };
        let report = importer.import(&self.source).unwrap();
        importer.publish(&meta, &report, b"ssh-fixture", 0).unwrap();
    }
    fn real_ssh(&mut self) {
        let root = self.temp.path();
        let host = root.join("host-key");
        let key = root.join("client-key");
        let reader = root.join("reader-key");
        let writer = root.join("writer-key");
        for path in [&host, &key, &reader, &writer] {
            let mut c = self.command("ssh-keygen");
            c.args(["-q", "-t", "ed25519", "-N", ""])
                .arg("-f")
                .arg(path);
            run(c);
        }
        let mut keys = String::new();
        for (key, credential) in [
            (&key, root.join("operator.token")),
            (&reader, root.join("reader.token")),
            (&writer, root.join("writer.token")),
        ] {
            let forced = format!(
                "{} {} repo {}",
                quote(Path::new(env!("CARGO_BIN_EXE_kasu-git-ssh"))),
                quote(&self.store),
                quote(&credential)
            );
            keys.push_str(&format!(
                "restrict,command=\"{}\" {}",
                forced.replace('\\', "\\\\").replace('"', "\\\""),
                fs::read_to_string(key.with_extension("pub")).unwrap()
            ));
        }
        fs::write(root.join("authorized_keys"), keys).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut c = self.command("ssh-keygen");
        c.args(["-y", "-f"]).arg(&host);
        let hostkey = String::from_utf8(run(c).stdout).unwrap();
        fs::write(
            root.join("known_hosts"),
            format!("[127.0.0.1]:{port} {hostkey}"),
        )
        .unwrap();
        fs::write(root.join("sshd_config"),format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nStrictModes no\nUsePAM no\nPermitRootLogin prohibit-password\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\nAuthenticationMethods publickey\nPermitUserEnvironment no\nAcceptEnv GIT_PROTOCOL\nAllowTcpForwarding no\nAllowAgentForwarding no\nX11Forwarding no\nPermitTTY no\nSetEnv KASU_TOKEN_FILE={} KASU_PROJECTION_ACCOUNTS_FILE={} KASU_WRITE_POLICY_FILE={} KASU_ACCOUNTS_FILE={}\n",host.display(),root.join("sshd.pid").display(),root.join("authorized_keys").display(),root.join("operator.token").display(),root.join("accounts.json").display(),root.join("policy.json").display(),root.join("writers.json").display())).unwrap();
        let log = fs::File::create(root.join("sshd.log")).unwrap();
        let mut c = self.command("/usr/sbin/sshd");
        c.args(["-D", "-e", "-f"])
            .arg(root.join("sshd_config"))
            .stdout(Stdio::null())
            .stderr(log);
        self.sshd = Some(c.spawn().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(
                self.sshd.as_mut().unwrap().try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(root.join("sshd.log")).unwrap()
            );
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        for (wrapper, key) in [
            (&self.ssh, &key),
            (&self.reader_ssh, &reader),
            (&self.writer_ssh, &writer),
        ] {
            executable(
                wrapper,
                &format!(
                    "#!/bin/sh\nexec ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile={} -i {} -p {port} \"$@\"\n",
                    quote(&root.join("known_hosts")),
                    quote(key)
                ),
            );
        }
        self.remote = format!("ssh://root@127.0.0.1:{port}/repo.git");
    }
    fn exercise(&self) {
        let initial = self.git(&self.source, &["rev-parse", "HEAD"]);
        for protocol in ["0", "1", "2"] {
            for shallow in [false, true] {
                let target = self.temp.path().join(format!("p{protocol}-s{shallow}"));
                let setting = format!("protocol.version={protocol}");
                let mut args = vec!["-c", &setting, "clone", "--no-single-branch"];
                if shallow {
                    args.extend(["--depth", "1"]);
                }
                args.extend([&self.remote, target.to_str().unwrap()]);
                self.git(self.temp.path(), &args);
                assert_eq!(self.git(&target, &["rev-parse", "HEAD"]), initial);
                self.git(&target, &["fsck", "--strict"]);
                if shallow {
                    self.git(
                        &target,
                        &[
                            "-c",
                            &format!("protocol.version={protocol}"),
                            "fetch",
                            "--deepen",
                            "1",
                            "origin",
                        ],
                    );
                }
                self.git(&target, &["fetch", "origin"]);
                self.git(&target, &["fsck", "--strict"]);
            }
        }
        let client = self.temp.path().join("p2-sfalse");
        fs::write(client.join("deploy/manifest.yaml"), b"revision: from-ssh\n").unwrap();
        self.git(&client, &["add", "."]);
        self.git(&client, &["commit", "-m", "SSH native push"]);
        self.git(&client, &["push", "origin", "main"]);
        // An existing object still needs a self-delimited empty pack. Deletion sends no pack.
        self.git(&client, &["push", "origin", "main:refs/heads/temporary"]);
        self.git(&client, &["push", "origin", ":refs/heads/temporary"]);
        self.git(&client, &["tag", "-a", "ssh-tag", "-m", "SSH tag"]);
        self.git(&client, &["branch", "atomic-branch"]);
        self.git(
            &client,
            &[
                "push",
                "--atomic",
                "origin",
                "refs/heads/atomic-branch:refs/heads/atomic-branch",
                "refs/tags/ssh-tag:refs/tags/ssh-tag",
            ],
        );
        let expected = self.git(&client, &["rev-parse", "HEAD"]);
        for protocol in ["0", "1", "2"] {
            for shallow in [false, true] {
                let target = self.temp.path().join(format!("p{protocol}-s{shallow}"));
                self.git(
                    &target,
                    &[
                        "-c",
                        &format!("protocol.version={protocol}"),
                        "fetch",
                        "origin",
                    ],
                );
                assert_eq!(self.git(&target, &["rev-parse", "origin/main"]), expected);
                self.git(&target, &["fsck", "--strict"]);
                if shallow {
                    assert!(target.join(".git/shallow").exists());
                    self.git(
                        &target,
                        &[
                            "-c",
                            &format!("protocol.version={protocol}"),
                            "fetch",
                            "--unshallow",
                            "origin",
                        ],
                    );
                    assert!(!target.join(".git/shallow").exists());
                    self.git(&target, &["fsck", "--strict"]);
                }
            }
        }
        let projected = self.remote.replace("/repo.git", "/repo/-/path/deploy.git");
        let target = self.temp.path().join("projection");
        self.git(
            self.temp.path(),
            &["clone", &projected, target.to_str().unwrap()],
        );
        assert_eq!(
            fs::read(target.join("manifest.yaml")).unwrap(),
            b"revision: from-ssh\n"
        );
        assert!(!target.join("private.txt").exists());
        let mut c = self.command("git");
        c.args(["ls-remote", &projected])
            .env("GIT_SSH_COMMAND", &self.reader_ssh)
            .env("GIT_SSH_VARIANT", "ssh");
        run(c);
        let mut c = self.command("git");
        c.args(["ls-remote", &self.remote])
            .env("GIT_SSH_COMMAND", &self.reader_ssh)
            .env("GIT_SSH_VARIANT", "ssh");
        assert!(!c.output().unwrap().status.success());
        let partial = self.temp.path().join("partial");
        self.git(
            self.temp.path(),
            &[
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                &self.remote,
                partial.to_str().unwrap(),
            ],
        );
        self.git(&partial, &["checkout", "main"]);
        assert_eq!(
            fs::read(partial.join("deploy/manifest.yaml")).unwrap(),
            b"revision: from-ssh\n"
        );
        self.git(&partial, &["fsck", "--strict"]);
        let meta = SqliteStore::open(self.store.join("meta.db")).unwrap();
        let blobs = FsBlobStore::open(self.store.join("objects")).unwrap();
        let index = GitIndex {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
            projection: ProjectionId::full(b"repo".as_slice()),
        };
        let refstate = meta.resolve(b"repo", b"refs/heads/main").unwrap();
        assert!(refstate.target.is_some());
        let reachable = reachable(&index, [expected.parse().unwrap()], &BTreeSet::new()).unwrap();
        assert!(reachable.contains(&GitOid::hash("blob", b"revision: from-ssh\n")));
        let engine = Engine::standard();
        let ops = kasu_ops::Operations {
            engine: &engine,
            ctx: kasu_derived::Ctx {
                repo: Repository {
                    meta: &meta,
                    blobs: &blobs,
                },
            },
        };
        assert_eq!(
            ops.read_file(
                refstate.target.unwrap(),
                &RepoPath::new(b"deploy/manifest.yaml".as_slice()).unwrap()
            )
            .unwrap()
            .1,
            b"revision: from-ssh\n"
        );
        let mut go_cases = Vec::new();
        if self.sshd.is_some()
            && let Some(binary) = std::env::var_os("KASU_GO_GIT_CLIENT")
        {
            for depth in [0, 1] {
                let mut c = self.command(binary.to_str().unwrap());
                c.arg(&self.remote)
                    .arg(self.temp.path().join(format!("go-{depth}")))
                    .arg(depth.to_string())
                    .env("KASU_SSH_USER", "root")
                    .env("KASU_SSH_KEY", self.temp.path().join("client-key"))
                    .env("KASU_SSH_KNOWN_HOSTS", self.temp.path().join("known_hosts"))
                    .env("KASU_PUSH", "1");
                let result = run(c);
                let oid = String::from_utf8(result.stdout).unwrap().trim().to_owned();
                let target = self.temp.path().join(format!("go-{depth}"));
                self.git(&target, &["fsck", "--strict"]);
                if let Some(capture) = std::env::var_os("KASU_CAPTURE_DIR") {
                    fs::write(
                        PathBuf::from(capture).join(format!("go-git-ssh-depth{depth}.trace")),
                        result.stderr,
                    )
                    .unwrap();
                }
                let native = meta
                    .resolve(b"repo", b"refs/heads/main")
                    .unwrap()
                    .target
                    .unwrap();
                assert_eq!(
                    index.find_changeset(oid.parse().unwrap()).unwrap(),
                    Some(native)
                );
                assert_eq!(
                    ops.read_file(
                        native,
                        &RepoPath::new(b"go-git-writeback.yaml".as_slice()).unwrap()
                    )
                    .unwrap()
                    .1,
                    format!("client: go-git\ndepth: {depth}\n").into_bytes()
                );
                go_cases.push(serde_json::json!({"depth":depth,"pushed":oid}));
            }
        }
        let writer_target = self.temp.path().join("writer-checkout");
        self.git(
            self.temp.path(),
            &["clone", &self.remote, writer_target.to_str().unwrap()],
        );
        fs::write(
            writer_target.join("deploy/manifest.yaml"),
            b"revision: repository-account\n",
        )
        .unwrap();
        self.git(
            &writer_target,
            &["commit", "-am", "repository account allowed path"],
        );
        let push_as_writer = |args: &[&str]| {
            let mut c = self.command("git");
            c.arg("-C")
                .arg(&writer_target)
                .args(args)
                .env("GIT_SSH_COMMAND", &self.writer_ssh)
                .env("GIT_SSH_VARIANT", "ssh");
            c.output().unwrap()
        };
        let allowed = push_as_writer(&["push", "origin", "HEAD:main"]);
        assert!(
            allowed.status.success(),
            "{}",
            String::from_utf8_lossy(&allowed.stderr)
        );
        let accepted = self.git(&writer_target, &["rev-parse", "HEAD"]);
        fs::write(
            writer_target.join("private.txt"),
            b"forbidden repository-account write\n",
        )
        .unwrap();
        self.git(
            &writer_target,
            &["commit", "-am", "repository account denied path"],
        );
        assert!(
            !push_as_writer(&["push", "origin", "HEAD:main"])
                .status
                .success()
        );
        assert!(
            !push_as_writer(&["push", "origin", ":main"])
                .status
                .success()
        );
        assert!(
            !push_as_writer(&["push", "origin", "HEAD:refs/heads/denied"])
                .status
                .success()
        );
        assert_eq!(
            self.git(&writer_target, &["ls-remote", "origin", "refs/heads/main"])
                .split_whitespace()
                .next(),
            Some(accepted.as_str())
        );
        self.git(&writer_target, &["fsck", "--strict"]);
        let native_writer = meta
            .resolve(b"repo", b"refs/heads/main")
            .unwrap()
            .target
            .unwrap();
        assert_eq!(
            ops.read_file(
                native_writer,
                &RepoPath::new(b"deploy/manifest.yaml".as_slice()).unwrap()
            )
            .unwrap()
            .1,
            b"revision: repository-account\n"
        );
        if let Some(capture) = std::env::var_os("KASU_CAPTURE_DIR") {
            let mut c = self.command("git");
            c.arg("--version");
            let version = String::from_utf8(run(c).stdout).unwrap();
            let mut c = self.command("ssh");
            c.arg("-V");
            let ssh_version = String::from_utf8(run(c).stderr).unwrap();
            let report = serde_json::json!({"success":true,"git":version.trim(),"ssh":ssh_version.trim(),"real_openssh":self.sshd.is_some(),"initial":initial,"git_cli_push":expected,"protocols":[0,1,2],"clone_modes":["full","depth1"],"strict_fsck":true,"native_contents_match":true,"repository_account_writes":true,"go_git":go_cases});
            fs::write(
                PathBuf::from(capture).join(if self.sshd.is_some() {
                    "ssh-openssh.json"
                } else {
                    "ssh-stdio.json"
                }),
                serde_json::to_vec_pretty(&report).unwrap(),
            )
            .unwrap();
        }
    }
}
#[test]
fn git_over_forced_command_stdio_supports_all_protocols_read_push_and_projection() {
    Fixture::new(false).exercise();
}
#[test]
#[ignore = "requires root, OpenSSH sshd and /run/sshd; tools/ssh/Dockerfile"]
fn git_over_real_openssh_supports_all_protocols_read_push_and_projection() {
    Fixture::new(true).exercise();
}

#[test]
fn idle_stdio_sessions_exit_at_the_configured_deadline() {
    use std::io::Read;
    let f = Fixture::new(false);
    let mut cmd = f.command(env!("CARGO_BIN_EXE_kasu-git-ssh"));
    cmd.arg(&f.store)
        .arg("repo")
        .arg(f.temp.path().join("operator.token"))
        .env("SSH_ORIGINAL_COMMAND", "git-upload-pack 'repo.git'")
        .env("KASU_SSH_TIMEOUT_SECS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(124));
            break;
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("SSH deadline did not terminate idle session");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let mut error = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut error)
        .unwrap();
    assert!(error.contains("session deadline exceeded"));
}
