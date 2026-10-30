use super::{Ctx, Page, Prefs, Result, WebError};
use crate::{auth::Session, i18n::Lang};
use askama::Template;
use axum::{
    http::{Method, StatusCode},
    response::Response,
};
use kasu_proto::v1 as p;
use std::sync::atomic::Ordering;

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    page: Page,
    csrf: String,
    next: String,
    error: Option<String>,
}
/// Only same-site paths are followed after signing in.
fn safe_next(next: &str) -> String {
    if next.starts_with('/')
        && !next.starts_with("//")
        && !next.contains('\\')
        && !next.starts_with("/-/log")
    {
        next.to_owned()
    } else {
        "/".to_owned()
    }
}
pub async fn login(ctx: &mut Ctx) -> Result<Response> {
    if !ctx.app.config.file.auth.pat_login {
        return Err(WebError::NotFound);
    }
    let t = ctx.t;
    if ctx.method == Method::GET {
        let next = safe_next(
            &percent_encoding::percent_decode_str(ctx.q("next").unwrap_or("/")).decode_utf8_lossy(),
        );
        let page = LoginPage {
            page: ctx.page(t.s("Log in")),
            csrf: ctx.csrf("login"),
            next,
            error: None,
        };
        return Ok(ctx.html(&page, StatusCode::OK, false));
    }
    let fields = ctx.form("login").await?;
    let next = safe_next(fields.get("next").unwrap_or("/"));
    let retry = |ctx: &mut Ctx, message: &str, status: StatusCode| {
        let page = LoginPage {
            page: ctx.page(t.s("Log in")),
            csrf: ctx.csrf("login"),
            next: next.clone(),
            error: Some(message.to_owned()),
        };
        ctx.html(&page, status, false)
    };
    if !ctx.app.login_allowed(ctx.viewer.caller.client) {
        tracing::warn!(client = ?ctx.viewer.caller.client, "login rate limit exceeded");
        return Ok(retry(
            ctx,
            t.s("The server is busy; try again later."),
            StatusCode::TOO_MANY_REQUESTS,
        ));
    }
    let token = fields.text("token");
    let token = token.trim();
    let principal = match ctx.upstream().with_token(token, p::WhoAmIRequest {}) {
        Ok(request) => ctx
            .upstream()
            .identity()
            .who_am_i(request)
            .await
            .map(|r| r.into_inner()),
        Err(status) => Err(status),
    };
    match principal {
        Ok(principal)
            if !principal.account.is_empty()
                && !principal.anonymous
                && !principal.operator
                && !principal.binding.is_empty() =>
        {
            ctx.app.metrics.logins[0].fetch_add(1, Ordering::Relaxed);
            tracing::info!(account = %principal.account, client = ?ctx.viewer.caller.client, "login succeeded");
            let session = Session::new(principal.account, principal.binding, ctx.now);
            ctx.set_cookies.push(format!(
                "{}={}{}",
                ctx.app.session_cookie(),
                session.encode(&ctx.app.config.session_key),
                ctx.app.cookie_attributes()
            ));
            Ok(ctx.redirect(&next))
        }
        result => {
            ctx.app.metrics.logins[1].fetch_add(1, Ordering::Relaxed);
            tracing::info!(client = ?ctx.viewer.caller.client, reason = ?result.err().map(|s| s.code()), "login failed");
            Ok(retry(
                ctx,
                "This token cannot be used to sign in.",
                StatusCode::UNAUTHORIZED,
            ))
        }
    }
}

#[derive(Template)]
#[template(path = "logout.html")]
struct LogoutPage {
    page: Page,
    csrf: String,
}
pub async fn logout(ctx: &mut Ctx) -> Result<Response> {
    if ctx.method == Method::GET {
        let page = LogoutPage {
            page: ctx.page(ctx.t.s("Log out")),
            csrf: ctx.csrf("logout"),
        };
        return Ok(ctx.html(&page, StatusCode::OK, false));
    }
    ctx.form("logout").await?;
    ctx.set_cookies.push(format!(
        "{}=; Max-Age=0{}",
        ctx.app.session_cookie(),
        ctx.app.cookie_attributes()
    ));
    Ok(ctx.redirect("/"))
}

#[derive(Template)]
#[template(path = "preferences.html")]
struct PreferencesPage {
    page: Page,
    csrf: String,
    lang: &'static str,
    tz: String,
    split: bool,
}
pub async fn preferences(ctx: &mut Ctx) -> Result<Response> {
    if ctx.method == Method::POST {
        let fields = ctx.form("preferences").await?;
        let prefs = Prefs {
            lang: fields.get("lang").and_then(Lang::parse),
            tz: fields
                .get("tz")
                .filter(|tz| !tz.is_empty() && jiff::tz::TimeZone::get(tz).is_ok())
                .map(str::to_owned),
            split: fields.get("diff") == Some("split"),
        };
        ctx.set_cookies.push(format!(
            "kasu_prefs={}; Path=/; SameSite=Lax; Max-Age=31536000{}",
            prefs.encode(),
            if ctx.app.config.secure() {
                "; Secure"
            } else {
                ""
            }
        ));
        return Ok(ctx.redirect("/-/preferences"));
    }
    let page = PreferencesPage {
        page: ctx.page(ctx.t.s("Preferences")),
        csrf: ctx.csrf("preferences"),
        lang: ctx.prefs.lang.map(Lang::code).unwrap_or(""),
        tz: ctx.prefs.tz.clone().unwrap_or_default(),
        split: ctx.prefs.split,
    };
    Ok(ctx.html(&page, StatusCode::OK, false))
}
