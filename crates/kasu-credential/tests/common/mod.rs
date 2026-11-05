#![allow(dead_code)]
use kasu_credential::{Credential, Helper, ServerKey};
use zeroize::Zeroizing;

pub fn credential(server: &str, account: Option<&str>, secret: &str) -> Credential {
    Credential {
        server: ServerKey::parse(server).unwrap(),
        account: account.map(str::to_owned),
        secret: Zeroizing::new(secret.to_owned()),
    }
}

/// The store → get → list → replace → erase → get(None) round trip every helper must pass.
pub fn round_trip(helper: &Helper) {
    let https = ServerKey::parse("https://kasu.example.com").unwrap();
    let unix = ServerKey::parse("unix:///run/kasu/node.sock").unwrap();
    assert!(helper.get_blocking(&https).unwrap().is_none());
    assert!(!helper.erase_blocking(&https).unwrap());

    helper
        .store_blocking(&credential(
            "https://kasu.example.com",
            Some("alice"),
            "tok-one",
        ))
        .unwrap();
    helper
        .store_blocking(&credential("unix:///run/kasu/node.sock", None, "tok-unix"))
        .unwrap();
    let got = helper.get_blocking(&https).unwrap().unwrap();
    assert_eq!(got.account.as_deref(), Some("alice"));
    assert_eq!(got.secret.as_str(), "tok-one");
    let got = helper.get_blocking(&unix).unwrap().unwrap();
    assert_eq!((got.account, got.secret.as_str()), (None, "tok-unix"));

    let listed = helper.list_blocking().unwrap();
    assert!(listed.iter().any(|(k, _)| *k == https));
    assert!(listed.iter().any(|(k, _)| *k == unix));

    // Storing again replaces, even when the account name changes.
    helper
        .store_blocking(&credential(
            "https://kasu.example.com",
            Some("bob"),
            "tok-two",
        ))
        .unwrap();
    let got = helper.get_blocking(&https).unwrap().unwrap();
    assert_eq!(
        (got.account.as_deref(), got.secret.as_str()),
        (Some("bob"), "tok-two")
    );

    assert!(helper.erase_blocking(&https).unwrap());
    assert!(helper.get_blocking(&https).unwrap().is_none());
    assert!(!helper.erase_blocking(&https).unwrap());
    assert!(helper.erase_blocking(&unix).unwrap());
}
