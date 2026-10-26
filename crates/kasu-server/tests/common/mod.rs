#![allow(dead_code)]
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use kasu_core::*;
use kasu_proto::v1 as p;
use kasu_server::*;
use kasu_store::*;
use std::sync::Arc;
use tower::ServiceExt;

pub struct Fixture {
    pub _dir: tempfile::TempDir,
    pub store: Arc<SqliteStore>,
    pub blobs: Arc<FsBlobStore>,
    pub state: Arc<Backend>,
}
pub fn name(value: &str) -> RepoName {
    RepoName::new(value).unwrap()
}
impl Fixture {
    pub fn new(repositories: &[&str]) -> Self {
        Self::with(repositories, |backend| backend)
    }
    pub fn with(repositories: &[&str], configure: impl FnOnce(Backend) -> Backend) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteStore::memory().unwrap());
        let blobs = Arc::new(FsBlobStore::open(dir.path()).unwrap());
        for repository in repositories {
            let record =
                RepositoryRecord::new(RepoKey::generate().unwrap(), name(repository), b"test", 1);
            assert!(matches!(
                store.create_repository(&record).unwrap(),
                RepositoryWrite::Applied(_)
            ));
        }
        let backend = Backend::with_registry(
            store.clone(),
            blobs.clone(),
            store.clone(),
            store.clone(),
            Some("operator".into()),
        )
        .with_registry_ttl(std::time::Duration::from_millis(1));
        Self {
            _dir: dir,
            store,
            blobs,
            state: Arc::new(configure(backend)),
        }
    }
    pub fn record(&self, repository: &str) -> RepositoryRecord {
        let entry = self
            .store
            .resolve_repository_name(&name(repository), 0)
            .unwrap()
            .unwrap();
        self.store.repository(&entry.key).unwrap().unwrap()
    }
    pub fn update(&self, repository: &str, change: impl FnOnce(&mut RepositoryRecord)) {
        let mut record = self.record(repository);
        change(&mut record);
        assert!(matches!(
            self.store
                .update_repository(&record, "test", b"test", 2)
                .unwrap(),
            RepositoryWrite::Applied(_)
        ));
    }
    pub async fn call(
        &self,
        token: Option<&str>,
        path: &str,
        body: &impl serde::Serialize,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::post(path).header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = http_router(self.state.clone())
            .oneshot(
                request
                    .body(Body::from(serde_json::to_vec(body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
        )
    }
    pub async fn get(&self, token: Option<&str>, path: &str) -> (StatusCode, Vec<u8>) {
        let mut request = Request::get(path);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = http_router(self.state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        (
            status,
            to_bytes(response.into_body(), 64 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
    }
    /// Upload `files`, create a changeset on `parent` and move refs/heads/main.
    pub async fn commit(
        &self,
        token: &str,
        repository: &str,
        parent: Option<Vec<u8>>,
        files: &[(&[u8], &[u8])],
    ) -> Vec<u8> {
        let repo = repository.as_bytes().to_vec();
        let chunks = files
            .iter()
            .map(|(_, content)| p::BlobUpload {
                repo: repo.clone(),
                id: ObjectId::hash(content).0.to_vec(),
                offset: 0,
                data: content.to_vec(),
                last: true,
            })
            .collect();
        let (status, _) = self
            .call(
                Some(token),
                "/v1/upload-blobs",
                &p::UploadBlobsHttpRequest { chunks },
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let signature = p::Signature {
            name: b"A".to_vec(),
            email: b"a@example.com".to_vec(),
            timestamp: 1,
            tz: b"+0000".to_vec(),
        };
        let (status, changeset) = self
            .call(
                Some(token),
                "/v1/create-changeset",
                &p::CreateChangesetRequest {
                    repo: repo.clone(),
                    parents: parent.iter().cloned().collect(),
                    changes: files
                        .iter()
                        .map(|(path, content)| p::FileChange {
                            path: path.to_vec(),
                            entry: Some(p::Entry {
                                kind: Some(p::entry::Kind::File(p::File {
                                    blob: ObjectId::hash(content).0.to_vec(),
                                    executable: false,
                                })),
                            }),
                        })
                        .collect(),
                    metadata: Some(p::CommitMetadata {
                        author: Some(signature.clone()),
                        committer: Some(signature),
                        message: b"change".to_vec(),
                    }),
                },
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{changeset}");
        let id = bytes(&changeset["id"]);
        let (_, current) = self
            .call(
                Some(token),
                "/v1/resolve-ref",
                &p::RefRequest {
                    repo: repo.clone(),
                    name: b"refs/heads/main".to_vec(),
                },
            )
            .await;
        let (status, updated) = self
            .call(
                Some(token),
                "/v1/update-ref",
                &p::UpdateRefRequest {
                    repo,
                    name: b"refs/heads/main".to_vec(),
                    expected: Some(p::Ref {
                        name: b"refs/heads/main".to_vec(),
                        target: parent,
                        version: current["version"].as_u64().unwrap_or(0),
                    }),
                    target: Some(id.clone()),
                    force: false,
                },
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{updated}");
        assert_eq!(updated["applied"], true);
        id
    }
}
pub fn bytes(value: &serde_json::Value) -> Vec<u8> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
        .collect()
}
pub fn advertisement(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}
