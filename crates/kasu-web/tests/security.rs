mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::*;
use std::net::SocketAddr;

fn proxied(h: &Harness, path: &str, peer: &str, user: &str) -> Request<Body> {
    let mut request = Request::get(path)
        .header("x-forwarded-user", user)
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        peer.parse::<SocketAddr>().unwrap(),
    ));
    let _ = h;
    request
}
async fn proxy_harness() -> Harness {
    Harness::start(
        &["team/app"],
        |b| b.with_repository_accounts(accounts()).unwrap().with_delegates(vec![delegate()]).unwrap(),
        |c| {
            c.delegate_token = Some("web-delegate".into());
            c.file.auth.proxy = Some(serde_json::from_str(r#"{"trusted_peers":["10.0.0.0/8"],"user_header":"X-Forwarded-User","accounts":{"map":{"alice@example.com":"alice"}}}"#).unwrap());
        },
    )
    .await
}

#[tokio::test]
async fn proxy_identity_is_trusted_only_from_configured_peers() {
    let h = proxy_harness().await;
    h.commit("team/app", &[(b"a", Some(b"1"))], "init").await;
    let trusted = h
        .send(proxied(
            &h,
            "/team/app",
            "10.1.2.3:5555",
            "alice@example.com",
        ))
        .await;
    assert_eq!(trusted.status, StatusCode::OK, "{}", trusted.text());
    assert!(trusted.text().contains("alice"));
    // The same header from elsewhere is ignored: anonymous access is off.
    let spoofed = h
        .send(proxied(
            &h,
            "/team/app",
            "192.168.1.1:5555",
            "alice@example.com",
        ))
        .await;
    assert_eq!(spoofed.status, StatusCode::SEE_OTHER);
    // Unmapped users are refused when anonymous access is off.
    let unknown = h
        .send(proxied(
            &h,
            "/team/app",
            "10.1.2.3:5555",
            "mallory@example.com",
        ))
        .await;
    assert_eq!(unknown.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn sessions_end_when_the_pat_changes_and_operators_cannot_log_in() {
    let h = Harness::delegated(&["team/app"]).await;
    h.commit("team/app", &[(b"a", Some(b"1"))], "init").await;
    let alice = h.login("alice").await;
    let cookie = alice
        .cookies
        .iter()
        .find(|(k, _)| k == "kasu_session")
        .unwrap()
        .1
        .clone();
    assert_eq!(
        h.get("/team/app", Some(&alice)).await.status,
        StatusCode::OK
    );
    // Same session key, same server binding key, but alice's PAT rotated.
    let rotated = Harness::start(
        &["team/app"],
        |b| {
            let mut accounts = accounts();
            accounts[0].token = "alice-rotated".into();
            b.with_binding_key(h.backend_binding_key())
                .unwrap()
                .with_repository_accounts(accounts)
                .unwrap()
                .with_delegates(vec![delegate()])
                .unwrap()
        },
        |c| {
            c.delegate_token = Some("web-delegate".into());
            c.file.auth.anonymous.enabled = true;
            c.file.auth.anonymous.account = Some("public".into());
        },
    )
    .await;
    rotated
        .commit("team/app", &[(b"a", Some(b"1"))], "init")
        .await;
    let stale = Session {
        cookies: vec![("kasu_session".into(), cookie)],
    };
    let response = rotated.get("/team/app/-/new/main", Some(&stale)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(response.location().starts_with("/-/login"));
    assert!(
        response
            .headers
            .get_all("set-cookie")
            .iter()
            .any(|v| v.to_str().unwrap().starts_with("kasu_session=;"))
    );

    // The operator token and unknown tokens do not sign in.
    let visitor = Session {
        cookies: vec![("kasu_visitor".into(), "1".repeat(32))],
    };
    let csrf = h.get("/-/login", Some(&visitor)).await.field("csrf");
    for token in ["operator", "web-delegate", "nobody"] {
        let response = h
            .post(
                "/-/login",
                Some(&visitor),
                &[("csrf", &csrf), ("token", token), ("next", "/")],
            )
            .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{token}");
    }
}

#[tokio::test]
async fn login_rejects_control_characters_in_redirect_targets() {
    let h = Harness::delegated(&["team/app"]).await;
    let visitor = Session {
        cookies: vec![("kasu_visitor".into(), "3".repeat(32))],
    };
    let csrf = h.get("/-/login", Some(&visitor)).await.field("csrf");
    let response = h
        .post(
            "/-/login",
            Some(&visitor),
            &[
                ("csrf", &csrf),
                ("token", "alice-pat"),
                ("next", "/\t//evil.example"),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/");
}

#[tokio::test]
async fn logins_are_rate_limited_per_client() {
    let h = Harness::delegated(&["team/app"]).await;
    let visitor = Session {
        cookies: vec![("kasu_visitor".into(), "2".repeat(32))],
    };
    let csrf = h.get("/-/login", Some(&visitor)).await.field("csrf");
    let mut last = StatusCode::OK;
    for _ in 0..12 {
        let body = form_urlencoded::Serializer::new(String::new())
            .extend_pairs([("csrf", csrf.as_str()), ("token", "wrong")])
            .finish();
        let mut request = Request::post("/-/login")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("origin", ORIGIN)
            .header("cookie", visitor.cookie())
            .body(Body::from(body))
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "203.0.113.9:1".parse::<SocketAddr>().unwrap(),
        ));
        last = h.send(request).await.status;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn hostile_names_messages_and_content_are_rendered_inert() {
    let h = Harness::delegated(&["team/app"]).await;
    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" onload=\"alert(1)\"/>";
    let payload: &[u8] = b"<img src=x onerror=alert(1)>";
    h.commit(
        "team/app",
        &[
            (b"<script>alert(1)</script>.txt", Some(payload)),
            (b"logo.svg", Some(svg)),
            (b"page.html", Some(b"<script>alert(1)</script>")),
            (b"README.md", Some(b"<script>alert(1)</script>\n[x](javascript:alert(1))\n<img src=x onerror=alert(1)>")),
        ],
        "<script>alert('message')</script>",
    )
    .await;
    for path in [
        "/team/app",
        "/team/app/-/log/main",
        "/team/app/-/blob/main/%3Cscript%3Ealert(1)%3C%2Fscript%3E.txt",
        "/team/app/-/blob/main/README.md",
    ] {
        let page = h.get(path, None).await;
        assert_eq!(page.status, StatusCode::OK, "{path}");
        let text = page.text();
        assert!(!text.contains("<script>alert"), "{path}: {text}");
        assert!(!text.contains("<img src=x onerror"), "{path}");
        assert!(!text.contains("javascript:alert"), "{path}");
    }
    for file in ["logo.svg", "page.html"] {
        let raw = h.get(&format!("/team/app/-/raw/main/{file}"), None).await;
        assert!(
            raw.headers["content-disposition"]
                .to_str()
                .unwrap()
                .starts_with("attachment"),
            "{file}"
        );
        assert_ne!(raw.headers["content-type"], "image/svg+xml");
        assert_ne!(raw.headers["content-type"], "text/html");
        assert_eq!(raw.headers["x-content-type-options"], "nosniff");
    }
}

#[tokio::test]
async fn cached_views_never_cross_accounts() {
    let h = Harness::delegated(&["team/app"]).await;
    let id = h
        .commit(
            "team/app",
            &[(b"docs/a", Some(b"public")), (b"secret/b", Some(b"hidden"))],
            "init",
        )
        .await;
    let alice = h.login("alice").await;
    let dave = h.login("dave").await;
    let tree = format!("/team/app/-/tree/{}", hex::encode(&id));
    let full = h.get(&tree, Some(&alice)).await;
    assert!(full.text().contains("secret"));
    let limited = h.get(&tree, Some(&dave)).await;
    assert_eq!(limited.status, StatusCode::OK);
    assert!(!limited.text().contains("secret"), "{}", limited.text());
    let raw = h
        .get(
            &format!("/team/app/-/raw/{}/secret/b", hex::encode(&id)),
            Some(&dave),
        )
        .await;
    assert_eq!(raw.status, StatusCode::NOT_FOUND);
}
