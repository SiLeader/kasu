use crate::{
    Backend, Mode,
    auth::{Credential, not_found},
};
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use kasu_proto::v1::{
    self as p,
    identity_server::{Identity, IdentityServer},
};
use std::sync::Arc;
use tonic::{Request, Response, Status};

impl std::fmt::Display for crate::RepoPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Legacy => f.write_str("(legacy)"),
            Self::Glob(segments) => f.write_str(&segments.join("/")),
        }
    }
}
impl Backend {
    pub fn who_am_i(&self, authorization: Option<&str>) -> Result<p::Principal, Status> {
        Ok(match self.credential(authorization, false)? {
            Credential::Operator => p::Principal {
                operator: true,
                ..Default::default()
            },
            Credential::Anonymous => p::Principal {
                anonymous: true,
                operator: true,
                ..Default::default()
            },
            credential @ (Credential::Account(_) | Credential::Delegated(_)) => {
                let index = credential.account().expect("an account credential");
                let account = &self.repository_accounts[index];
                let anonymous = credential.anonymous_delegation();
                p::Principal {
                    account: account.name.clone(),
                    anonymous,
                    display_name: account
                        .display_name
                        .clone()
                        .unwrap_or_default()
                        .into_bytes(),
                    email: account.email.clone().unwrap_or_default().into_bytes(),
                    binding: if matches!(credential, Credential::Account(_)) {
                        self.binding(index)
                    } else {
                        Vec::new()
                    },
                    create_repositories: if anonymous {
                        Vec::new()
                    } else {
                        account
                            .create_repositories
                            .iter()
                            .map(ToString::to_string)
                            .collect()
                    },
                    operator: false,
                }
            }
            Credential::Projection(_) | Credential::Hop => {
                return Err(Status::permission_denied(
                    "projection and object credentials have no identity",
                ));
            }
        })
    }
    /// The advisory permissions of the caller on a visible repository.
    pub fn permissions(
        &self,
        authorization: Option<&str>,
        q: p::PermissionsRequest,
    ) -> Result<p::Permissions, Status> {
        if q.refs.len() > 256 {
            return Err(Status::invalid_argument("at most 256 refs"));
        }
        let credential = self.credential(authorization, false)?;
        let mut scoped = self.scope(authorization, &q.repo, Mode::Read)?;
        let record = scoped.record().clone();
        let writable_state = record.state.writable();
        let grant = credential.account().and_then(|account| {
            self.repository_accounts[account]
                .grant_for(&record, self.legacy.as_ref())
                .map(|grant| (account, grant))
        });
        let write = writable_state
            && !credential.anonymous_delegation()
            && match credential {
                Credential::Anonymous | Credential::Operator => true,
                _ => grant.is_some_and(|(a, g)| {
                    self.repository_accounts[a].grant(g).write_policy.is_some()
                }),
            };
        if grant.is_some_and(|(a, g)| self.repository_accounts[a].grant(g).write_policy.is_some()) {
            scoped.writer_grant = grant;
        }
        let bypass = scoped.review_bypass();
        let refs = q
            .refs
            .into_iter()
            .map(|name| {
                let update = write
                    && kasu_git::validate_branch_name(&name).is_ok()
                    && scoped.allows_ref(&name);
                let protected = scoped.review_rule(&name).is_some();
                p::RefPermission {
                    update,
                    create: update && scoped.allows_create(),
                    delete: update && scoped.allows_delete(),
                    force: update && scoped.allows_force(&name),
                    review_required: protected && !bypass,
                    review_bypass: protected && bypass,
                    name,
                }
            })
            .collect();
        let allowed_path_prefixes = scoped.effective_path_prefixes();
        Ok(p::Permissions {
            read_filtered: scoped.read_scope.is_some(),
            read_directories: scoped
                .read_scope
                .as_ref()
                .map(|s| {
                    s.directories
                        .iter()
                        .map(|d| d.as_bytes().to_vec())
                        .collect()
                })
                .unwrap_or_default(),
            write,
            allowed_path_prefixes,
            paths_restricted: !scoped.unrestricted_paths(),
            refs,
            admin: self.administers(credential, &record),
            review: self.reviews.is_some()
                && scoped.read_scope.is_none()
                && !credential.anonymous_delegation(),
        })
    }
}

#[derive(Clone)]
pub struct IdentityService {
    state: Arc<Backend>,
}
impl IdentityService {
    pub fn new(state: Arc<Backend>) -> Self {
        Self { state }
    }
    pub fn into_server(self) -> crate::ObservedService<IdentityServer<Self>> {
        use tower::Layer;
        let layer = crate::ServerLayer::grpc(self.state.clone());
        layer.layer(IdentityServer::new(self).max_decoding_message_size(1024 * 1024))
    }
}
fn authorization<T>(request: &Request<T>) -> Option<String> {
    request
        .metadata()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}
#[tonic::async_trait]
impl Identity for IdentityService {
    async fn who_am_i(
        &self,
        request: Request<p::WhoAmIRequest>,
    ) -> Result<Response<p::Principal>, Status> {
        Ok(Response::new(
            self.state.who_am_i(authorization(&request).as_deref())?,
        ))
    }
    async fn get_permissions(
        &self,
        request: Request<p::PermissionsRequest>,
    ) -> Result<Response<p::Permissions>, Status> {
        let authorization = authorization(&request);
        let input = request.into_inner();
        let state = self.state.clone();
        let result =
            crate::admission::blocking(move || state.permissions(authorization.as_deref(), input))
                .await
                .map_err(|_| Status::internal("identity worker failed"))?;
        // Invisible repositories stay indistinguishable from missing ones.
        Ok(Response::new(result.map_err(|e| {
            if e.code() == tonic::Code::PermissionDenied {
                not_found()
            } else {
                e
            }
        })?))
    }
}
async fn who_am_i(
    State(state): State<Arc<Backend>>,
    headers: HeaderMap,
) -> Result<Json<p::Principal>, crate::transport::HttpError> {
    state
        .who_am_i(headers.get("authorization").and_then(|v| v.to_str().ok()))
        .map(Json)
        .map_err(crate::transport::HttpError)
}
async fn permissions(
    State(state): State<Arc<Backend>>,
    headers: HeaderMap,
    Json(input): Json<p::PermissionsRequest>,
) -> Result<Json<p::Permissions>, crate::transport::HttpError> {
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    crate::admission::blocking(move || state.permissions(authorization.as_deref(), input))
        .await
        .map_err(|_| crate::transport::HttpError(Status::internal("identity worker failed")))?
        .map(Json)
        .map_err(crate::transport::HttpError)
}
pub fn identity_http_router(state: Arc<Backend>) -> Router {
    Router::new()
        .route("/v1/identity/whoami", post(who_am_i))
        .route("/v1/identity/permissions", post(permissions))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}
