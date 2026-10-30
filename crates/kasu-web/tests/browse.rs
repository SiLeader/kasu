mod common;
use axum::http::StatusCode;
use common::*;

#[tokio::test]
async fn anonymous_visitors_browse_trees_files_history_and_changesets() {
    let h = Harness::delegated(&["team/app", "team/secret"]).await;
    h.commit(
        "team/app",
        &[
            (b"README.md", Some(b"# Hello\n\n[guide](docs/guide.md)\n")),
            (b"docs/guide.md", Some(b"guide")),
            (b"src/main.rs", Some(b"fn main() {}\n")),
        ],
        "initial",
    )
    .await;
    let second = h
        .commit(
            "team/app",
            &[(
                b"src/main.rs",
                Some(b"fn main() {\n    println!(\"hi\");\n}\n"),
            )],
            "say hi",
        )
        .await;

    let top = h.get("/team/app", None).await;
    assert_eq!(top.status, StatusCode::OK, "{}", top.text());
    let text = top.text();
    assert!(text.contains("<h1>Hello</h1>"), "{text}");
    assert!(
        top.links()
            .iter()
            .any(|l| l == "/team/app/-/blob/main/docs/guide.md")
    );
    assert!(top.links().iter().any(|l| l == "/team/app/-/tree/main/src"));
    assert!(
        top.headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("script-src 'self'")
    );
    assert_eq!(top.headers["cache-control"], "private, no-cache");

    let blob = h.get("/team/app/-/blob/main/src/main.rs", None).await;
    assert_eq!(blob.status, StatusCode::OK);
    assert!(blob.text().contains("hl-"), "highlighted");
    assert_eq!(blob.count("id", "L2"), 1);
    // Anonymous visitors see no write actions.
    assert!(!blob.links().iter().any(|l| l.contains("/-/edit/")));

    let id = hex::encode(&second);
    let permalink = h
        .get(&format!("/team/app/-/blob/{id}/src/main.rs"), None)
        .await;
    assert_eq!(permalink.headers["cache-control"], "private, max-age=3600");

    let log = h.get("/team/app/-/log/main", None).await;
    assert_eq!(log.status, StatusCode::OK);
    assert!(log.text().contains("say hi") && log.text().contains("initial"));

    let changeset = h.get(&format!("/team/app/-/changeset/{id}"), None).await;
    assert_eq!(changeset.status, StatusCode::OK, "{}", changeset.text());
    assert!(
        changeset.text().contains("println!"),
        "{}",
        changeset.text()
    );
    assert!(changeset.count("class", "add") >= 2);

    let raw = h.get("/team/app/-/raw/main/src/main.rs", None).await;
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(raw.headers["content-type"], "text/plain; charset=utf-8");
    assert!(
        raw.headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    assert_eq!(
        raw.headers["content-security-policy"],
        "sandbox; default-src 'none'"
    );

    let blame = h.get("/team/app/-/blame/main/src/main.rs", None).await;
    assert_eq!(blame.status, StatusCode::OK, "{}", blame.text());
    assert!(blame.text().contains("say hi"));

    let list = h.get("/", None).await;
    assert!(list.text().contains("team/app"));
    assert!(
        !list.text().contains("team/secret"),
        "invisible to the anonymous account"
    );
}

#[tokio::test]
async fn not_found_responses_are_identical_for_missing_and_invisible_things() {
    let h = Harness::delegated(&["team/app", "other/hidden"]).await;
    h.commit("team/app", &[(b"a", Some(b"1"))], "a").await;
    h.commit("other/hidden", &[(b"a", Some(b"1"))], "a").await;
    let mut bodies = Vec::new();
    for path in [
        "/other/hidden",
        "/other/nothing",
        "/other/hidden/-/tree/main",
        "/team/app/-/tree/nobranch",
        "/team/app/-/blob/main/missing",
        &format!("/team/app/-/changeset/{}", "ab".repeat(32)),
    ] {
        let response = h.get(path, None).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
        bodies.push(response.body);
    }
    assert!(bodies.windows(2).all(|w| w[0] == w[1]));
}

#[tokio::test]
async fn directory_entries_show_their_latest_change_on_request() {
    let h = Harness::delegated(&["team/app"]).await;
    h.commit(
        "team/app",
        &[(b"a.txt", Some(b"1")), (b"b.txt", Some(b"1"))],
        "add both",
    )
    .await;
    h.commit("team/app", &[(b"b.txt", Some(b"2"))], "touch b")
        .await;
    let plain = h.get("/team/app/-/tree/main", None).await;
    assert!(
        plain
            .text()
            .contains("data-latest=\"/team/app/-/tree/main?latest=1\""),
        "{}",
        plain.text()
    );
    let with = h.get("/team/app/-/tree/main?latest=1", None).await;
    let text = with.text();
    let a = text.find(">a.txt<").unwrap();
    let b = text.find(">b.txt<").unwrap();
    assert!(text[a..b].contains("add both"), "{text}");
    assert!(text[b..].contains("touch b"), "{text}");
}

#[tokio::test]
async fn archives_stream_tar_gz_and_zip_within_the_read_scope() {
    let h = Harness::delegated(&["team/app"]).await;
    h.commit(
        "team/app",
        &[
            (b"docs/a.txt", Some(b"alpha")),
            (b"secret/b.txt", Some(b"bravo")),
        ],
        "init",
    )
    .await;
    let alice = h.login("alice").await;
    let tar = h.get("/team/app/-/archive/main", Some(&alice)).await;
    assert_eq!(tar.status, StatusCode::OK);
    assert_eq!(tar.headers["content-type"], "application/gzip");
    assert!(
        tar.headers["content-disposition"]
            .to_str()
            .unwrap()
            .contains(".tar.gz")
    );
    let mut plain = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::GzDecoder::new(tar.body.as_slice()),
        &mut plain,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&plain);
    assert!(text.contains("/docs/a.txt") && text.contains("alpha") && text.contains("bravo"));
    let zip = h
        .get("/team/app/-/archive/main/docs?format=zip", Some(&alice))
        .await;
    assert_eq!(zip.status, StatusCode::OK);
    assert!(zip.body.starts_with(b"PK\x03\x04"));
    assert!(zip.body.windows(4).any(|w| w == b"PK\x05\x06"));
    assert!(!zip.body.windows(5).any(|w| w == b"b.txt"));
    // Standard tools read both formats when they are installed.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.tar.gz"), &tar.body).unwrap();
    std::fs::write(dir.path().join("a.zip"), &zip.body).unwrap();
    for (tool, args) in [("tar", ["tzf", "a.tar.gz"]), ("unzip", ["-t", "a.zip"])] {
        if let Ok(output) = std::process::Command::new(tool)
            .args(args)
            .current_dir(dir.path())
            .output()
        {
            assert!(
                output.status.success(),
                "{tool}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("a.txt"));
        }
    }
    // A directory-limited account archives only what it can read.
    let dave = h.login("dave").await;
    let limited = h.get("/team/app/-/archive/main", Some(&dave)).await;
    let mut plain = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::GzDecoder::new(limited.body.as_slice()),
        &mut plain,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&plain);
    assert!(text.contains("alpha") && !text.contains("bravo"), "{text}");
}

#[tokio::test]
async fn zip_downloads_stream_files_larger_than_the_grpc_message_limit() {
    let h = Harness::delegated(&["team/app"]).await;
    let mut data = vec![0; 5 * 1024 * 1024];
    getrandom::fill(&mut data).unwrap();
    h.commit("team/app", &[(b"large.bin", Some(&data))], "large file")
        .await;
    let zip = h.get("/team/app/-/archive/main?format=zip", None).await;
    assert_eq!(zip.status, StatusCode::OK, "{}", zip.text());
    assert!(zip.body.starts_with(b"PK\x03\x04"));
    let compressed = u32::from_le_bytes(zip.body[18..22].try_into().unwrap()) as usize;
    assert!(compressed > 4 * 1024 * 1024);
    let name_len = u16::from_le_bytes(zip.body[26..28].try_into().unwrap()) as usize;
    let extra_len = u16::from_le_bytes(zip.body[28..30].try_into().unwrap()) as usize;
    let start = 30 + name_len + extra_len;
    let mut restored = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::DeflateDecoder::new(&zip.body[start..start + compressed]),
        &mut restored,
    )
    .unwrap();
    assert_eq!(restored, data);
    assert!(zip.body.ends_with(b"\0\0"));
    assert_eq!(
        &zip.body[zip.body.len() - 22..zip.body.len() - 18],
        b"PK\x05\x06"
    );
}

#[tokio::test]
async fn short_changeset_ids_resolve_when_unambiguous() {
    let h = Harness::delegated(&["team/app"]).await;
    let id = h
        .commit("team/app", &[(b"a.txt", Some(b"1"))], "first")
        .await;
    let short = &hex::encode(&id)[..12];
    let page = h.get(&format!("/team/app/-/tree/{short}"), None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("a.txt"));
    let changeset = h.get(&format!("/team/app/-/log/{short}"), None).await;
    assert!(changeset.text().contains("first"));
    assert_eq!(
        h.get("/team/app/-/tree/0000000000", None).await.status,
        StatusCode::NOT_FOUND
    );
}
