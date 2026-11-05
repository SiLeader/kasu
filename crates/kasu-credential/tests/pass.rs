//! Needs `pass` and `gpg`; run with `cargo test -p kasu-credential --test pass -- --ignored`.
mod common;

use kasu_credential::{Config, Helper};
use std::process::{Command, Stdio};

#[test]
#[ignore = "needs pass and gpg"]
fn pass_helper_round_trips() {
    let temp = tempfile::tempdir().unwrap();
    let gnupg = temp.path().join("gnupg");
    std::fs::create_dir(&gnupg).unwrap();
    std::fs::set_permissions(&gnupg, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let store = temp.path().join("store");
    let run = |program: &str, args: &[&str]| {
        let status = Command::new(program)
            .args(args)
            .env("GNUPGHOME", &gnupg)
            .env("PASSWORD_STORE_DIR", &store)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{program} {args:?}");
    };
    run(
        "gpg",
        &[
            "--batch",
            "--passphrase",
            "",
            "--quick-gen-key",
            "kasu-test@example.invalid",
            "default",
            "default",
            "never",
        ],
    );
    run("pass", &["init", "kasu-test@example.invalid"]);
    // SAFETY: this test binary has one test, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("GNUPGHOME", &gnupg);
        std::env::set_var("PASSWORD_STORE_DIR", &store);
    }
    let helper = Helper::open("pass", &Config::parse(br#"{"version":1}"#).unwrap()).unwrap();
    common::round_trip(&helper);
}

#[test]
fn uninitialised_store_is_not_available() {
    // Uses a child process so the environment change cannot race other tests.
    if std::env::var_os("KASU_PASS_CHILD").is_some() {
        let error = Helper::open("pass", &Config::parse(br#"{"version":1}"#).unwrap()).unwrap_err();
        assert!(
            matches!(error, kasu_credential::Error::NotAvailable(_)),
            "{error}"
        );
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "uninitialised_store_is_not_available",
            "--exact",
            "--nocapture",
        ])
        .env("KASU_PASS_CHILD", "1")
        .env("PASSWORD_STORE_DIR", temp.path())
        .status()
        .unwrap();
    assert!(status.success());
}
