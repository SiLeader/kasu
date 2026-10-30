use crate::{Result, SparseProfile, invalid, validate_work_path};
use kasu_core::{Canonical, ChangesetId, Entry, ObjectId, Tree, TreeEntry, TreeId};
use kasu_proto::v1::{
    self as p, admin_client::AdminClient, metadata_client::MetadataClient,
    writer_client::WriterClient,
};
use kasu_store::{BlobStore, FsBlobStore};
use std::collections::BTreeMap;
use tonic::{
    Request,
    metadata::{Ascii, MetadataValue},
    transport::Channel,
};

pub type Snapshot = BTreeMap<Vec<u8>, Entry>;
pub struct Remote {
    pub metadata: MetadataClient<Channel>,
    pub writer: WriterClient<Channel>,
    objects: kasu_object::Client,
    pub repo: Vec<u8>,
    authorization: Option<MetadataValue<Ascii>>,
}
fn endpoint(url: &str) -> Result<tonic::transport::Endpoint> {
    let endpoint = tonic::transport::Endpoint::from_shared(url.to_owned())?;
    if !matches!(endpoint.uri().scheme_str(), Some("http" | "https"))
        || endpoint
            .uri()
            .authority()
            .is_none_or(|a| a.as_str().contains('@'))
    {
        return Err(invalid(
            "endpoint must be an HTTP(S) URL without credentials",
        ));
    }
    let endpoint = if endpoint.uri().scheme_str() == Some("https") {
        endpoint.tls_config(tonic::transport::ClientTlsConfig::new().with_webpki_roots())?
    } else {
        endpoint
    };
    Ok(endpoint
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30)))
}
/// Reviews on one server (kasu.v1.Review).
pub struct Reviews {
    pub client: kasu_proto::v1::review_client::ReviewClient<Channel>,
    authorization: Option<MetadataValue<Ascii>>,
}
impl Reviews {
    pub async fn connect(server: &str, token: Option<&str>) -> Result<Self> {
        Ok(Self {
            client: kasu_proto::v1::review_client::ReviewClient::new(
                endpoint(server)?.connect().await?,
            ),
            authorization: token.map(kasu_object::bearer).transpose()?,
        })
    }
    pub fn request<T>(&self, value: T) -> Request<T> {
        let mut request = Request::new(value);
        if let Some(value) = &self.authorization {
            request
                .metadata_mut()
                .insert("authorization", value.clone());
        }
        kasu_telemetry::inject(request.metadata_mut());
        request
    }
}
/// Repository administration on one server (kasu.v1.Admin).
pub struct Admin {
    pub client: AdminClient<Channel>,
    authorization: Option<MetadataValue<Ascii>>,
}
impl Admin {
    pub async fn connect(server: &str, token: Option<&str>) -> Result<Self> {
        Ok(Self {
            client: AdminClient::new(endpoint(server)?.connect().await?),
            authorization: token.map(kasu_object::bearer).transpose()?,
        })
    }
    pub fn request<T>(&self, value: T) -> Request<T> {
        let mut request = Request::new(value);
        if let Some(value) = &self.authorization {
            request
                .metadata_mut()
                .insert("authorization", value.clone());
        }
        kasu_telemetry::inject(request.metadata_mut());
        request
    }
    /// The registered default branch, for servers that expose it to the caller.
    pub async fn default_branch(&mut self, repo: &str) -> Result<Vec<u8>> {
        let q = self.request(p::GetRepositoryRequest { name: repo.into() });
        Ok(self
            .client
            .get_repository(q)
            .await?
            .into_inner()
            .default_branch)
    }
}
impl Remote {
    pub async fn connect(
        server: &str,
        objects: &str,
        repo: Vec<u8>,
        token: Option<&str>,
    ) -> Result<Self> {
        let channel = endpoint(server)?.connect().await?;
        let object_channel = if objects == server {
            channel.clone()
        } else {
            kasu_object::connect(objects).await?
        };
        Ok(Self {
            metadata: MetadataClient::new(channel.clone())
                .max_decoding_message_size(256 * 1024 * 1024),
            writer: WriterClient::new(channel).max_decoding_message_size(256 * 1024 * 1024),
            objects: kasu_object::Client::new(
                object_channel,
                token,
                if objects.starts_with("unix://") {
                    Some(kasu_object::Layer::Daemon)
                } else {
                    None
                },
            )?,
            repo,
            authorization: token.map(kasu_object::bearer).transpose()?,
        })
    }
    pub fn request<T>(&self, value: T) -> Request<T> {
        let mut request = Request::new(value);
        if let Some(value) = &self.authorization {
            request
                .metadata_mut()
                .insert("authorization", value.clone());
        }
        kasu_telemetry::inject(request.metadata_mut());
        request
    }
    pub async fn resolve(&mut self, name: &[u8]) -> Result<(ChangesetId, Option<Vec<u8>>)> {
        if let Ok(text) = std::str::from_utf8(name)
            && let Ok(id) = text.parse::<ChangesetId>()
        {
            let q = self.request(p::ChangesetRequest {
                repo: self.repo.clone(),
                id: id.0.to_vec(),
            });
            self.metadata.get_changeset(q).await?;
            return Ok((id, None));
        }
        let name = if name.starts_with(b"refs/") {
            name.to_vec()
        } else {
            [b"refs/heads/".as_slice(), name].concat()
        };
        let q = self.request(p::RefRequest {
            repo: self.repo.clone(),
            name: name.clone(),
        });
        let value = self.metadata.resolve_ref(q).await?.into_inner();
        let id = ChangesetId(
            value
                .target
                .ok_or_else(|| invalid("ref has no target"))?
                .try_into()
                .map_err(|_| invalid("invalid ref target"))?,
        );
        Ok((id, Some(name)))
    }
    pub async fn fetch(
        &mut self,
        cache: &FsBlobStore,
        id: ObjectId,
        tree: bool,
    ) -> Result<Vec<u8>> {
        match cache.get(id) {
            Ok(raw) => return Ok(raw),
            Err(kasu_core::Error::NotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
        let bytes = self
            .objects
            .fetch(
                &self.repo,
                id,
                if tree {
                    kasu_object::Kind::Tree
                } else {
                    kasu_object::Kind::Blob
                },
            )
            .await?;
        cache.put_compressed(id, &bytes)?;
        Ok(cache.get(id)?)
    }
    pub async fn snapshot(
        &mut self,
        cache: &FsBlobStore,
        id: ChangesetId,
        sparse: &SparseProfile,
    ) -> Result<Snapshot> {
        self.snapshot_mode(cache, id, sparse, true).await
    }
    pub async fn metadata_snapshot(
        &mut self,
        cache: &FsBlobStore,
        id: ChangesetId,
        sparse: &SparseProfile,
    ) -> Result<Snapshot> {
        self.snapshot_mode(cache, id, sparse, false).await
    }
    pub async fn blob_sizes(
        &mut self,
        ids: &[kasu_core::BlobId],
    ) -> Result<BTreeMap<kasu_core::BlobId, u64>> {
        let mut result = BTreeMap::new();
        for batch in ids.chunks(128) {
            let q = self.request(p::ObjectRequest {
                repo: self.repo.clone(),
                ids: batch.iter().map(|id| id.0.to_vec()).collect(),
            });
            let records = self.metadata.get_blob_metadata(q).await?.into_inner().blobs;
            if records.len() != batch.len() {
                return Err(invalid("incomplete blob metadata batch"));
            }
            for (expected, record) in batch.iter().zip(records) {
                if record.id != expected.0 || record.size > kasu_store::MAX_OBJECT_SIZE {
                    return Err(invalid("invalid blob metadata response"));
                }
                result.insert(*expected, record.size);
            }
        }
        Ok(result)
    }
    pub async fn fetch_blob_bytes(&mut self, id: kasu_core::BlobId) -> Result<Vec<u8>> {
        let frame = self
            .objects
            .fetch(&self.repo, ObjectId(id.0), kasu_object::Kind::Blob)
            .await?;
        kasu_object::validate(kasu_object::Kind::Blob, ObjectId(id.0), &frame)?;
        Ok(kasu_store::decompress(&frame)?)
    }
    async fn snapshot_mode(
        &mut self,
        cache: &FsBlobStore,
        id: ChangesetId,
        sparse: &SparseProfile,
        hydrate: bool,
    ) -> Result<Snapshot> {
        sparse.validate()?;
        let q = self.request(p::ChangesetRequest {
            repo: self.repo.clone(),
            id: id.0.to_vec(),
        });
        let root = TreeId(
            self.metadata
                .get_root_tree(q)
                .await?
                .into_inner()
                .id
                .try_into()
                .map_err(|_| invalid("invalid root tree ID"))?,
        );
        let mut stack = vec![(vec![], root)];
        let mut result = BTreeMap::new();
        while let Some((path, id)) = stack.pop() {
            if !sparse.enters(&path) {
                continue;
            }
            let tree = Tree::decode(&self.fetch(cache, ObjectId(id.0), true).await?)?;
            for (name, value) in tree.entries {
                let child = if path.is_empty() {
                    name.to_vec()
                } else {
                    [path.as_slice(), b"/", name.as_slice()].concat()
                };
                match value {
                    TreeEntry::SubTree(id) => {
                        if sparse.enters(&child) {
                            validate_work_path(&child)?;
                            stack.push((child, id));
                        }
                    }
                    TreeEntry::Leaf(entry) => {
                        if sparse.includes(&child) {
                            validate_work_path(&child)?;
                            if hydrate && let Some(blob) = entry.blob() {
                                self.fetch(cache, ObjectId(blob.0), false).await?;
                            }
                            result.insert(child, entry);
                        }
                    }
                }
            }
        }
        Ok(result)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use kasu_object::Frame;
    #[test]
    fn frames_reject_truncation_wrong_ids_offsets_and_data_after_completion() {
        let id = ObjectId([1; 32]);
        let mut frame = Frame::new(id);
        frame.push(&id.0, 0, b"x", false).unwrap();
        assert!(frame.finish().is_err());
        for (raw, offset, data) in [
            (vec![2; 32], 0, vec![1]),
            (id.0.to_vec(), 1, vec![1]),
            (id.0.to_vec(), 0, vec![]),
            (id.0.to_vec(), 0, vec![0; 65537]),
        ] {
            assert!(Frame::new(id).push(&raw, offset, &data, true).is_err());
        }
        let mut frame = Frame::new(id);
        frame.push(&id.0, 0, b"x", true).unwrap();
        assert!(frame.push(&id.0, 1, b"y", true).is_err());
    }
}
