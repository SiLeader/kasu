use crate::Backend;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use kasu_proto::v1::{
    self as p,
    metadata_server::{Metadata, MetadataServer},
};
use std::sync::Arc;
use tonic::{Request, Response, Status};

#[derive(Clone)]
pub struct MetadataService {
    state: Arc<Backend>,
}
impl MetadataService {
    pub fn new(state: Arc<Backend>) -> Self {
        Self { state }
    }
    pub fn into_server(self) -> crate::ObservedService<MetadataServer<Self>> {
        use tower::Layer;
        let layer = crate::ServerLayer::grpc(self.state.clone());
        layer.layer(
            MetadataServer::new(self)
                .max_decoding_message_size(16 * 1024 * 1024)
                .max_encoding_message_size(256 * 1024 * 1024),
        )
    }
}
#[derive(serde::Serialize)]
struct ErrorBody {
    code: i32,
    message: String,
}
pub(crate) struct HttpError(pub(crate) Status);
impl axum::response::IntoResponse for HttpError {
    fn into_response(self) -> axum::response::Response {
        let status = match self.0.code() {
            tonic::Code::InvalidArgument => StatusCode::BAD_REQUEST,
            tonic::Code::NotFound => StatusCode::NOT_FOUND,
            tonic::Code::PermissionDenied => StatusCode::FORBIDDEN,
            tonic::Code::Unauthenticated => StatusCode::UNAUTHORIZED,
            tonic::Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
            tonic::Code::FailedPrecondition => StatusCode::PRECONDITION_FAILED,
            tonic::Code::AlreadyExists | tonic::Code::Aborted => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(ErrorBody {
                code: self.0.code() as i32,
                message: self.0.message().into(),
            }),
        )
            .into_response()
    }
}
async fn server_metrics(
    State(state): State<Arc<Backend>>,
    headers: HeaderMap,
) -> Result<impl axum::response::IntoResponse, HttpError> {
    state
        .authorize(headers.get("authorization").and_then(|v| v.to_str().ok()))
        .map_err(HttpError)?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics(),
    ))
}
macro_rules! endpoints {
    ($(($method:ident,$request:ident,$response:ident,$path:literal)),* $(,)?)=>{
        #[tonic::async_trait]
        impl Metadata for MetadataService {
            $(async fn $method(&self,request:Request<p::$request>)->Result<Response<p::$response>,Status> {
                let authorization=request.metadata().get("authorization").and_then(|v|v.to_str().ok()).map(str::to_owned);let input=request.into_inner();
                let state=self.state.scoped(authorization.as_deref(),&input.repo,crate::Mode::Read).await?;
                let result=crate::admission::blocking(move||state.$method(input)).await.map_err(|_|Status::internal("metadata worker failed"))??;
                Ok(Response::new(result))
            })*
        }
        $(async fn $method(State(state):State<Arc<Backend>>,headers:HeaderMap,Json(input):Json<p::$request>)->Result<Json<p::$response>,HttpError> {
            let state=state.scoped(headers.get("authorization").and_then(|v|v.to_str().ok()),&input.repo,crate::Mode::Read).await.map_err(HttpError)?;
            let result=crate::admission::blocking(move||state.$method(input)).await.map_err(|_|HttpError(Status::internal("metadata worker failed")))?.map_err(HttpError)?;
            Ok(Json(result))
        })*
        pub fn http_router(state:Arc<Backend>)->Router {
            Router::new()$(.route($path,post($method)))*.route("/metrics",get(server_metrics)).layer(axum::extract::DefaultBodyLimit::max(16*1024*1024)).with_state(state.clone()).merge(crate::write_http_router(state.clone())).merge(crate::admin_http_router(state.clone())).merge(crate::git_http_router(state.clone())).layer(crate::ServerLayer::http(state))
        }
    }
}
endpoints! {
    (get_changeset,ChangesetRequest,Changeset,"/v1/changeset"),
    (get_root_tree,ChangesetRequest,RootTreeResponse,"/v1/root-tree"),
    (get_blob_metadata,ObjectRequest,BlobMetadataResponse,"/v1/blob-metadata"),
    (resolve_ref,RefRequest,Ref,"/v1/resolve-ref"),
    (list_refs,ListRefsRequest,ListRefsResponse,"/v1/refs"),
    (log,LogRequest,LogResponse,"/v1/log"),
    (is_ancestor,CompareRequest,AncestorResponse,"/v1/is-ancestor"),
    (merge_base,CompareRequest,MergeBaseResponse,"/v1/merge-base"),
    (diff,DiffRequest,DiffResponse,"/v1/diff"),
    (blame,PathRequest,BlameResponse,"/v1/blame"),
    (read_file,PathRequest,ReadFileResponse,"/v1/read-file"),
    (list_dir,PathRequest,ListDirResponse,"/v1/list-dir"),
    (find_changeset_by_git_oid,GitOidRequest,FindChangesetResponse,"/v1/find-git-oid"),
}
