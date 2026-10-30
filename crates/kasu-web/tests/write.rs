mod common;
use axum::http::StatusCode;
use common::*;
use kasu_store::RefStore;

fn main_tip(h: &Harness, repository: &str) -> kasu_store::RefState {
    use kasu_store::RepositoryStore;
    let entry = h
        .store
        .resolve_repository_name(&kasu_core::RepoName::new(repository).unwrap(), 0)
        .unwrap()
        .unwrap();
    h.store
        .resolve(entry.key.as_bytes(), b"refs/heads/main")
        .unwrap()
}
async fn read(h: &Harness, session: &Session, path: &str) -> Vec<u8> {
    let response = h.get(path, Some(session)).await;
    assert_eq!(response.status, StatusCode::OK, "{path}");
    response.body
}
fn form_fields(page: &Response) -> Vec<(String, String)> {
    [
        "csrf",
        "key",
        "at",
        "base",
        "original_path",
        "original_blob",
        "author_name",
        "author_email",
    ]
    .iter()
    .map(|n| (n.to_string(), page.field(n)))
    .collect()
}

#[tokio::test]
async fn editing_preserves_line_endings_and_double_submission_lands_once() {
    let h = Harness::delegated(&["team/app"]).await;
    h.commit(
        "team/app",
        &[
            (b"conf.txt", Some(b"a\r\nb\r\n")),
            (b"dir/\xff.txt", Some(b"x")),
        ],
        "init",
    )
    .await;
    let alice = h.login("alice").await;
    let form = h.get("/team/app/-/edit/main/conf.txt", Some(&alice)).await;
    assert_eq!(form.status, StatusCode::OK, "{}", form.text());
    assert_eq!(form.field("author_email"), "alice@example.com");
    assert_eq!(form.field("author_name"), "Alice A");
    let mut fields = form_fields(&form);
    fields.extend([
        ("path".into(), "conf.txt".into()),
        ("content".into(), "a\r\nB".into()),
        ("line_endings".into(), "crlf".into()),
        ("final_newline".into(), "1".into()),
        ("message".into(), "Edit conf".into()),
        ("destination".into(), "direct".into()),
    ]);
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let first = h
        .post("/team/app/-/edit/main/conf.txt", Some(&alice), &pairs)
        .await;
    assert_eq!(first.status, StatusCode::SEE_OTHER, "{}", first.text());
    let version = main_tip(&h, "team/app").version;
    let second = h
        .post("/team/app/-/edit/main/conf.txt", Some(&alice), &pairs)
        .await;
    assert_eq!(second.status, StatusCode::SEE_OTHER, "{}", second.text());
    assert_eq!(first.location(), second.location());
    assert_eq!(
        main_tip(&h, "team/app").version,
        version,
        "the resubmission did not move the branch again"
    );
    assert_eq!(
        read(&h, &alice, "/team/app/-/raw/main/conf.txt").await,
        b"a\r\nB\r\n"
    );
    // The commit is attributed to alice through the delegate.
    let tip = main_tip(&h, "team/app").target.unwrap();
    let log = h
        .get(&format!("/team/app/-/changeset/{tip}"), Some(&alice))
        .await;
    assert!(log.text().contains("Alice A"), "{}", log.text());
    // A non-UTF-8 path round-trips through editing, links and deletion.
    let edit = h
        .get("/team/app/-/edit/main/dir/%FF.txt", Some(&alice))
        .await;
    assert_eq!(edit.status, StatusCode::OK, "{}", edit.text());
    let mut fields = form_fields(&edit);
    fields.extend([
        ("path".into(), edit.field("path")),
        ("content".into(), "changed".into()),
        ("message".into(), "Edit non-UTF-8 path".into()),
        ("destination".into(), "direct".into()),
    ]);
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let edited = h
        .post("/team/app/-/edit/main/dir/%FF.txt", Some(&alice), &pairs)
        .await;
    assert_eq!(edited.status, StatusCode::SEE_OTHER, "{}", edited.text());
    assert!(edited.location().ends_with("/dir/%FF.txt"));
    assert_eq!(
        read(&h, &alice, "/team/app/-/raw/main/dir/%FF.txt").await,
        b"changed"
    );
    let tree = h.get("/team/app/-/tree/main/dir", Some(&alice)).await;
    assert!(
        tree.links()
            .iter()
            .any(|l| l == "/team/app/-/blob/main/dir/%FF.txt"),
        "{:?}",
        tree.links()
    );
    let delete = h
        .get("/team/app/-/delete/main/dir/%FF.txt", Some(&alice))
        .await;
    assert_eq!(delete.status, StatusCode::OK);
    let mut fields = form_fields(&delete);
    fields.extend([
        ("message".into(), "rm".into()),
        ("destination".into(), "direct".into()),
    ]);
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let deleted = h
        .post("/team/app/-/delete/main/dir/%FF.txt", Some(&alice), &pairs)
        .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER, "{}", deleted.text());
    assert_eq!(
        h.get("/team/app/-/blob/main/dir/%FF.txt", Some(&alice))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn editing_round_trips_empty_files_and_trailing_blank_lines() {
    let h = Harness::delegated(&["team/app"]).await;
    let cases = [
        ("lf", "hello\n\n\n"),
        ("crlf", "hello\r\n\r\n\r\n"),
        ("empty", ""),
        ("unterminated", "hello"),
        ("blank", "\n\n\n"),
        ("leading", "\nhello\n\n"),
    ];
    let files: Vec<_> = cases
        .iter()
        .map(|(path, text)| (path.as_bytes(), Some(text.as_bytes())))
        .collect();
    h.commit("team/app", &files, "init").await;
    let alice = h.login("alice").await;
    for (path, text) in cases {
        let url = format!("/team/app/-/edit/main/{path}");
        let form = h.get(&url, Some(&alice)).await;
        assert_eq!(form.status, StatusCode::OK, "{}", form.text());
        assert_eq!(
            form.text()
                .contains("name=\"final_newline\" value=\"1\" checked"),
            text.ends_with('\n'),
            "{path}"
        );
        let mut fields = form_fields(&form);
        fields.extend([
            ("path".into(), form.field("path")),
            ("content".into(), form.field("content")),
            (
                "line_endings".into(),
                if text.contains("\r\n") { "crlf" } else { "lf" }.into(),
            ),
            ("message".into(), format!("Save {path} unchanged")),
            ("destination".into(), "direct".into()),
        ]);
        if text.ends_with('\n') {
            fields.push(("final_newline".into(), "1".into()));
        }
        let pairs: Vec<_> = fields
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let saved = h.post(&url, Some(&alice), &pairs).await;
        assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.text());
        assert_eq!(
            read(&h, &alice, &format!("/team/app/-/raw/main/{path}")).await,
            text.as_bytes(),
            "{path}"
        );
    }
}

#[tokio::test]
async fn branch_creation_replay_succeeds_and_conflict_allows_corrected_resubmission() {
    let h = Harness::delegated(&["team/app"]).await;
    h.commit("team/app", &[(b"a", Some(b"base\n"))], "init")
        .await;
    let alice = h.login("alice").await;

    let form = h.get("/team/app/-/edit/main/a", Some(&alice)).await;
    let mut fields = form_fields(&form);
    fields.extend([
        ("path".into(), "a".into()),
        ("content".into(), "branch content".into()),
        ("message".into(), "Create feature branch".into()),
        ("destination".into(), "branch".into()),
        ("new_branch".into(), "feature/web".into()),
    ]);
    let pairs: Vec<_> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let first = h
        .post("/team/app/-/edit/main/a", Some(&alice), &pairs)
        .await;
    assert_eq!(first.status, StatusCode::SEE_OTHER, "{}", first.text());
    let second = h
        .post("/team/app/-/edit/main/a", Some(&alice), &pairs)
        .await;
    assert_eq!(second.status, StatusCode::SEE_OTHER, "{}", second.text());
    assert_eq!(first.location(), second.location());

    let main_version = main_tip(&h, "team/app").version;
    let form = h.get("/team/app/-/edit/main/a", Some(&alice)).await;
    let mut fields = form_fields(&form);
    fields.extend([
        ("path".into(), "a".into()),
        ("content".into(), "must not replace main".into()),
        ("message".into(), "Reject existing branch".into()),
        ("destination".into(), "branch".into()),
        ("new_branch".into(), "main".into()),
    ]);
    let pairs: Vec<_> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let collision = h
        .post("/team/app/-/edit/main/a", Some(&alice), &pairs)
        .await;
    assert_eq!(
        collision.status,
        StatusCode::CONFLICT,
        "{}",
        collision.text()
    );
    assert_eq!(main_tip(&h, "team/app").version, main_version);

    let form = h.get("/team/app/-/edit/main/a", Some(&alice)).await;
    let mut fields = form_fields(&form);
    fields.extend([
        ("path".into(), "a".into()),
        ("content".into(), "mine\n".into()),
        ("message".into(), "Resolve concurrent edit".into()),
        ("destination".into(), "direct".into()),
        ("confirmed".into(), "1".into()),
    ]);
    let pairs: Vec<_> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    h.commit("team/app", &[(b"a", Some(b"theirs\n"))], "concurrent")
        .await;
    let conflict = h
        .post("/team/app/-/edit/main/a", Some(&alice), &pairs)
        .await;
    assert_eq!(conflict.status, StatusCode::CONFLICT, "{}", conflict.text());
    let old_key = fields
        .iter()
        .find(|(name, _)| name == "key")
        .unwrap()
        .1
        .clone();
    fields.iter_mut().find(|(name, _)| name == "key").unwrap().1 = conflict.field("key");
    assert_ne!(
        fields.iter().find(|(name, _)| name == "key").unwrap().1,
        old_key
    );
    fields
        .iter_mut()
        .find(|(name, _)| name == "base")
        .unwrap()
        .1 = hex::encode(main_tip(&h, "team/app").target.unwrap().0);
    fields
        .iter_mut()
        .find(|(name, _)| name == "content")
        .unwrap()
        .1 = "resolved\n".into();
    let pairs: Vec<_> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let corrected = h
        .post("/team/app/-/edit/main/a", Some(&alice), &pairs)
        .await;
    assert_eq!(
        corrected.status,
        StatusCode::SEE_OTHER,
        "{}",
        corrected.text()
    );
    assert_eq!(
        read(&h, &alice, "/team/app/-/raw/main/a").await,
        b"resolved\n"
    );
}

#[tokio::test]
async fn empty_repositories_accept_initial_commits_and_reviews() {
    let h = Harness::delegated(&[]).await;
    let alice = h.login("alice").await;
    for destination in ["direct", "review"] {
        let repository = format!("team/{destination}");
        let form = h.get("/-/new", Some(&alice)).await;
        let created = h
            .post(
                "/-/new",
                Some(&alice),
                &[
                    ("csrf", &form.field("csrf")),
                    ("key", &form.field("key")),
                    ("name", &repository),
                    ("default_branch", "trunk"),
                ],
            )
            .await;
        assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
        if destination == "review" {
            h.set_config(
                &repository,
                r#"{"review":{"rules":[{"ref_patterns":["refs/heads/trunk"]}]}}"#,
            );
        }
        let page = h.get(&created.location(), Some(&alice)).await;
        let url = page
            .links()
            .into_iter()
            .find(|url| url.contains("/-/new/"))
            .unwrap();
        let form = h.get(&url, Some(&alice)).await;
        assert_eq!(form.status, StatusCode::OK, "{}", form.text());
        assert_eq!(form.field("base"), "");
        assert!(form.links().contains(&format!("/{repository}")));
        let mut fields = form_fields(&form);
        fields.extend([
            ("path".into(), "first.txt".into()),
            ("content".into(), "first".into()),
            ("message".into(), "Initial commit".into()),
            ("destination".into(), destination.into()),
        ]);
        let pairs: Vec<_> = fields
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let saved = h.post(&url, Some(&alice), &pairs).await;
        assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.text());
        let replayed = h.post(&url, Some(&alice), &pairs).await;
        assert_eq!(
            replayed.status,
            StatusCode::SEE_OTHER,
            "{}",
            replayed.text()
        );
        assert_eq!(saved.location(), replayed.location());
        assert_eq!(
            h.get(&saved.location(), Some(&alice)).await.status,
            StatusCode::OK
        );
        if destination == "direct" {
            assert_eq!(
                read(&h, &alice, &format!("/{repository}/-/raw/trunk/first.txt")).await,
                b"first"
            );
        } else {
            assert_eq!(saved.location(), format!("/{repository}/-/reviews/1"));
        }
        assert_eq!(
            h.get(&format!("/{repository}/-/new/unknown"), Some(&alice))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn writes_require_an_account_csrf_and_same_origin() {
    let h = Harness::delegated(&["team/app"]).await;
    h.commit("team/app", &[(b"a", Some(b"1"))], "init").await;
    // Anonymous visitors are sent to the login page.
    let anonymous = h.get("/team/app/-/new/main", None).await;
    assert_eq!(anonymous.status, StatusCode::SEE_OTHER);
    assert!(anonymous.location().starts_with("/-/login"));
    let bob = h.login("bob").await;
    let form = h.get("/team/app/-/new/main", Some(&bob)).await;
    let mut fields = form_fields(&form);
    fields.extend([
        ("path".into(), "b".into()),
        ("content".into(), "2".into()),
        ("message".into(), "add".into()),
        ("destination".into(), "direct".into()),
    ]);
    let mut pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    // A wrong token is refused.
    pairs[0].1 = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    assert_eq!(
        h.post("/team/app/-/new/main", Some(&bob), &pairs)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // A token from another session is refused.
    let carol = h.login("carol").await;
    let other = h.get("/-/preferences", Some(&carol)).await.field("csrf");
    pairs[0].1 = &other;
    assert_eq!(
        h.post("/team/app/-/new/main", Some(&bob), &pairs)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // A cross-site origin is refused even with a valid token.
    let csrf = form.field("csrf");
    pairs[0].1 = &csrf;
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter())
        .finish();
    let forged = h
        .send(
            axum::http::Request::post("/team/app/-/new/main")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("origin", "https://evil.example")
                .header("cookie", bob.cookie())
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    let created = h.post("/team/app/-/new/main", Some(&bob), &pairs).await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
    // Carol is read-only: the server refuses her write.
    let form = h.get("/team/app/-/new/main", Some(&carol)).await;
    let mut fields = form_fields(&form);
    fields.extend([
        ("path".into(), "c".into()),
        ("content".into(), "3".into()),
        ("message".into(), "add".into()),
        ("destination".into(), "direct".into()),
    ]);
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    assert_eq!(
        h.post("/team/app/-/new/main", Some(&carol), &pairs)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn protected_branches_route_web_edits_to_reviews() {
    let h = Harness::delegated(&["team/app"]).await;
    h.commit("team/app", &[(b"a", Some(b"1"))], "init").await;
    h.set_config(
        "team/app",
        r#"{"review":{"rules":[{"ref_patterns":["refs/heads/main"]}]}}"#,
    );
    let alice = h.login("alice").await;
    let form = h.get("/team/app/-/edit/main/a", Some(&alice)).await;
    assert_eq!(form.status, StatusCode::OK);
    let html = form.text();
    assert!(html.contains("value=\"direct\" disabled"), "{html}");
    let mut fields = form_fields(&form);
    fields.extend([
        ("path".into(), "a".into()),
        ("content".into(), "2".into()),
        ("final_newline".into(), "1".into()),
        ("message".into(), "Change a\n\nBecause.".into()),
        ("destination".into(), "direct".into()),
    ]);
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let denied = h
        .post("/team/app/-/edit/main/a", Some(&alice), &pairs)
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.text());
    assert!(denied.text().contains("review required"));
    let mut fields = fields.clone();
    fields.retain(|(k, _)| k != "destination");
    fields.push(("destination".into(), "review".into()));
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let reviewed = h
        .post("/team/app/-/edit/main/a", Some(&alice), &pairs)
        .await;
    assert_eq!(
        reviewed.status,
        StatusCode::SEE_OTHER,
        "{}",
        reviewed.text()
    );
    assert_eq!(reviewed.location(), "/team/app/-/reviews/1");
}

#[tokio::test]
async fn branches_are_created_and_deleted_from_the_refs_page() {
    let h = Harness::delegated(&["team/app"]).await;
    h.commit("team/app", &[(b"a", Some(b"1"))], "init").await;
    let bob = h.login("bob").await;
    let page = h.get("/team/app/-/refs", Some(&bob)).await;
    let csrf = page.field("csrf");
    let created = h
        .post(
            "/team/app/-/refs",
            Some(&bob),
            &[
                ("csrf", &csrf),
                ("action", "create"),
                ("name", "feature/x"),
                ("from", "main"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
    let page = h.get("/team/app/-/refs", Some(&bob)).await;
    assert!(
        page.links()
            .iter()
            .any(|l| l == "/team/app/-/tree/feature%2Fx"),
        "{:?}",
        page.links()
    );
    let tree = h.get("/team/app/-/tree/feature%2Fx", Some(&bob)).await;
    assert_eq!(tree.status, StatusCode::OK);
    let refs = page.fields("ref");
    let targets = page.fields("target");
    let versions = page.fields("version");
    let index = refs
        .iter()
        .position(|r| r == &hex::encode(b"refs/heads/feature/x"))
        .unwrap();
    let deleted = h
        .post(
            "/team/app/-/refs",
            Some(&bob),
            &[
                ("csrf", &csrf),
                ("action", "delete"),
                ("ref", &refs[index]),
                ("target", &targets[index]),
                ("version", &versions[index]),
            ],
        )
        .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER, "{}", deleted.text());
    assert_eq!(
        h.get("/team/app/-/tree/feature%2Fx", Some(&bob))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}
