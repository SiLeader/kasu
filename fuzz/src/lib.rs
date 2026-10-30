//! Bounded harnesses used both by libFuzzer and deterministic corpus replay.
pub mod api;
pub mod web;
pub mod wire;
use kasu_core::{GitKind, GitOid};
use kasu_git::{RawObject, ReceiveLimits, decode_received_pack};
use sha1::{Digest, Sha1};
use std::{io::Write, sync::Arc};
pub const MAX_INPUT: usize = 65536;
pub fn limits() -> ReceiveLimits {
    ReceiveLimits {
        pack_bytes: MAX_INPUT,
        objects: 32,
        object_bytes: 8192,
        expanded_bytes: 65536,
        delta_depth: 8,
    }
}
fn check_pack(bytes: &[u8], existing: &dyn Fn(GitOid) -> kasu_core::Result<Option<RawObject>>) {
    let limits = limits();
    if let Ok(objects) = decode_received_pack(bytes, &limits, existing) {
        assert!(objects.len() <= limits.objects as usize + 1);
        let mut total = 0;
        for (id, object) in objects {
            assert_eq!(id, GitOid::hash(object.kind.as_str(), &object.bytes));
            assert!(object.bytes.len() <= limits.object_bytes);
            total += object.bytes.len();
        }
        assert!(total <= limits.expanded_bytes);
    }
}
pub fn pack(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    check_pack(data, &|_| Ok(None));
    // Keep raw rejection paths, then repair the checksum so mutations also reach entry decoding.
    if data.len() >= 32 {
        let mut fixed = data[..data.len() - 20].to_vec();
        finish(&mut fixed);
        check_pack(&fixed, &|_| Ok(None));
    }
}
fn finish(pack: &mut Vec<u8>) {
    pack.extend_from_slice(&Sha1::digest(&*pack));
}
fn start(count: u32) -> Vec<u8> {
    let mut pack = b"PACK".to_vec();
    pack.extend(2u32.to_be_bytes());
    pack.extend(count.to_be_bytes());
    pack
}
fn entry(pack: &mut Vec<u8>, kind: u8, size: usize, data: &[u8], base: &[u8]) {
    let mut remaining = size >> 4;
    let mut first = (kind << 4) | ((size & 15) as u8);
    if remaining != 0 {
        first |= 128;
    }
    pack.push(first);
    while remaining != 0 {
        let mut byte = (remaining & 127) as u8;
        remaining >>= 7;
        if remaining != 0 {
            byte |= 128;
        }
        pack.push(byte);
    }
    pack.extend(base);
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    z.write_all(data).unwrap();
    pack.extend(z.finish().unwrap());
}
fn ofs(mut distance: u64) -> Vec<u8> {
    let mut out = vec![(distance & 127) as u8];
    while {
        distance >>= 7;
        distance != 0
    } {
        distance -= 1;
        out.push(128 | (distance & 127) as u8);
    }
    out.reverse();
    out
}
pub fn pack_delta(data: &[u8]) {
    let Some((&mode, tail)) = data.split_first() else {
        return;
    };
    let tail = &tail[..tail.len().min(4096)];
    let base = b"0123456789";
    let oid = GitOid::hash("blob", base);
    let mut pack = match mode % 5 {
        0 => {
            let mut p = start(1);
            entry(&mut p, 3, tail.len(), tail, &[]);
            p
        }
        1 => {
            let mut p = start(2);
            entry(&mut p, 3, base.len(), base, &[]);
            let distance = p.len() as u64 - 12;
            entry(&mut p, 6, tail.len(), tail, &ofs(distance));
            p
        }
        2 => {
            let mut p = start(2);
            entry(&mut p, 7, tail.len(), tail, &oid.0);
            entry(&mut p, 3, base.len(), base, &[]);
            p
        }
        3 => {
            let mut p = start(1);
            entry(&mut p, 7, tail.len(), tail, &oid.0);
            p
        }
        _ => {
            let depth = 1 + (tail.first().copied().unwrap_or(0) % 10);
            let mut p = start(1 + u32::from(depth));
            entry(&mut p, 3, base.len(), base, &[]);
            let mut offset = 12;
            // A fully valid copy chain crosses the configured depth limit as well as success paths.
            for _ in 0..depth {
                let current = p.len();
                entry(
                    &mut p,
                    6,
                    4,
                    &[10, 10, 0x90, 10],
                    &ofs((current - offset) as u64),
                );
                offset = current;
            }
            p
        }
    };
    finish(&mut pack);
    check_pack(&pack, &|id| {
        Ok((id == oid).then(|| RawObject {
            kind: GitKind::Blob,
            bytes: Arc::from(base.as_slice()),
        }))
    });
}
pub mod seeds;
