use crate::{BlobStore, decompress, storage_error};
use kasu_core::{Error, ObjectId, Result, invalid};
use std::{
    fs,
    io::Write,
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
}
impl BlobStore for FsBlobStore {
    fn get_compressed(&self, id: ObjectId) -> Result<Vec<u8>> {
        fs::read(self.path(id)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::NotFound(id.to_string())
            } else {
                storage_error(error)
            }
        })
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
