mod common;
use axum::http::StatusCode;
use common::*;

async fn branch(h: &Harness, session: &Session, name: &str) {
    let page = h.get("/team/app/-/refs", Some(session)).await;
    let csrf = page.field("csrf");
    let created = h
        .post(
            "/team/app/-/refs",
            Some(session),
            &[
                ("csrf", &csrf),
                ("action", "create"),
                ("name", name),
                ("from", "main"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
}
async fn edit_on(h: &Harness, session: &Session, branch: &str, file: &str, content: &str) {
    let form = h
        .get(&format!("/team/app/-/new/{branch}"), Some(session))
        .await;
    let mut fields: Vec<(String, String)> = [
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
    .map(|n| (n.to_string(), form.field(n)))
    .collect();
    fields.extend([
        ("path".into(), file.into()),
        ("content".into(), content.into()),
        ("final_newline".into(), "1".into()),
        ("message".into(), format!("add {file}")),
        ("destination".into(), "direct".into()),
    ]);
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let response = h
        .post(&format!("/team/app/-/new/{branch}"), Some(session), &pairs)
        .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{}",
        response.text()
    );
}

#[tokio::test]
async fn reviews_are_created_commented_approved_and_submitted_without_javascript() {
    let h = Harness::delegated(&["team/app"]).await;
    h.commit(
        "team/app",
        &[(b"a.txt", Some(b"one\ntwo\nthree\n"))],
        "init",
    )
    .await;
    h.set_config(
        "team/app",
        r#"{"review":{"rules":[{"ref_patterns":["refs/heads/main"],"required_approvals":1}]}}"#,
    );
    let alice = h.login("alice").await;
    let bob = h.login("bob").await;
    branch(&h, &alice, "topic").await;
    edit_on(&h, &alice, "topic", "b.txt", "bee").await;

    // Compare offers review creation, which walks the stack from the head.
    let compare = h
        .get("/team/app/-/compare/main...topic", Some(&alice))
        .await;
    assert_eq!(compare.status, StatusCode::OK, "{}", compare.text());
    let new_link = compare
        .links()
        .into_iter()
        .find(|l| l.contains("/-/reviews/new"))
        .expect("create review link");
    let form = h.get(&new_link, Some(&alice)).await;
    assert_eq!(form.status, StatusCode::OK, "{}", form.text());
    let fields = [
        ("csrf", form.field("csrf")),
        ("key", form.field("key")),
        ("target", form.field("target")),
        ("head", form.field("head")),
        ("base_after", form.field("base_after")),
        ("title", "Add b".into()),
        ("description", "Adds **b**.".into()),
    ];
    let pairs: Vec<(&str, &str)> = fields.iter().map(|(a, b)| (*a, b.as_str())).collect();
    let created = h
        .post("/team/app/-/reviews/new", Some(&alice), &pairs)
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
    assert_eq!(created.location(), "/team/app/-/reviews/1");

    let overview = h.get("/team/app/-/reviews/1", Some(&bob)).await;
    assert_eq!(overview.status, StatusCode::OK, "{}", overview.text());
    assert!(overview.text().contains("<strong>b</strong>"));
    assert!(overview.text().contains("0 of 1 required approvals"));

    // A line comment through plain links and forms: open the form, save a draft.
    let files = h.get("/team/app/-/reviews/1/files", Some(&bob)).await;
    assert_eq!(files.status, StatusCode::OK, "{}", files.text());
    let open = files
        .links()
        .into_iter()
        .find(|l| l.contains("comment="))
        .expect("comment link");
    let with_form = h.get(&open.replace("&amp;", "&"), Some(&bob)).await;
    let csrf = with_form.field("csrf");
    let draft = h
        .post(
            "/team/app/-/reviews/1/comment",
            Some(&bob),
            &[
                ("csrf", &csrf),
                ("revision", &with_form.field("revision")),
                ("path", &with_form.field("path")),
                ("side", &with_form.field("side")),
                ("line", &with_form.field("line")),
                ("body", "why <script>alert(1)</script>?"),
            ],
        )
        .await;
    assert_eq!(draft.status, StatusCode::SEE_OTHER, "{}", draft.text());
    // Alice does not see bob's draft.
    assert!(
        !h.get("/team/app/-/reviews/1/files", Some(&alice))
            .await
            .text()
            .contains("why")
    );
    let overview = h.get("/team/app/-/reviews/1", Some(&bob)).await;
    let csrf = overview.field("csrf");
    let published = h
        .post(
            "/team/app/-/reviews/1/publish",
            Some(&bob),
            &[("csrf", &csrf), ("body", "LGTM"), ("vote", "approve")],
        )
        .await;
    assert_eq!(
        published.status,
        StatusCode::SEE_OTHER,
        "{}",
        published.text()
    );
    let files = h.get("/team/app/-/reviews/1/files", Some(&alice)).await;
    let text = files.text();
    assert!(text.contains("why"), "{text}");
    assert!(
        !text.contains("<script>alert"),
        "comment markdown is sanitized"
    );

    let overview = h.get("/team/app/-/reviews/1", Some(&alice)).await;
    assert!(overview.text().contains("ready"), "{}", overview.text());
    let submitted = h
        .post(
            "/team/app/-/reviews/1/submit",
            Some(&alice),
            &[
                ("csrf", &overview.field("csrf")),
                ("key", &overview.field("key")),
                ("revision", &overview.field("revision")),
                ("method", "REBASE"),
            ],
        )
        .await;
    assert_eq!(
        submitted.status,
        StatusCode::SEE_OTHER,
        "{}",
        submitted.text()
    );
    let main = h.get("/team/app/-/blob/main/b.txt", Some(&alice)).await;
    assert_eq!(main.status, StatusCode::OK);
    let list = h
        .get("/team/app/-/reviews?state=submitted", Some(&alice))
        .await;
    assert!(list.text().contains("Add b"));
}

#[tokio::test]
async fn directory_limited_accounts_see_no_reviews_and_admins_manage_settings() {
    let h = Harness::delegated(&["team/app"]).await;
    h.commit(
        "team/app",
        &[(b"docs/a", Some(b"1")), (b"src/b", Some(b"2"))],
        "init",
    )
    .await;
    let dave = h.login("dave").await;
    let tree = h.get("/team/app/-/tree/main", Some(&dave)).await;
    assert_eq!(tree.status, StatusCode::OK, "{}", tree.text());
    assert!(tree.text().contains("docs"));
    assert!(!tree.text().contains(">src<"), "{}", tree.text());
    assert!(tree.text().contains("Directory-limited view"));
    assert_eq!(
        h.get("/team/app/-/blob/main/src/b", Some(&dave))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.get("/team/app/-/reviews", Some(&dave)).await.status,
        StatusCode::NOT_FOUND
    );

    let bob = h.login("bob").await;
    assert_eq!(
        h.get("/team/app/-/settings", Some(&bob)).await.status,
        StatusCode::NOT_FOUND
    );
    let alice = h.login("alice").await;
    let settings = h.get("/team/app/-/settings", Some(&alice)).await;
    assert_eq!(settings.status, StatusCode::OK, "{}", settings.text());
    let csrf = settings.field("csrf");
    let version = settings.field("version");
    let saved = h
        .post(
            "/team/app/-/settings",
            Some(&alice),
            &[
                ("csrf", &csrf),
                ("version", &version),
                ("section", "review"),
                ("patterns", "refs/heads/main"),
                ("approvals", "2"),
                ("block", "1"),
                ("keep", "1"),
                ("rebase", "1"),
            ],
        )
        .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.text());
    // A stale version shows the conflict instead of overwriting.
    let stale = h
        .post(
            "/team/app/-/settings",
            Some(&alice),
            &[
                ("csrf", &csrf),
                ("version", &version),
                ("section", "state"),
                ("state", "1"),
            ],
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    let settings = h.get("/team/app/-/settings", Some(&alice)).await;
    assert!(settings.text().contains("required_approvals"));
    assert_eq!(settings.field("approvals"), "2");
    // Creating repositories follows create_repositories.
    let new = h.get("/-/new", Some(&alice)).await;
    let created = h
        .post(
            "/-/new",
            Some(&alice),
            &[
                ("csrf", &new.field("csrf")),
                ("key", &new.field("key")),
                ("name", "team/created"),
                ("default_branch", "main"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
    assert_eq!(
        h.get("/team/created", Some(&alice)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        h.get("/-/new", Some(&bob)).await.status,
        StatusCode::FORBIDDEN
    );
}
