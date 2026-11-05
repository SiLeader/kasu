mod common;

use kasu_credential::{Config, Error, Helper, ServerKey, protocol};
use std::{os::unix::fs::PermissionsExt, path::Path};

fn script(dir: &Path, name: &str, body: &str) -> Helper {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = Config::parse(
        format!(
            r#"{{"version":1,"helper_timeout_secs":1,"helpers":{{"t":{{"path":{:?}}}}}}}"#,
            path
        )
        .as_bytes(),
    )
    .unwrap();
    Helper::open("t", &config).unwrap()
}

fn server() -> ServerKey {
    ServerKey::parse("https://kasu.example.com").unwrap()
}

#[test]
fn well_behaved_helper_works() {
    let temp = tempfile::tempdir().unwrap();
    let helper = script(
        temp.path(),
        "ok",
        r#"cat >/dev/null
case "$1" in
get) echo '{"status":"ok","account":"alice","secret":"tok"}';;
erase) echo '{"status":"not_found"}';;
list) echo '{"status":"unsupported"}';;
*) echo '{"status":"ok"}';;
esac"#,
    );
    let got = helper.get_blocking(&server()).unwrap().unwrap();
    assert_eq!(
        (got.account.as_deref(), got.secret.as_str()),
        (Some("alice"), "tok")
    );
    helper
        .store_blocking(&common::credential("https://kasu.example.com", None, "x"))
        .unwrap();
    assert!(!helper.erase_blocking(&server()).unwrap());
    assert!(matches!(helper.list_blocking(), Err(Error::Unsupported)));
}

#[test]
fn request_carries_the_secret_on_stdin_only() {
    let temp = tempfile::tempdir().unwrap();
    let seen = temp.path().join("seen");
    let helper = script(
        temp.path(),
        "rec",
        &format!(
            r#"echo "$@" > {0}.args; cat > {0}.stdin; echo '{{"status":"ok"}}'"#,
            seen.display()
        ),
    );
    helper
        .store_blocking(&common::credential(
            "https://kasu.example.com",
            Some("alice"),
            "s3cret-pat",
        ))
        .unwrap();
    let args = std::fs::read_to_string(temp.path().join("seen.args")).unwrap();
    assert_eq!(args.trim(), "store");
    let stdin = std::fs::read_to_string(temp.path().join("seen.stdin")).unwrap();
    assert!(stdin.contains("s3cret-pat") && stdin.contains(r#""version":1"#));
}

#[test]
fn broken_helpers_fail_without_leaking_the_secret() {
    let temp = tempfile::tempdir().unwrap();
    let cred = common::credential("https://kasu.example.com", Some("alice"), "s3cret-pat");
    let cases = [
        (
            "garbage",
            "cat >/dev/null; echo '{\"status\": \"ok\", \"secret\": s3cret-pat'",
        ),
        (
            "badtype",
            "cat >/dev/null; echo '{\"status\": 7, \"secret\": \"s3cret-pat\"}'",
        ),
        (
            "exit",
            "cat >/dev/null; echo 'denied for s3cret-pat' >&2; exit 3",
        ),
        ("slow", "sleep 20"),
        (
            "huge",
            "cat >/dev/null; head -c 100000 /dev/zero | tr '\\0' 'a'",
        ),
        ("empty", "cat >/dev/null"),
    ];
    for (name, body) in cases {
        let helper = script(temp.path(), name, body);
        let started = std::time::Instant::now();
        let error = helper.store_blocking(&cred).unwrap_err().to_string();
        assert!(!error.contains("s3cret-pat"), "{name}: {error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{name}"
        );
    }
    let helper = script(
        temp.path(),
        "exit2",
        "cat >/dev/null; echo 'denied for s3cret-pat' >&2; exit 3",
    );
    let error = helper.store_blocking(&cred).unwrap_err().to_string();
    assert!(
        error.contains("status 3") && error.contains("denied for ***"),
        "{error}"
    );
}

#[test]
fn get_requires_a_secret_in_ok_responses() {
    let temp = tempfile::tempdir().unwrap();
    let helper = script(
        temp.path(),
        "nosecret",
        r#"cat >/dev/null; echo '{"status":"ok","secret":""}'"#,
    );
    assert!(helper.get_blocking(&server()).is_err());
}

#[test]
fn missing_helper_is_not_available() {
    let config = Config::parse(br#"{"version":1}"#).unwrap();
    let helper = Helper::open("definitely-not-installed-xyz", &config).unwrap();
    assert!(matches!(
        helper.get_blocking(&server()),
        Err(Error::NotAvailable(_))
    ));
    assert!(Helper::open("../evil", &config).is_err());
    assert!(Helper::open("Upper", &config).is_err());
}

#[test]
fn builtin_protocol_and_external_helper_agree() {
    // `kasu credential file <op>` and an external helper answer alike.
    let temp = tempfile::tempdir().unwrap();
    let file = Helper::from_file_store(kasu_credential::FileStore::new(temp.path().join("c.json")));
    let store =
        br#"{"version":1,"server":"https://kasu.example.com","account":"alice","secret":"tok"}"#;
    assert_eq!(
        protocol::handle(&file, protocol::Operation::Store, store).unwrap(),
        b"{\"status\":\"ok\"}\n"
    );
    let got = protocol::handle(
        &file,
        protocol::Operation::Get,
        br#"{"version":1,"server":"https://kasu.example.com"}"#,
    )
    .unwrap();
    let got: serde_json::Value = serde_json::from_slice(&got).unwrap();
    assert_eq!(
        got,
        serde_json::json!({"status":"ok","account":"alice","secret":"tok"})
    );
    let missing = protocol::handle(
        &file,
        protocol::Operation::Get,
        br#"{"version":1,"server":"https://other.example"}"#,
    )
    .unwrap();
    assert_eq!(missing, b"{\"status\":\"not_found\"}\n");
    assert!(
        protocol::handle(
            &file,
            protocol::Operation::Get,
            br#"{"version":2,"server":"https://x.example"}"#
        )
        .is_err()
    );
}
