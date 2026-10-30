use super::{Ctx, Fields, Page, ReadExt, Repo, Result, WebError};
use crate::{render::text::lossy, rev};
use askama::Template;
use axum::{
    http::{Method, StatusCode},
    response::Response,
};
use kasu_proto::v1 as p;
use serde_json::{Value, json};

#[derive(Template)]
#[template(path = "new_repository.html")]
struct NewRepositoryPage {
    page: Page,
    csrf: String,
    key: String,
    name: String,
    branch: String,
    patterns: Vec<String>,
    error: Option<String>,
}
pub async fn new_repository(ctx: &mut Ctx) -> Result<Response> {
    ctx.require_account()?;
    let me = rpc!(ctx, identity.who_am_i(p::WhoAmIRequest {}))
        .await
        .read()?;
    if !me.operator && me.create_repositories.is_empty() {
        return Err(WebError::Forbidden("you cannot create repositories".into()));
    }
    let t = ctx.t;
    let render = |ctx: &mut Ctx,
                  name: String,
                  branch: String,
                  key: String,
                  error: Option<String>,
                  status| {
        let page = NewRepositoryPage {
            page: ctx.page(t.s("New repository")),
            csrf: ctx.csrf("new_repository"),
            key,
            name,
            branch,
            patterns: me.create_repositories.clone(),
            error,
        };
        ctx.html(&page, status, false)
    };
    if ctx.method == Method::GET {
        return Ok(render(
            ctx,
            String::new(),
            "main".into(),
            Ctx::idempotency_key(),
            None,
            StatusCode::OK,
        ));
    }
    let fields = ctx.form("new_repository").await?;
    let name = fields.text("name").trim().to_owned();
    let branch = fields.text("default_branch").trim().to_owned();
    if kasu_core::RepoName::new(name.clone()).is_err() {
        return Ok(render(
            ctx,
            name,
            branch,
            fields.text("key"),
            Some("invalid repository name".into()),
            StatusCode::BAD_REQUEST,
        ));
    }
    let created = ctx
        .upstream()
        .admin()
        .create_repository(ctx.wreq(p::CreateRepositoryRequest {
            name: name.clone(),
            default_branch: branch.as_bytes().to_vec(),
            config_json: String::new(),
            idempotency_key: fields.text("key").into_bytes(),
        }))
        .await;
    match created {
        Ok(created) => {
            let created = created.into_inner();
            tracing::info!(account = ?ctx.viewer.account(), repository = %created.name, "repository created through the web UI");
            Ok(ctx.redirect(&rev::repo_url(&created.name)))
        }
        Err(status) if status.code() == tonic::Code::AlreadyExists => Ok(render(
            ctx,
            name,
            branch,
            Ctx::idempotency_key(),
            Some(t.s("A repository with this name already exists.").into()),
            StatusCode::CONFLICT,
        )),
        Err(status) => Err(WebError::write(status)),
    }
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    page: Page,
    csrf: String,
    key: String,
    version: u64,
    default_branch: String,
    state: i32,
    config: String,
    rule: RuleForm,
    extra_rules: bool,
    webhooks: Vec<(String, String, String)>,
    usage: Option<p::RepositoryUsage>,
    error: Option<String>,
    submitted: Option<String>,
}
#[derive(Default)]
struct RuleForm {
    patterns: String,
    approvals: u64,
    approvers: String,
    self_approval: bool,
    block: bool,
    keep: bool,
    resolved: bool,
    rebase: bool,
    squash: bool,
    bypass: String,
}
fn join(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}
fn split(text: &str) -> Vec<Value> {
    text.split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Value::String(s.to_owned()))
        .collect()
}
fn rule_form(config: &Value) -> (RuleForm, bool) {
    let review = &config["review"];
    let Some(rule) = review["rules"].get(0) else {
        return (
            RuleForm {
                approvals: 1,
                block: true,
                keep: true,
                rebase: true,
                squash: true,
                bypass: join(review.get("bypass_accounts")),
                ..Default::default()
            },
            false,
        );
    };
    let methods = rule.get("methods").and_then(Value::as_array);
    let has = |m: &str| methods.is_none_or(|a| a.iter().any(|v| v == m));
    (
        RuleForm {
            patterns: join(rule.get("ref_patterns")),
            approvals: rule["required_approvals"].as_u64().unwrap_or(1),
            approvers: join(rule.get("approvers")),
            self_approval: rule["allow_self_approval"].as_bool().unwrap_or(false),
            block: rule["block_on_request_changes"].as_bool().unwrap_or(true),
            keep: rule["keep_approvals_on_trivial_rebase"]
                .as_bool()
                .unwrap_or(true),
            resolved: rule["require_resolved_threads"].as_bool().unwrap_or(false),
            rebase: has("REBASE"),
            squash: has("SQUASH"),
            bypass: join(review.get("bypass_accounts")),
        },
        review["rules"].as_array().is_some_and(|r| r.len() > 1),
    )
}
async fn show(
    ctx: &mut Ctx,
    repo: Repo,
    error: Option<String>,
    submitted: Option<String>,
    status: StatusCode,
) -> Result<Response> {
    let config: Value = if repo.record.config_json.is_empty() {
        json!({})
    } else {
        serde_json::from_str(&repo.record.config_json).unwrap_or(json!({}))
    };
    let (rule, extra_rules) = rule_form(&config);
    let webhooks = config["webhooks"]
        .as_array()
        .map(|hooks| {
            hooks
                .iter()
                .map(|h| {
                    (
                        h["id"].as_str().unwrap_or_default().to_owned(),
                        h["url"].as_str().unwrap_or_default().to_owned(),
                        h["secret_file"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let usage = rpc!(
        ctx,
        admin.get_repository_usage(p::GetRepositoryRequest {
            name: repo.name.clone()
        })
    )
    .await
    .ok();
    let mut page = ctx.page(format!("{} {}", ctx.t.s("Settings"), repo.name));
    page.repo = Some(ctx.nav(&repo, "settings", None, true));
    let body = SettingsPage {
        page,
        csrf: ctx.csrf("settings"),
        key: Ctx::idempotency_key(),
        version: repo.record.config_version,
        default_branch: lossy(&repo.record.default_branch),
        state: repo.record.state,
        config: serde_json::to_string_pretty(&config).unwrap_or_default(),
        rule,
        extra_rules,
        webhooks,
        usage,
        error,
        submitted,
    };
    Ok(ctx.html(&body, status, false))
}
pub async fn settings(
    ctx: &mut Ctx,
    name: String,
    rest: Vec<Vec<u8>>,
    post: bool,
) -> Result<Response> {
    ctx.require_account()?;
    if !rest.is_empty() {
        return Err(WebError::NotFound);
    }
    let repo = ctx.repo(&name).await?;
    let permissions = ctx.permissions(&repo, &[]).await;
    if !permissions.is_some_and(|p| p.admin) {
        return Err(WebError::NotFound);
    }
    if !post {
        return show(ctx, repo, None, None, StatusCode::OK).await;
    }
    let fields = ctx.form("settings").await?;
    let version: u64 = fields
        .text("version")
        .parse()
        .map_err(|_| WebError::BadRequest("invalid version".into()))?;
    let section = fields.text("section");
    let update = |config: Option<String>, state: Option<i32>, branch: Option<Vec<u8>>| {
        p::UpdateRepositoryRequest {
            name: repo.name.clone(),
            expected_config_version: version,
            config_json: config,
            state,
            default_branch: branch,
        }
    };
    let current: Value = if repo.record.config_json.is_empty() {
        json!({})
    } else {
        serde_json::from_str(&repo.record.config_json).unwrap_or(json!({}))
    };
    let request = match section.as_str() {
        "general" => update(
            None,
            None,
            Some(fields.text("default_branch").trim().as_bytes().to_vec()),
        ),
        "state" => update(
            None,
            Some(
                fields
                    .text("state")
                    .parse()
                    .map_err(|_| WebError::BadRequest("invalid state".into()))?,
            ),
            None,
        ),
        "config" => update(Some(fields.text("config")), None, None),
        "review" => update(
            Some(review_config(current, &fields).to_string()),
            None,
            None,
        ),
        "webhook_add" | "webhook_remove" => update(
            Some(webhook_config(current, &fields, section == "webhook_add")?.to_string()),
            None,
            None,
        ),
        "rename" => {
            let new_name = fields.text("new_name").trim().to_owned();
            let renamed = ctx
                .upstream()
                .admin()
                .rename_repository(ctx.wreq(p::RenameRepositoryRequest {
                    name: repo.name.clone(),
                    new_name,
                    keep_alias: fields.flag("keep_alias"),
                }))
                .await
                .wrote()?
                .into_inner();
            return Ok(ctx.redirect(&rev::url(&renamed.name, "settings", None, None)));
        }
        "fork" => {
            let forked = ctx
                .upstream()
                .admin()
                .fork_repository(ctx.wreq(p::ForkRepositoryRequest {
                    source: repo.name.clone(),
                    r#ref: fields.text("ref").trim().as_bytes().to_vec(),
                    new_name: fields.text("new_name").trim().to_owned(),
                    idempotency_key: fields.text("key").into_bytes(),
                }))
                .await
                .wrote()?
                .into_inner();
            return Ok(ctx.redirect(&rev::repo_url(&forked.name)));
        }
        "delete" => {
            if fields.text("confirm") != repo.name {
                return show(
                    ctx,
                    repo,
                    Some(ctx.t.s("Type the repository name to confirm").into()),
                    None,
                    StatusCode::BAD_REQUEST,
                )
                .await;
            }
            ctx.upstream()
                .admin()
                .delete_repository(ctx.wreq(p::DeleteRepositoryRequest {
                    name: repo.name.clone(),
                }))
                .await
                .wrote()?;
            tracing::info!(account = ?ctx.viewer.account(), repository = %repo.name, "repository deleted through the web UI");
            return Ok(ctx.redirect("/"));
        }
        _ => return Err(WebError::BadRequest("unknown settings section".into())),
    };
    let submitted = request.config_json.clone();
    match ctx
        .upstream()
        .admin()
        .update_repository(ctx.wreq(request))
        .await
    {
        Ok(_) => {
            tracing::info!(account = ?ctx.viewer.account(), repository = %repo.name, section, "repository settings changed through the web UI");
            let url = repo.url("settings", None, None);
            Ok(ctx.redirect(&url))
        }
        Err(status)
            if matches!(
                status.code(),
                tonic::Code::Aborted
                    | tonic::Code::FailedPrecondition
                    | tonic::Code::InvalidArgument
            ) =>
        {
            // Show the current settings next to what was submitted.
            let repo = ctx.repo(&name).await?;
            let code = match status.code() {
                tonic::Code::InvalidArgument => StatusCode::BAD_REQUEST,
                _ => StatusCode::CONFLICT,
            };
            show(
                ctx,
                repo,
                Some(status.message().to_owned()),
                submitted,
                code,
            )
            .await
        }
        Err(status) => Err(WebError::write(status)),
    }
}
fn review_config(mut config: Value, fields: &Fields) -> Value {
    let patterns = split(&fields.text("patterns"));
    let bypass = split(&fields.text("bypass"));
    if patterns.is_empty() {
        if let Some(object) = config.as_object_mut() {
            object.remove("review");
        }
        return config;
    }
    let mut methods = Vec::new();
    if fields.flag("rebase") {
        methods.push(json!("REBASE"));
    }
    if fields.flag("squash") {
        methods.push(json!("SQUASH"));
    }
    let mut rules = config["review"]["rules"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if rules.is_empty() {
        rules.push(json!({}));
    }
    // Fields the form does not show (owners, trailer, ...) are kept.
    let rule = &mut rules[0];
    rule["ref_patterns"] = Value::Array(patterns);
    rule["required_approvals"] = json!(fields.text("approvals").parse::<u64>().unwrap_or(1));
    rule["allow_self_approval"] = json!(fields.flag("self_approval"));
    rule["block_on_request_changes"] = json!(fields.flag("block"));
    rule["keep_approvals_on_trivial_rebase"] = json!(fields.flag("keep"));
    rule["require_resolved_threads"] = json!(fields.flag("resolved"));
    rule["methods"] = Value::Array(methods);
    let approvers = split(&fields.text("approvers"));
    match rule.as_object_mut() {
        Some(object) if approvers.is_empty() => {
            object.remove("approvers");
        }
        _ => rule["approvers"] = Value::Array(approvers),
    }
    config["review"] = json!({ "rules": rules });
    if !bypass.is_empty() {
        config["review"]["bypass_accounts"] = Value::Array(bypass);
    }
    config
}
fn webhook_config(mut config: Value, fields: &Fields, add: bool) -> Result<Value> {
    let id = fields.text("id").trim().to_owned();
    let mut hooks = config["webhooks"].as_array().cloned().unwrap_or_default();
    hooks.retain(|h| h["id"] != id.as_str());
    if add {
        let mut hook = json!({ "id": id, "url": fields.text("url").trim() });
        let secret = fields.text("secret_file");
        if !secret.trim().is_empty() {
            hook["secret_file"] = json!(secret.trim());
        }
        hooks.push(hook);
    }
    if hooks.is_empty() {
        if let Some(object) = config.as_object_mut() {
            object.remove("webhooks");
        }
    } else {
        config["webhooks"] = Value::Array(hooks);
    }
    Ok(config)
}
