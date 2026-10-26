use crate::Key;
use kasu_core::ObjectId;
use kasu_object::Kind;
use kasu_store::{BlobStore, FsBlobStore};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::Path,
    sync::Arc,
};
use tonic::Status;
fn io(_: impl std::fmt::Display) -> Status {
    Status::internal("disk object cache IO failed")
}
#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Scope {
    version: u32,
    repository: Vec<u8>,
}
mod state;
pub use state::{DiskLimits, DiskStats, DiskStore};
impl DiskStore {
    /// Export every cached frame with the repository memberships that let it
    /// be served.
    pub fn export_snapshot(&self, path: &Path) -> Result<usize, Status> {
        self.export_snapshot_for(path, &[])
    }
    /// Export only frames confirmed for `repositories` (all when empty).
    pub fn export_snapshot_for(
        &self,
        path: &Path,
        repositories: &[Vec<u8>],
    ) -> Result<usize, Status> {
        let selected = repositories
            .iter()
            .map(|r| {
                crate::normalize(r).ok_or_else(|| Status::invalid_argument("invalid repository"))
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let guard = self.guard(false)?;
        let members: Vec<_> = self
            .members_locked(&guard)?
            .into_iter()
            .filter(|(repo, _, _)| selected.is_empty() || selected.contains(repo))
            .collect();
        let wanted: std::collections::HashSet<Key> =
            members.iter().map(|(_, _, key)| *key).collect();
        let keys: Vec<_> = self
            .catalog_locked(&guard)?
            .into_iter()
            .filter(|(key, _)| selected.is_empty() || wanted.contains(key))
            .collect();
        let position: std::collections::HashMap<Key, usize> = keys
            .iter()
            .enumerate()
            .map(|(n, (key, _))| (*key, n))
            .collect();
        let mut snapshot_repositories: Vec<SnapshotRepository> = Vec::new();
        for (repo, identity, key) in members {
            let Some(index) = position.get(&key) else {
                continue;
            };
            if snapshot_repositories.last().is_none_or(|r| r.name != repo) {
                snapshot_repositories.push(SnapshotRepository {
                    name: repo,
                    identity,
                    members: Vec::new(),
                });
            }
            snapshot_repositories
                .last_mut()
                .unwrap()
                .members
                .push(*index);
        }
        let manifest = Manifest {
            version: 2,
            repository: None,
            repositories: snapshot_repositories,
            entries: keys
                .iter()
                .map(|(key, size)| Item {
                    kind: key.kind.name().into(),
                    id: key.id.to_string(),
                    size: *size,
                })
                .collect(),
        };
        let manifest = serde_json::to_vec(&manifest).map_err(io)?;
        if keys.len() > 100_000 || manifest.len() > 16 * 1024 * 1024 {
            return Err(Status::resource_exhausted(
                "snapshot manifest exceeds limits",
            ));
        }
        let parent = path.parent().unwrap_or(Path::new("."));
        let mut output = tempfile::Builder::new()
            .prefix(".kasu-snapshot-write-")
            .tempfile_in(parent)
            .map_err(io)?;
        {
            let mut tar = tar::Builder::new(output.as_file_mut());
            append(&mut tar, "manifest.json", &manifest)?;
            for (key, _) in &keys {
                let bytes = self
                    .get_locked(*key)?
                    .ok_or_else(|| Status::data_loss("snapshot object disappeared"))?;
                append(&mut tar, &item_path(*key), &bytes)?;
            }
            tar.finish().map_err(io)?;
        }
        output.as_file().sync_all().map_err(io)?;
        output.persist_noclobber(path).map_err(io)?;
        fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(io)?;
        Ok(keys.len())
    }
    pub fn import_snapshot(&self, path: &Path, max_bytes: u64) -> Result<usize, Status> {
        let input = fs::File::open(path).map_err(io)?;
        if input.metadata().map_err(io)?.len() > max_bytes.saturating_add(64 * 1024 * 1024) {
            return Err(Status::resource_exhausted("snapshot archive exceeds limit"));
        }
        let mut archive = tar::Archive::new(input);
        let mut entries = archive.entries().map_err(io)?;
        let mut header = entries
            .next()
            .ok_or_else(|| Status::data_loss("snapshot manifest missing"))?
            .map_err(io)?;
        if !header.header().entry_type().is_file()
            || header.path().map_err(io)?.as_ref() != Path::new("manifest.json")
            || header.size() > 16 * 1024 * 1024
        {
            return Err(Status::data_loss("invalid snapshot manifest entry"));
        }
        let mut bytes = vec![];
        header.read_to_end(&mut bytes).map_err(io)?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|_| Status::data_loss("invalid snapshot manifest"))?;
        // Version 1 snapshots hold frames of one repository.
        let repositories = match (manifest.version, &manifest.repository) {
            (1, Some(repository)) if manifest.repositories.is_empty() => {
                vec![SnapshotRepository {
                    name: repository.clone(),
                    identity: None,
                    members: (0..manifest.entries.len()).collect(),
                }]
            }
            (2, None) => manifest.repositories.clone(),
            _ => return Err(Status::data_loss("unsupported snapshot version")),
        };
        let mut memberships = Vec::new();
        if repositories.len() > 4096 {
            return Err(Status::resource_exhausted(
                "snapshot repository count exceeds limit",
            ));
        }
        for repository in repositories {
            let name = crate::normalize(&repository.name)
                .ok_or_else(|| Status::data_loss("invalid snapshot repository"))?;
            if repository
                .identity
                .as_ref()
                .is_some_and(|i| i.is_empty() || i.len() > 128)
                || repository
                    .members
                    .iter()
                    .any(|n| *n >= manifest.entries.len())
            {
                return Err(Status::data_loss("invalid snapshot membership"));
            }
            memberships.push((name, repository.identity, repository.members));
        }
        if manifest.entries.len() > 100_000 {
            return Err(Status::resource_exhausted(
                "snapshot object count exceeds limit",
            ));
        }
        let mut guard = self.guard(true)?;
        let limits = self.limits_locked(&guard)?;
        if manifest.entries.len() as u64 > limits.entries {
            return Err(Status::resource_exhausted(
                "snapshot exceeds disk cache entry limit",
            ));
        }
        let stage = tempfile::Builder::new()
            .prefix(".snapshot-import-")
            .tempdir_in(&self.root)
            .map_err(io)?;
        let staging = FsBlobStore::open(stage.path()).map_err(io)?;
        let mut seen = BTreeSet::new();
        let mut keys = vec![];
        let mut total = 0u64;
        let mut physical = BTreeSet::new();
        let mut physical_bytes = 0u64;
        for item in manifest.entries {
            let kind = match item.kind.as_str() {
                "trees" => Kind::Tree,
                "blobs" => Kind::Blob,
                _ => return Err(Status::data_loss("invalid snapshot object kind")),
            };
            let id = item
                .id
                .parse::<ObjectId>()
                .map_err(|_| Status::data_loss("invalid snapshot object ID"))?;
            if id.to_string() != item.id
                || !seen.insert((kind, id))
                || item.size == 0
                || item.size > kasu_object::MAX_FRAME_BYTES as u64
            {
                return Err(Status::data_loss("invalid snapshot object descriptor"));
            }
            total = total
                .checked_add(item.size)
                .ok_or_else(|| Status::resource_exhausted("snapshot size overflow"))?;
            if total > max_bytes {
                return Err(Status::resource_exhausted("snapshot exceeds size limit"));
            }
            if physical.insert(id) {
                physical_bytes = physical_bytes
                    .checked_add(self.existing_size(&guard, id)?.unwrap_or(item.size))
                    .ok_or_else(|| Status::resource_exhausted("snapshot size overflow"))?;
                if physical_bytes > limits.bytes {
                    return Err(Status::resource_exhausted(
                        "snapshot exceeds disk cache byte limit",
                    ));
                }
            }
            let key = Key { kind, id };
            let mut entry = entries
                .next()
                .ok_or_else(|| Status::data_loss("snapshot object missing"))?
                .map_err(io)?;
            if !entry.header().entry_type().is_file()
                || entry.path().map_err(io)?.as_ref() != Path::new(&item_path(key))
                || entry.size() != item.size
            {
                return Err(Status::data_loss("unexpected snapshot entry"));
            }
            let mut frame = vec![];
            entry.read_to_end(&mut frame).map_err(io)?;
            if frame.len() as u64 != item.size {
                return Err(Status::data_loss("truncated snapshot frame"));
            }
            kasu_object::validate(kind, id, &frame)?;
            staging.put_compressed(id, &frame).map_err(io)?;
            keys.push(key);
        }
        if entries.next().is_some() {
            return Err(Status::data_loss("unexpected extra snapshot entry"));
        }
        if keys.is_empty() {
            return Ok(0);
        }
        // Pin the entire validated import so quota eviction cannot discard an earlier entry.
        {
            let tx = guard.transaction().map_err(io)?;
            for key in &keys {
                tx.execute(
                    "INSERT INTO pinned(kind,id) VALUES(?1,?2)",
                    rusqlite::params![key.kind.name(), key.id.to_string()],
                )
                .map_err(io)?;
            }
            tx.execute("INSERT INTO candidates(kind,id,accessed) SELECT kinds.kind,kinds.id,frames.accessed FROM kinds JOIN frames USING(id) WHERE NOT EXISTS(SELECT 1 FROM pinned WHERE pinned.kind=kinds.kind AND pinned.id=kinds.id)",[]).map_err(io)?;
            tx.commit().map_err(io)?;
        }
        let result = (|| {
            for key in &keys {
                self.put_locked(
                    &mut guard,
                    *key,
                    Arc::new(staging.get_compressed(key.id).map_err(io)?),
                )?;
                let cached: bool = guard
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM kinds WHERE kind=?1 AND id=?2)",
                        rusqlite::params![key.kind.name(), key.id.to_string()],
                        |r| r.get(0),
                    )
                    .map_err(io)?;
                if !cached {
                    return Err(Status::data_loss(
                        "validated snapshot entry was not admitted",
                    ));
                }
            }
            Ok::<_, Status>(())
        })();
        guard
            .execute_batch("DELETE FROM pinned;DELETE FROM candidates;")
            .map_err(io)?;
        result?;
        for (repo, identity, members) in memberships {
            if let Some(identity) = identity {
                self.identify_locked(&mut guard, &repo, &identity)?;
            }
            for index in members {
                self.add_member_locked(&guard, &repo, keys[index])?;
            }
        }
        Ok(keys.len())
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository: Option<Vec<u8>>,
    #[serde(default)]
    repositories: Vec<SnapshotRepository>,
    entries: Vec<Item>,
}
/// Frames confirmed for one repository, as indices into `entries`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotRepository {
    name: Vec<u8>,
    identity: Option<String>,
    members: Vec<usize>,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    kind: String,
    id: String,
    size: u64,
}
fn item_path(key: Key) -> String {
    format!("objects/{}/{}.zst", key.kind.name(), key.id)
}
fn append<W: Write>(archive: &mut tar::Builder<W>, path: &str, bytes: &[u8]) -> Result<(), Status> {
    let mut header = tar::Header::new_ustar();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(0);
    header.set_cksum();
    archive.append_data(&mut header, path, bytes).map_err(io)
}
