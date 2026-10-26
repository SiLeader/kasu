use crate::{from_gix_kind, git_error, oid_from_bytes};
use gix_pack::{
    cache::DecodeEntry,
    data::{self, entry::Header},
};
use kasu_core::{GitKind, GitOid, Result, invalid};
use sha1::{Digest, Sha1};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug)]
pub struct ReceiveLimits {
    pub pack_bytes: usize,
    pub objects: u32,
    pub object_bytes: usize,
    pub expanded_bytes: usize,
    pub delta_depth: u32,
}
impl Default for ReceiveLimits {
    fn default() -> Self {
        Self {
            pack_bytes: 128 * 1024 * 1024,
            objects: 100_000,
            object_bytes: 64 * 1024 * 1024,
            expanded_bytes: 512 * 1024 * 1024,
            delta_depth: 64,
        }
    }
}
#[derive(Clone, Debug)]
pub struct RawObject {
    pub kind: GitKind,
    pub bytes: Arc<[u8]>,
}
struct Decoded {
    object: RawObject,
    depth: u32,
    compressed: usize,
}
struct Cache<'a>(&'a BTreeMap<u64, Decoded>);
impl DecodeEntry for Cache<'_> {
    fn put(&mut self, _: u32, _: u64, _: &[u8], _: gix::objs::Kind, _: usize) {}
    fn get(&mut self, _: u32, offset: u64, out: &mut Vec<u8>) -> Option<(gix::objs::Kind, usize)> {
        let decoded = self.0.get(&offset)?;
        out.clear();
        out.extend_from_slice(&decoded.object.bytes);
        Some((kind(decoded.object.kind), decoded.compressed))
    }
}
fn kind(k: GitKind) -> gix::objs::Kind {
    match k {
        GitKind::Blob => gix::objs::Kind::Blob,
        GitKind::Tree => gix::objs::Kind::Tree,
        GitKind::Commit => gix::objs::Kind::Commit,
        GitKind::Tag => gix::objs::Kind::Tag,
    }
}
fn charge(total: &mut usize, n: usize, limit: usize) -> Result<()> {
    *total = total
        .checked_add(n)
        .filter(|n| *n <= limit)
        .ok_or_else(|| invalid("pack expanded byte limit exceeded"))?;
    Ok(())
}
fn insert_object(
    objects: &mut BTreeMap<GitOid, RawObject>,
    oid: GitOid,
    object: RawObject,
) -> Result<()> {
    match objects.entry(oid) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(object);
        }
        std::collections::btree_map::Entry::Occupied(entry) => {
            if entry.get().kind != object.kind || entry.get().bytes != object.bytes {
                return Err(invalid("conflicting objects have the same Git OID"));
            }
        }
    }
    Ok(())
}

/// Decode a bounded pack, including forward ref-deltas and authorized thin
/// bases. The caller controls which pre-existing objects can be used as bases.
pub fn decode_received_pack(
    bytes: &[u8],
    limits: &ReceiveLimits,
    existing: &dyn Fn(GitOid) -> Result<Option<RawObject>>,
) -> Result<BTreeMap<GitOid, RawObject>> {
    if bytes.len() > limits.pack_bytes {
        return Err(invalid("pack byte limit exceeded"));
    }
    let pack = data::File::from_data(bytes, "received.pack".into(), gix::hash::Kind::Sha1)
        .map_err(git_error)?
        .with_alloc_limit_bytes(Some(limits.object_bytes.saturating_mul(3)));
    if pack.num_objects() > limits.objects {
        return Err(invalid("pack object count limit exceeded"));
    }
    let end = bytes.len() - 20;
    if Sha1::digest(&bytes[..end]).as_slice() != &bytes[end..] {
        return Err(invalid("pack checksum mismatch"));
    }
    let mut offset = 12u64;
    let mut expanded = 0;
    let mut entries = BTreeMap::new();
    let mut decoded = BTreeMap::new();
    let mut objects = BTreeMap::new();
    let mut depths = BTreeMap::new();
    let mut inflate = gix_zlib::Inflate::default();
    for _ in 0..pack.num_objects() {
        if offset as usize >= end {
            return Err(invalid("truncated pack entries"));
        }
        let entry = pack.entry(offset).map_err(git_error)?;
        let size = usize::try_from(entry.decompressed_size).map_err(git_error)?;
        if size > limits.object_bytes {
            return Err(invalid("pack object size limit exceeded"));
        }
        charge(&mut expanded, size, limits.expanded_bytes)?;
        if let Header::OfsDelta { base_distance } = entry.header {
            let base = Header::verified_base_pack_offset(offset, base_distance)
                .ok_or_else(|| invalid("invalid ofs-delta base"))?;
            if !entries.contains_key(&base) {
                return Err(invalid("ofs-delta base is not an entry boundary"));
            }
        }
        let mut raw = vec![0; size];
        let compressed = pack
            .decompress_entry(&entry, &mut inflate, &mut raw)
            .map_err(git_error)?;
        let next = entry
            .data_offset
            .checked_add(compressed as u64)
            .filter(|n| *n <= end as u64)
            .ok_or_else(|| invalid("pack entry exceeds trailer"))?;
        if let Some(k) = entry.header.as_kind() {
            let k = from_gix_kind(k);
            let oid = GitOid::hash(k.as_str(), &raw);
            let object = RawObject {
                kind: k,
                bytes: raw.into(),
            };
            insert_object(&mut objects, oid, object.clone())?;
            depths.insert(oid, 0);
            decoded.insert(
                entry.data_offset,
                Decoded {
                    object,
                    depth: 0,
                    compressed,
                },
            );
        }
        entries.insert(offset, entry);
        offset = next;
    }
    if offset as usize != end {
        return Err(invalid("pack has trailing bytes or incorrect object count"));
    }
    let mut pending: Vec<_> = entries
        .values()
        .filter(|e| e.header.is_delta())
        .cloned()
        .collect();
    // Every pass resolves at least one level; the depth bound also bounds the
    // number of full scans for adversarial reverse-ordered ref-delta chains.
    for _ in 0..=limits.delta_depth {
        if pending.is_empty() {
            return Ok(objects);
        }
        let previous = pending.len();
        let mut unresolved = Vec::new();
        for entry in pending {
            let depth = match entry.header {
                Header::OfsDelta { base_distance } => {
                    let base_offset =
                        Header::verified_base_pack_offset(entry.pack_offset(), base_distance)
                            .unwrap();
                    let base = &entries[&base_offset];
                    let Some(base) = decoded.get(&base.data_offset) else {
                        unresolved.push(entry);
                        continue;
                    };
                    base.depth + 1
                }
                Header::RefDelta { base_id } => {
                    let oid = oid_from_bytes(base_id.as_bytes())?;
                    if let std::collections::btree_map::Entry::Vacant(slot) = objects.entry(oid) {
                        if let Some(object) = existing(oid)? {
                            if object.bytes.len() > limits.object_bytes
                                || GitOid::hash(object.kind.as_str(), &object.bytes) != oid
                            {
                                return Err(invalid("invalid thin-pack base"));
                            }
                            charge(&mut expanded, object.bytes.len(), limits.expanded_bytes)?;
                            slot.insert(object);
                            depths.insert(oid, 0);
                        } else {
                            unresolved.push(entry);
                            continue;
                        }
                    }
                    depths[&oid] + 1
                }
                _ => unreachable!(),
            };
            if depth > limits.delta_depth {
                return Err(invalid("pack delta depth limit exceeded"));
            }
            let header = pack
                .decode_header(entry.clone(), &mut inflate, &|id| {
                    let oid = oid_from_bytes(id.as_bytes()).ok()?;
                    let object = objects.get(&oid)?;
                    Some(data::decode::header::ResolvedBase::OutOfPack {
                        kind: kind(object.kind),
                        num_deltas: Some(0),
                    })
                })
                .map_err(git_error)?;
            if header.object_size > limits.object_bytes as u64 {
                return Err(invalid("delta result size limit exceeded"));
            }
            charge(
                &mut expanded,
                header.object_size as usize,
                limits.expanded_bytes,
            )?;
            let mut raw = Vec::new();
            let result = pack
                .decode_entry(
                    entry.clone(),
                    &mut raw,
                    &mut inflate,
                    &|id, out| {
                        let oid = oid_from_bytes(id.as_bytes()).ok()?;
                        let object = objects.get(&oid)?;
                        out.clear();
                        out.extend_from_slice(&object.bytes);
                        Some(data::decode::entry::ResolvedBase::OutOfPack {
                            kind: kind(object.kind),
                            end: out.len(),
                        })
                    },
                    &mut Cache(&decoded),
                )
                .map_err(git_error)?;
            let k = from_gix_kind(result.kind);
            let oid = GitOid::hash(k.as_str(), &raw);
            let object = RawObject {
                kind: k,
                bytes: raw.into(),
            };
            depths
                .entry(oid)
                .and_modify(|d| *d = (*d).min(depth))
                .or_insert(depth);
            insert_object(&mut objects, oid, object.clone())?;
            decoded.insert(
                entry.data_offset,
                Decoded {
                    object,
                    depth,
                    compressed: 0,
                },
            );
        }
        if unresolved.len() == previous {
            return Err(invalid("unresolved or cyclic delta bases"));
        }
        pending = unresolved;
    }
    Err(invalid("pack delta depth limit exceeded"))
}
