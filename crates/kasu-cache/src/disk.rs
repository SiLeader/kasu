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
    pub fn export_snapshot(&self, path: &Path) -> Result<usize, Status> {
        let guard = self.guard(false)?;
        let keys = self.catalog_locked(&guard)?;
        let manifest = Manifest {
            version: 1,
            repository: self.repo.clone(),
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
        if manifest.version != 1 || manifest.repository != self.repo {
            return Err(Status::permission_denied(
                "snapshot repository or version mismatch",
            ));
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
        Ok(keys.len())
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    repository: Vec<u8>,
    entries: Vec<Item>,
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
