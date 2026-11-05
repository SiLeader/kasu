mod common;

use common::{credential, round_trip};
use kasu_credential::{FileStore, Helper, ServerKey};
use std::os::unix::fs::PermissionsExt;

fn helper(dir: &std::path::Path) -> (Helper, std::path::PathBuf) {
    let path = dir.join("cfg").join("credentials.json");
    (Helper::from_file_store(FileStore::new(path.clone())), path)
}

#[test]
fn file_helper_round_trips() {
    let temp = tempfile::tempdir().unwrap();
    round_trip(&helper(temp.path()).0);
}

#[test]
fn files_are_private_and_never_followed_through_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let (helper, path) = helper(temp.path());
    helper
        .store_blocking(&credential("https://a.example", None, "tok"))
        .unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);

    let target = temp.path().join("elsewhere");
    std::fs::write(&target, "{}").unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(
        helper
            .get_blocking(&ServerKey::parse("https://a.example").unwrap())
            .is_err()
    );
}

#[test]
fn concurrent_stores_do_not_lose_or_corrupt_entries() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("credentials.json");
    let threads: Vec<_> = (0..16)
        .map(|i| {
            let path = path.clone();
            std::thread::spawn(move || {
                let helper = Helper::from_file_store(FileStore::new(path));
                helper
                    .store_blocking(&credential(
                        &format!("https://s{i}.example"),
                        Some("alice"),
                        &format!("tok-{i}"),
                    ))
                    .unwrap();
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let helper = Helper::from_file_store(FileStore::new(path));
    assert_eq!(helper.list_blocking().unwrap().len(), 16);
    for i in 0..16 {
        let key = ServerKey::parse(&format!("https://s{i}.example")).unwrap();
        assert_eq!(
            helper.get_blocking(&key).unwrap().unwrap().secret.as_str(),
            format!("tok-{i}")
        );
    }
}

#[test]
fn unusable_secrets_are_rejected_and_debug_hides_them() {
    let temp = tempfile::tempdir().unwrap();
    let (helper, _) = helper(temp.path());
    for bad in ["", " tok", "tok\n", "to\u{0}k"] {
        assert!(
            helper
                .store_blocking(&credential("https://a.example", None, bad))
                .is_err(),
            "{bad:?}"
        );
    }
    let c = credential("https://a.example", Some("alice"), "super-secret-value");
    assert!(!format!("{c:?}").contains("super-secret-value"));
}
