use crate::{Backend, write::UploadBuffer};
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use kasu_proto::v1::{
    self as p,
    writer_server::{Writer, WriterServer},
};
use std::sync::Arc;
use tonic::{Request, Response, Status};

#[derive(Clone)]
pub struct WriteService {
    state: Arc<Backend>,
}
impl WriteService {
    pub fn new(state: Arc<Backend>) -> Self {
        Self { state }
    }
    pub fn into_server(self) -> crate::ObservedService<WriterServer<Self>> {
        use tower::Layer;
        let layer = crate::ServerLayer::grpc(self.state.clone());
        layer.layer(
            WriterServer::new(self)
                .max_decoding_message_size(16 * 1024 * 1024)
                .max_encoding_message_size(256 * 1024 * 1024),
        )
    }
}
macro_rules! endpoints {
    ($(($method:ident,$request:ident,$response:ident,$path:literal)),* $(,)?) => {
        #[tonic::async_trait]
        impl Writer for WriteService {
            async fn land(&self, request: Request<p::LandRequest>) -> Result<Response<p::LandResponse>, Status> {
                let state = self.state.writer(request.metadata().get("authorization").and_then(|v| v.to_str().ok()))?;
                Ok(Response::new(state.land(request.into_inner()).await?))
            }
            async fn upload_blobs(&self, request: Request<tonic::Streaming<p::BlobUpload>>) -> Result<Response<p::UploadBlobsResponse>, Status> {
                let state = self.state.writer(request.metadata().get("authorization").and_then(|v| v.to_str().ok()))?;
                let mut stream = request.into_inner();
                let mut buffer = UploadBuffer::default();
                while let Some(chunk) = stream.message().await? {
                    let state = state.clone();
                    buffer = crate::admission::blocking(move || {
                        buffer.push(&state, chunk)?;
                        Ok::<_, Status>(buffer)
                    }).await.map_err(|_| Status::internal("upload worker failed"))??;
                }
                Ok(Response::new(buffer.finish()?))
            }
            $(async fn $method(&self, request: Request<p::$request>) -> Result<Response<p::$response>, Status> {
                let state = self.state.writer(request.metadata().get("authorization").and_then(|v| v.to_str().ok()))?;
                let input = request.into_inner();
                let result = crate::admission::blocking(move || state.$method(input)).await.map_err(|_| Status::internal("write worker failed"))??;
                Ok(Response::new(result))
            })*
        }
        $(async fn $method(State(state): State<Arc<Backend>>, headers: HeaderMap, Json(input): Json<p::$request>) -> Result<Json<p::$response>, crate::transport::HttpError> {
            let state = state.writer(headers.get("authorization").and_then(|v| v.to_str().ok())).map_err(crate::transport::HttpError)?;
            let result = crate::admission::blocking(move || state.$method(input)).await.map_err(|_| crate::transport::HttpError(Status::internal("write worker failed")))?.map_err(crate::transport::HttpError)?;
            Ok(Json(result))
        })*
        pub fn write_http_router(state: Arc<Backend>) -> Router {
            Router::new()$(.route($path, post($method)))*.route("/v1/upload-blobs",post(upload_blobs)).route("/v1/land",post(land)).route("/v1/guarded-land",post(guarded_land))
                .layer(axum::extract::DefaultBodyLimit::max(16*1024*1024)).with_state(state)
        }
    }
}
async fn guarded_land(
    State(state): State<Arc<Backend>>,
    headers: HeaderMap,
    Json(input): Json<p::LandRequest>,
) -> Result<Json<p::LandResponse>, crate::transport::HttpError> {
    let state = state
        .writer(headers.get("authorization").and_then(|v| v.to_str().ok()))
        .map_err(crate::transport::HttpError)?;
    if input.required_ancestor.len() != 32 {
        return Err(crate::transport::HttpError(Status::invalid_argument(
            "guarded Land requires a 32-byte ancestor",
        )));
    }
    Ok(Json(
        state
            .land(input)
            .await
            .map_err(crate::transport::HttpError)?,
    ))
}
async fn land(
    State(state): State<Arc<Backend>>,
    headers: HeaderMap,
    Json(input): Json<p::LandRequest>,
) -> Result<Json<p::LandResponse>, crate::transport::HttpError> {
    let state = state
        .writer(headers.get("authorization").and_then(|v| v.to_str().ok()))
        .map_err(crate::transport::HttpError)?;
    Ok(Json(
        state
            .land(input)
            .await
            .map_err(crate::transport::HttpError)?,
    ))
}
endpoints! {
    (create_changeset,CreateChangesetRequest,Changeset,"/v1/create-changeset"),
    (merge,MergeRequest,RewriteResponse,"/v1/merge"),
    (rebase,RebaseRequest,RewriteResponse,"/v1/rebase"),
    (update_ref,UpdateRefRequest,UpdateRefResponse,"/v1/update-ref"),
}
async fn upload_blobs(
    State(state): State<Arc<Backend>>,
    headers: HeaderMap,
    Json(input): Json<p::UploadBlobsHttpRequest>,
) -> Result<Json<p::UploadBlobsResponse>, crate::transport::HttpError> {
    let state = state
        .writer(headers.get("authorization").and_then(|v| v.to_str().ok()))
        .map_err(crate::transport::HttpError)?;
    let result = crate::admission::blocking(move || {
        let mut buffer = UploadBuffer::default();
        for chunk in input.chunks {
            buffer.push(&state, chunk)?;
        }
        buffer.finish()
    })
    .await
    .map_err(|_| crate::transport::HttpError(Status::internal("upload worker failed")))?
    .map_err(crate::transport::HttpError)?;
    Ok(Json(result))
}
