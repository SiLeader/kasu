use crate::{BlobStore, MAX_COMPRESSED_OBJECT_SIZE, decompress, storage_error};
use kasu_core::{Error, ObjectId, Result, invalid};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub struct FsBlobStore {
    root: PathBuf,
}
impl FsBlobStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(root.as_ref()).map_err(storage_error)?;
        Ok(Self {
            root: root.as_ref().to_owned(),
        })
    }
    fn path(&self, id: ObjectId) -> PathBuf {
        let hex = id.to_string();
        self.root.join(&hex[..2]).join(&hex[2..])
    }
    fn read(&self, id: ObjectId) -> Result<(Vec<u8>, Vec<u8>)> {
        let file = fs::File::open(self.path(id)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::NotFound(id.to_string())
            } else {
                storage_error(error)
            }
        })?;
        if file.metadata().map_err(storage_error)?.len() > MAX_COMPRESSED_OBJECT_SIZE as u64 {
            return Err(invalid("compressed object too large"));
        }
        // Also bound the read: a file can grow after the metadata check.
        let mut bytes = Vec::new();
        file.take(MAX_COMPRESSED_OBJECT_SIZE as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(storage_error)?;
        if bytes.len() > MAX_COMPRESSED_OBJECT_SIZE {
            return Err(invalid("compressed object too large"));
        }
        let raw = decompress(&bytes).map_err(|error| Error::Corrupt(error.to_string()))?;
        if ObjectId::hash(&raw) != id {
            return Err(Error::Corrupt("object hash mismatch".into()));
        }
        Ok((bytes, raw))
    }
}
impl BlobStore for FsBlobStore {
    fn get_compressed(&self, id: ObjectId) -> Result<Vec<u8>> {
        self.read(id).map(|(frame, _)| frame)
    }
    fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        self.read(id).map(|(_, raw)| raw)
    }
    fn put_compressed(&self, id: ObjectId, bytes: &[u8]) -> Result<()> {
        let raw = decompress(bytes)?;
        if ObjectId::hash(&raw) != id {
            return Err(invalid("object hash mismatch"));
        }
        let path = self.path(id);
        if path.exists() {
            if self.get(id)? != raw {
                return Err(Error::Collision(id.to_string()));
            }
            return Ok(());
        }
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).map_err(storage_error)?;
        // Persist the shard's entry before publishing an object in it. Sync even
        // if another writer created the shard but has not synced the root yet.
        fs::File::open(&self.root)
            .and_then(|dir| dir.sync_all())
            .map_err(storage_error)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(storage_error)?;
        tmp.write_all(bytes).map_err(storage_error)?;
        tmp.as_file().sync_all().map_err(storage_error)?;
        match tmp.persist_noclobber(&path) {
            Ok(_) => {
                fs::File::open(parent)
                    .and_then(|dir| dir.sync_all())
                    .map_err(storage_error)?;
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if self.get(id)? != raw {
                    return Err(Error::Collision(id.to_string()));
                }
            }
            Err(error) => return Err(storage_error(error)),
        }
        Ok(())
    }
}
