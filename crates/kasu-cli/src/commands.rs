use crate::{Remote, Result, Snapshot, WorkingCopy, invalid};
use kasu_core::{Canonical, ChangesetId, Entry, ObjectId};
use kasu_proto::v1 as p;
use kasu_store::BlobStore;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
};

pub fn wire_entry(entry: &Entry) -> p::Entry {
    p::Entry {
        kind: Some(match entry {
            Entry::File { blob, executable } => p::entry::Kind::File(p::File {
                blob: blob.0.to_vec(),
                executable: *executable,
            }),
            Entry::Symlink { blob } => p::entry::Kind::Symlink(blob.0.to_vec()),
            Entry::Gitlink { commit } => p::entry::Kind::Gitlink(commit.0.to_vec()),
        }),
    }
}
impl WorkingCopy {
    fn require_resolved_land(&self) -> Result<()> {
        if self.state.land_key.is_some()
            && self
                .state
                .land_outcome
                .is_none_or(|outcome| outcome == p::LandOutcome::Landed as i32)
        {
            return Err(invalid(
                "pending Land must be replayed before commit/rebase; preserve local edits and retry land",
            ));
        }
        Ok(())
    }
    pub async fn commit(
        &mut self,
        remote: &mut Remote,
        message: Vec<u8>,
        signature: p::Signature,
    ) -> Result<ChangesetId> {
        self.require_resolved_land()?;
        let scanned = self.scan()?;
        let current: Snapshot = scanned
            .iter()
            .map(|(path, (entry, _))| (path.clone(), entry.clone()))
            .collect();
        let old = self.state.snapshot()?;
        let mut changes = BTreeMap::new();
        let mut anchor_entries = BTreeMap::new();
        let mut parent = self.state.base.clone();
        if self.state.draft {
            let q = remote.request(p::ChangesetRequest {
                repo: remote.repo.clone(),
                id: self.state.base.clone(),
            });
            let cs = remote.metadata.get_changeset(q).await?.into_inner();
            parent = cs
                .parents
                .first()
                .ok_or_else(|| invalid("draft has no parent"))?
                .clone();
            let q = remote.request(p::DiffRequest {
                repo: remote.repo.clone(),
                base: parent.clone(),
                head: self.state.base.clone(),
                paths: vec![],
                content: false,
            });
            for file in remote.metadata.diff(q).await?.into_inner().files {
                anchor_entries.insert(file.path.clone(), file.before);
                changes.insert(
                    file.path.clone(),
                    p::FileChange {
                        path: file.path,
                        entry: file.after,
                    },
                );
            }
        }
        let mut blobs = BTreeSet::new();
        let mut dirty = false;
        for path in old
            .keys()
            .chain(current.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
        {
            if old.get(&path) == current.get(&path) {
                continue;
            }
            dirty = true;
            if let Some(blob) = current.get(&path).and_then(Entry::blob) {
                blobs.insert(blob);
            }
            let anchor = anchor_entries
                .get(&path)
                .cloned()
                .unwrap_or_else(|| old.get(&path).map(wire_entry));
            let entry = current.get(&path).map(wire_entry);
            if anchor == entry {
                changes.remove(&path);
            } else {
                changes.insert(path.clone(), p::FileChange { path, entry });
            }
        }
        if !dirty {
            return Err(invalid("no local changes to commit"));
        }
        for blob in blobs {
            let probe = p::BlobUpload {
                repo: remote.repo.clone(),
                id: blob.0.to_vec(),
                offset: 0,
                data: vec![],
                last: true,
            };
            let q = remote.request(tokio_stream::iter([probe]));
            match remote.writer.upload_blobs(q).await {
                Ok(_) => continue,
                Err(error) if error.code() == tonic::Code::InvalidArgument => {}
                Err(error) => return Err(error.into()),
            }
            let bytes = self.cache.get(ObjectId(blob.0))?;
            let repo = remote.repo.clone();
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            let producer = tokio::spawn(async move {
                for (index, data) in bytes.chunks(64 * 1024).enumerate() {
                    let offset = index * 64 * 1024;
                    if tx
                        .send(p::BlobUpload {
                            repo: repo.clone(),
                            id: blob.0.to_vec(),
                            offset: offset as u64,
                            data: data.to_vec(),
                            last: offset + data.len() == bytes.len(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let q = remote.request(tokio_stream::wrappers::ReceiverStream::new(rx));
            let response = remote.writer.upload_blobs(q).await;
            producer.abort();
            let _ = producer.await;
            response?;
        }
        let q = remote.request(p::CreateChangesetRequest {
            repo: remote.repo.clone(),
            parents: vec![parent],
            changes: changes.into_values().collect(),
            metadata: Some(p::CommitMetadata {
                author: Some(signature.clone()),
                committer: Some(signature),
                message,
            }),
        });
        let cs = remote.writer.create_changeset(q).await?.into_inner();
        let id = ChangesetId(
            cs.id
                .as_slice()
                .try_into()
                .map_err(|_| invalid("invalid changeset ID"))?,
        );
        let now = self
            .scan()?
            .into_iter()
            .map(|(path, (entry, _))| (path, entry))
            .collect::<Snapshot>();
        if now != current {
            return Err(invalid(
                "files changed while committing; server changeset exists but working copy base was preserved",
            ));
        }
        self.state.base = cs.id.clone();
        self.state.draft = true;
        self.state.land_key = None;
        self.state.land_outcome = None;
        self.record_scanned(scanned)?;
        Ok(id)
    }
    pub async fn rebase(
        &mut self,
        remote: &mut Remote,
        onto: &[u8],
        signature: Option<p::Signature>,
    ) -> Result<Option<p::RewriteResponse>> {
        self.require_resolved_land()?;
        self.require_clean()?;
        let (onto, reference) = remote.resolve(onto).await?;
        let mut state = self.state.clone();
        state.reference = reference;
        state.land_key = None;
        state.land_outcome = None;
        let mut response = None;
        if state.draft {
            let q = remote.request(p::RebaseRequest {
                repo: remote.repo.clone(),
                id: state.base.clone(),
                onto: onto.0.to_vec(),
                committer: Some(
                    signature.ok_or_else(|| invalid("committer identity is required"))?,
                ),
            });
            let result = remote.writer.rebase(q).await?.into_inner();
            let Some(id) = &result.id else {
                self.conflict(&result)?;
                return Ok(Some(result));
            };
            state.base = id.clone();
            response = Some(result);
        } else {
            state.base = onto.0.to_vec();
        }
        let snapshot = remote
            .snapshot(&self.cache, state.id()?, &state.sparse)
            .await?;
        self.transition(snapshot, state)?;
        Ok(response)
    }
    pub async fn land(
        &mut self,
        remote: &mut Remote,
        reference: Option<&[u8]>,
    ) -> Result<p::LandResponse> {
        self.require_clean()?;
        if !self.state.draft {
            return Err(invalid("no draft changeset to land"));
        }
        let name = reference
            .map(|value| {
                if value.starts_with(b"refs/") {
                    value.to_vec()
                } else {
                    [b"refs/heads/".as_slice(), value].concat()
                }
            })
            .unwrap_or_else(|| self.state.reference.clone());
        if self.state.land_key.is_some() && self.state.reference != name {
            return Err(invalid(
                "pending Land targets another ref; retry that request first",
            ));
        }
        if self.state.land_key.is_none() {
            let mut key = vec![0; 32];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
            self.state.land_key = Some(key);
            self.state.land_outcome = None;
            self.state.reference = name.clone();
            self.save()?;
        }
        let q = remote.request(p::LandRequest {
            required_ancestor: Vec::new(),
            repo: remote.repo.clone(),
            id: self.state.base.clone(),
            name,
            idempotency_key: self.state.land_key.clone().unwrap(),
            max_attempts: 8,
            timeout_ms: 30_000,
        });
        let result = remote.writer.land(q).await?.into_inner();
        p::LandOutcome::try_from(result.outcome)?;
        self.state.land_outcome = Some(result.outcome);
        self.save()?;
        if result.outcome == p::LandOutcome::Landed as i32 {
            let mut state = self.state.clone();
            state.base = result
                .id
                .clone()
                .ok_or_else(|| invalid("Landed response has no changeset"))?;
            state.draft = false;
            state.land_key = None;
            state.land_outcome = None;
            let snapshot = remote
                .snapshot(&self.cache, state.id()?, &state.sparse)
                .await?;
            self.transition(snapshot, state)?;
        } else if let Some(conflict) = &result.conflict {
            self.conflict(conflict)?;
        }
        Ok(result)
    }
    pub async fn sparse(
        &mut self,
        remote: &mut Remote,
        path: Vec<u8>,
        include: bool,
    ) -> Result<()> {
        self.require_clean()?;
        let mut state = self.state.clone();
        state.sparse.set(path, include)?;
        let snapshot = remote
            .snapshot(&self.cache, state.id()?, &state.sparse)
            .await?;
        self.transition(snapshot, state)
    }
    pub async fn hydrate_diff(&self, token: Option<&str>) -> Result<()> {
        let mut missing = std::collections::BTreeSet::new();
        for change in self.changes()? {
            if let Some(raw) = change.before
                && let Some(blob) = Entry::decode(&raw)?.blob()
            {
                match self.cache.get(ObjectId(blob.0)) {
                    Ok(_) => {}
                    Err(kasu_core::Error::NotFound(_)) => {
                        missing.insert(blob);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        if !missing.is_empty() {
            let mut remote = self.remote(token).await?;
            for blob in missing {
                remote.fetch(&self.cache, ObjectId(blob.0), false).await?;
            }
        }
        Ok(())
    }
    pub fn diff_text(&self) -> Result<String> {
        use gix_imara_diff::{
            Algorithm, BasicLineDiffPrinter, Diff, InternedInput, UnifiedDiffConfig,
        };
        let mut result = String::new();
        for change in self.changes()? {
            let path = String::from_utf8(change.path.escape_ascii().collect())?;
            result.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
            let content = |raw: &Option<Vec<u8>>| -> Result<Vec<u8>> {
                Ok(match raw {
                    Some(raw) => match Entry::decode(raw)? {
                        Entry::File { blob, .. } | Entry::Symlink { blob } => {
                            self.cache.get(ObjectId(blob.0))?
                        }
                        Entry::Gitlink { commit } => format!("gitlink {commit}\n").into_bytes(),
                    },
                    None => vec![],
                })
            };
            let before = content(&change.before)?;
            let after = content(&change.after)?;
            match (std::str::from_utf8(&before), std::str::from_utf8(&after)) {
                (Ok(before), Ok(after)) if !before.contains('\0') && !after.contains('\0') => {
                    let input = InternedInput::new(before, after);
                    let mut diff = Diff::compute(Algorithm::Histogram, &input);
                    diff.postprocess_lines(&input);
                    result.push_str(
                        &diff
                            .unified_diff(
                                &BasicLineDiffPrinter(&input.interner),
                                UnifiedDiffConfig::default(),
                                &input,
                            )
                            .to_string(),
                    );
                    if before == after {
                        result.push_str("file type or executable mode changed\n");
                    }
                }
                _ => result.push_str("Binary files differ\n"),
            }
        }
        Ok(result)
    }
}
