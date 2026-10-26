use kasu_core::{Result, invalid};
use kasu_git::ReceiveLimits;
use kasu_store::storage_error;
use std::io::BufRead;

// Find exactly one pack's end without waiting for SSH channel EOF. The existing
// receiver subsequently verifies its checksum, object graph and delta policy.
pub(super) fn read_pack(
    reader: &mut impl BufRead,
    out: &mut Vec<u8>,
    limits: &ReceiveLimits,
) -> Result<()> {
    let start = out.len();
    let take = |reader: &mut dyn std::io::Read, out: &mut Vec<u8>, n: usize| -> Result<()> {
        if out.len() - start + n > limits.pack_bytes {
            return Err(invalid("SSH pack byte limit exceeded"));
        }
        let old = out.len();
        out.resize(old + n, 0);
        reader.read_exact(&mut out[old..]).map_err(storage_error)
    };
    take(reader, out, 12)?;
    if &out[start..start + 4] != b"PACK" || out[start + 4..start + 8] != 2u32.to_be_bytes() {
        return Err(invalid("unsupported SSH pack header"));
    }
    let count = u32::from_be_bytes(out[start + 8..start + 12].try_into().unwrap());
    if count > limits.objects {
        return Err(invalid("SSH pack object count limit exceeded"));
    }
    let mut expanded = 0u64;
    for _ in 0..count {
        take(reader, out, 1)?;
        let mut byte = *out.last().unwrap();
        let kind = (byte >> 4) & 7;
        let mut size = u64::from(byte & 15);
        let mut shift = 4;
        while byte & 128 != 0 {
            if shift > 60 {
                return Err(invalid("SSH pack object header overflow"));
            }
            take(reader, out, 1)?;
            byte = *out.last().unwrap();
            let value = u64::from(byte & 127)
                .checked_shl(shift)
                .filter(|value| *value >> shift == u64::from(byte & 127))
                .ok_or_else(|| invalid("SSH pack object size overflow"))?;
            size = size
                .checked_add(value)
                .ok_or_else(|| invalid("SSH pack object size overflow"))?;
            shift += 7;
        }
        if size > limits.object_bytes as u64 {
            return Err(invalid("SSH pack object byte limit exceeded"));
        }
        match kind {
            1..=4 => {}
            6 => {
                let mut n = 0;
                loop {
                    take(reader, out, 1)?;
                    n += 1;
                    if n > 10 {
                        return Err(invalid("SSH pack delta offset overflow"));
                    }
                    if out.last().unwrap() & 128 == 0 {
                        break;
                    }
                }
            }
            7 => take(reader, out, 20)?,
            _ => return Err(invalid("invalid SSH pack object kind")),
        }
        let mut decoder = flate2::Decompress::new(true);
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let input = reader.fill_buf().map_err(storage_error)?;
            if input.is_empty() {
                return Err(invalid("truncated SSH compressed object"));
            }
            let before_in = decoder.total_in();
            let before_out = decoder.total_out();
            let status = decoder
                .decompress(input, &mut buffer, flate2::FlushDecompress::None)
                .map_err(storage_error)?;
            let consumed = (decoder.total_in() - before_in) as usize;
            let produced = decoder.total_out() - before_out;
            expanded = expanded
                .checked_add(produced)
                .ok_or_else(|| invalid("SSH pack expanded byte overflow"))?;
            if decoder.total_out() > size || expanded > limits.expanded_bytes as u64 {
                return Err(invalid("SSH pack expanded byte limit exceeded"));
            }
            if out.len() - start + consumed > limits.pack_bytes {
                return Err(invalid("SSH pack byte limit exceeded"));
            }
            out.extend_from_slice(&input[..consumed]);
            reader.consume(consumed);
            if status == flate2::Status::StreamEnd {
                break;
            }
            if consumed == 0 && produced == 0 {
                return Err(invalid("invalid SSH compressed stream"));
            }
        }
        if decoder.total_out() != size {
            return Err(invalid("SSH pack object size mismatch"));
        }
    }
    take(reader, out, 20)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kasu_core::*;
    use kasu_git::*;
    use kasu_store::*;
    use std::io::{BufReader, Cursor, Read};
    #[test]
    fn pack_boundaries_do_not_consume_following_messages_or_require_eof() {
        let dir = tempfile::tempdir().unwrap();
        let meta = SqliteStore::memory().unwrap();
        let blobs = FsBlobStore::open(dir.path()).unwrap();
        let index = GitIndex {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
            projection: ProjectionId::full(b"repo".as_slice()),
        };
        let bytes = b"repeated contents to exercise compressed streaming\n".repeat(20);
        let blob = index.repo.put_blob(&bytes).unwrap();
        let oid = GitOid::hash("blob", &bytes);
        index.put(oid, NativeTarget::Blob(blob), &bytes).unwrap();
        for objects in [vec![], vec![oid]] {
            let mut pack = Vec::new();
            write_pack(&index, objects, &mut pack).unwrap();
            let wire = [pack.as_slice(), b"0000"].concat();
            let mut reader = BufReader::with_capacity(1, Cursor::new(wire));
            let mut out = b"command prefix".to_vec();
            read_pack(&mut reader, &mut out, &ReceiveLimits::default()).unwrap();
            assert_eq!(&out[b"command prefix".len()..], pack);
            let mut rest = Vec::new();
            reader.read_to_end(&mut rest).unwrap();
            assert_eq!(rest, b"0000");
            let limits = ReceiveLimits {
                pack_bytes: pack.len() - 1,
                ..Default::default()
            };
            assert!(read_pack(&mut Cursor::new(&pack), &mut Vec::new(), &limits).is_err());
            assert!(
                read_pack(
                    &mut Cursor::new(&pack[..pack.len() - 1]),
                    &mut Vec::new(),
                    &ReceiveLimits::default()
                )
                .is_err()
            );
        }
    }
}
