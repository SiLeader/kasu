use crate::{Backend, backend::status};
use kasu_core::{BlobId, ObjectId, TreeId};
use kasu_proto::v1::{
    self as p,
    objects_server::{Objects, ObjectsServer},
};
use std::{collections::BTreeSet, sync::Arc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

pub const OBJECT_CHUNK_BYTES: usize = 64 * 1024;
pub const OBJECT_BATCH_LIMIT: usize = 128;
const MAX_COMPRESSED_BYTES: usize = kasu_store::MAX_COMPRESSED_OBJECT_SIZE;

#[derive(Clone)]
pub struct ObjectService {
    state: Arc<Backend>,
}
impl ObjectService {
    pub fn new(state: Arc<Backend>) -> Self {
        Self { state }
    }
    pub fn into_server(self) -> crate::ObservedService<ObjectsServer<Self>> {
        use tower::Layer;
        let layer = crate::ServerLayer::grpc(self.state.clone());
        layer.layer(
            ObjectsServer::new(self)
                .max_decoding_message_size(16 * 1024)
                .max_encoding_message_size(OBJECT_CHUNK_BYTES + 1024),
        )
    }
    async fn stream<T: Send + 'static>(
        &self,
        request: Request<p::ObjectRequest>,
        tree: bool,
        encode: fn(Vec<u8>, u64, Vec<u8>, bool) -> T,
    ) -> Result<ReceiverStream<Result<T, Status>>, Status> {
        let authorization = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok());
        let scoped = self.state.object_reader(authorization)?;
        let input = request.into_inner();
        self.state.repo(&input.repo)?;
        if input.ids.len() > OBJECT_BATCH_LIMIT {
            return Err(Status::resource_exhausted("object batch exceeds 128 IDs"));
        }
        let ids = input
            .ids
            .iter()
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map(ObjectId)
                    .map_err(|_| Status::invalid_argument("object ID must be 32 bytes"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if ids.iter().copied().collect::<BTreeSet<_>>().len() != ids.len() {
            return Err(Status::invalid_argument("duplicate object ID"));
        }
        let state = scoped.clone();
        let ids = crate::admission::blocking(move || {
            let ops = state.ops();
            for id in &ids {
                if tree {
                    ops.ctx
                        .repo
                        .require_tree_membership(state.object_namespace(), TreeId(id.0))
                } else {
                    ops.ctx
                        .repo
                        .require_blob_membership(state.object_namespace(), BlobId(id.0))
                }
                .map_err(status)?;
            }
            Ok::<_, Status>(ids)
        })
        .await
        .map_err(|_| Status::internal("object authorization worker failed"))??;
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let state = scoped.clone();
        crate::admission::blocking(move || {
            for id in ids {
                if tx.is_closed() {
                    return;
                }
                let bytes = match state.ops().ctx.repo.blobs.get_compressed(id) {
                    Ok(bytes) if bytes.is_empty() => {
                        let _ = tx.blocking_send(Err(Status::data_loss("empty compressed object")));
                        return;
                    }
                    Ok(bytes) if bytes.len() <= MAX_COMPRESSED_BYTES => bytes,
                    Ok(_) => {
                        let _ = tx.blocking_send(Err(Status::resource_exhausted(
                            "compressed object exceeds limit",
                        )));
                        return;
                    }
                    Err(error) => {
                        let _ = tx.blocking_send(Err(status(error)));
                        return;
                    }
                };
                for (index, chunk) in bytes.chunks(OBJECT_CHUNK_BYTES).enumerate() {
                    let offset = index * OBJECT_CHUNK_BYTES;
                    let value = encode(
                        id.0.to_vec(),
                        offset as u64,
                        chunk.to_vec(),
                        offset + chunk.len() == bytes.len(),
                    );
                    if tx.blocking_send(Ok(value)).is_err() {
                        return;
                    }
                }
            }
        });
        Ok(ReceiverStream::new(rx))
    }
}
#[tonic::async_trait]
impl Objects for ObjectService {
    type GetTreesStream = ReceiverStream<Result<p::TreeObject, Status>>;
    type GetBlobsStream = ReceiverStream<Result<p::BlobChunk, Status>>;
    async fn get_trees(
        &self,
        request: Request<p::ObjectRequest>,
    ) -> Result<Response<Self::GetTreesStream>, Status> {
        let mut response = Response::new(
            self.stream(request, true, |id, offset, data, last| p::TreeObject {
                id,
                offset,
                data,
                last,
            })
            .await?,
        );
        response
            .metadata_mut()
            .insert(kasu_object::LAYER_HEADER, "server".parse().unwrap());
        Ok(response)
    }
    async fn get_blobs(
        &self,
        request: Request<p::ObjectRequest>,
    ) -> Result<Response<Self::GetBlobsStream>, Status> {
        let mut response = Response::new(
            self.stream(request, false, |id, offset, data, last| p::BlobChunk {
                id,
                offset,
                data,
                last,
            })
            .await?,
        );
        response
            .metadata_mut()
            .insert(kasu_object::LAYER_HEADER, "server".parse().unwrap());
        Ok(response)
    }
}
