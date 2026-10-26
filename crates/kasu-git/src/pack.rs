use crate::{GitIndex, PackCache, pack_cache::CompressedObject};
use kasu_core::{GitKind, GitOid, Result, invalid};
use kasu_store::storage_error;
use sha1::{Digest, Sha1};
use std::sync::Arc;
use std::{collections::BTreeSet, io::Write};

struct HashedWriter<W> {
    inner: W,
    hash: Sha1,
}
impl<W: Write> Write for HashedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// The caller supplies the exact, connectivity-checked set of objects to send.
// Pack data is streamed and never persisted in the native repository.
pub fn write_pack(
    index: &GitIndex<'_>,
    oids: impl IntoIterator<Item = GitOid>,
    out: impl Write,
) -> Result<()> {
    write_pack_cached(index, oids, None, out)
}
pub fn write_pack_cached(
    index: &GitIndex<'_>,
    oids: impl IntoIterator<Item = GitOid>,
    cache: Option<&PackCache>,
    out: impl Write,
) -> Result<()> {
    let oids: BTreeSet<_> = oids.into_iter().collect();
    let count: u32 = oids
        .len()
        .try_into()
        .map_err(|_| invalid("too many pack objects"))?;
    let mut out = HashedWriter {
        inner: out,
        hash: Sha1::new(),
    };
    out.write_all(b"PACK").map_err(storage_error)?;
    out.write_all(&2u32.to_be_bytes()).map_err(storage_error)?;
    out.write_all(&count.to_be_bytes()).map_err(storage_error)?;
    for oid in oids {
        let record = index.require(oid)?;
        let compressed = if let Some(object) = cache.and_then(|c| c.get(oid, record.raw)) {
            object
        } else {
            let (kind, raw) = index.object(oid)?;
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(&raw).map_err(storage_error)?;
            let object = Arc::new(CompressedObject {
                kind,
                size: raw.len(),
                zlib: encoder.finish().map_err(storage_error)?,
            });
            if let Some(cache) = cache {
                cache.insert(oid, record.raw, object.clone());
            }
            object
        };
        if record.native.kind() != compressed.kind {
            return Err(invalid("cached Git object kind mismatch"));
        }
        let tag = match compressed.kind {
            GitKind::Commit => 1,
            GitKind::Tree => 2,
            GitKind::Blob => 3,
            GitKind::Tag => 4,
        };
        let mut size = compressed.size;
        let mut byte = (tag << 4) | (size as u8 & 15);
        size >>= 4;
        if size > 0 {
            byte |= 128;
        }
        out.write_all(&[byte]).map_err(storage_error)?;
        while size > 0 {
            let mut byte = size as u8 & 127;
            size >>= 7;
            if size > 0 {
                byte |= 128;
            }
            out.write_all(&[byte]).map_err(storage_error)?;
        }
        out.write_all(&compressed.zlib).map_err(storage_error)?;
    }
    let checksum = out.hash.finalize();
    out.inner.write_all(&checksum).map_err(storage_error)?;
    out.inner.flush().map_err(storage_error)
}
