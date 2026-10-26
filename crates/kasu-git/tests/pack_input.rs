use gix_pack::data::entry::Header;
use kasu_core::*;
use kasu_git::*;
use proptest::prelude::*;
use sha1::{Digest, Sha1};
use std::{io::Write, sync::Arc};

fn entry(pack: &mut Vec<u8>, header: Header, size: usize, raw: &[u8]) {
    header.write_to(size as u64, pack).unwrap();
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    z.write_all(raw).unwrap();
    pack.extend(z.finish().unwrap());
}
fn start(count: u32) -> Vec<u8> {
    let mut pack = b"PACK".to_vec();
    pack.extend(2u32.to_be_bytes());
    pack.extend(count.to_be_bytes());
    pack
}
fn finish(mut pack: Vec<u8>) -> Vec<u8> {
    pack.extend_from_slice(&Sha1::digest(&pack));
    pack
}
fn base() -> RawObject {
    RawObject {
        kind: GitKind::Blob,
        bytes: Arc::from(b"0123456789".as_slice()),
    }
}
fn delta() -> Vec<u8> {
    let mut d = vec![10, 15, 0x90, 10, 5];
    d.extend(b"HELLO");
    d
}

#[test]
fn non_delta_ofs_forward_ref_and_thin_packs_decode_losslessly() {
    let base = base();
    let oid = GitOid::hash("blob", &base.bytes);
    let result = GitOid::hash("blob", b"0123456789HELLO");
    let mut pack = start(2);
    entry(&mut pack, Header::Blob, 10, &base.bytes);
    let offset = pack.len() as u64;
    entry(
        &mut pack,
        Header::OfsDelta {
            base_distance: offset - 12,
        },
        delta().len(),
        &delta(),
    );
    let decoded =
        decode_received_pack(&finish(pack), &ReceiveLimits::default(), &|_| Ok(None)).unwrap();
    assert_eq!(&*decoded[&result].bytes, b"0123456789HELLO");
    assert_eq!(&*decoded[&oid].bytes, &*base.bytes);
    let header = Header::RefDelta {
        base_id: gix::hash::ObjectId::from_bytes_or_panic(&oid.0),
    };
    let mut pack = start(2);
    entry(&mut pack, header, delta().len(), &delta());
    entry(&mut pack, Header::Blob, 10, &base.bytes);
    assert_eq!(
        &*decode_received_pack(&finish(pack), &ReceiveLimits::default(), &|_| Ok(None)).unwrap()
            [&result]
            .bytes,
        b"0123456789HELLO"
    );
    let mut pack = start(1);
    entry(&mut pack, header, delta().len(), &delta());
    let pack = finish(pack);
    assert!(decode_received_pack(&pack, &ReceiveLimits::default(), &|_| Ok(None)).is_err());
    let decoded = decode_received_pack(&pack, &ReceiveLimits::default(), &|id| {
        Ok((id == oid).then(|| base.clone()))
    })
    .unwrap();
    assert_eq!(&*decoded[&result].bytes, b"0123456789HELLO");
}

#[test]
fn corruption_sizes_counts_bombs_and_invalid_bases_are_rejected() {
    let mut good = start(1);
    entry(&mut good, Header::Blob, 1, b"x");
    let good = finish(good);
    let mut corrupt = good.clone();
    corrupt[12] ^= 1;
    assert!(decode_received_pack(&corrupt, &ReceiveLimits::default(), &|_| Ok(None)).is_err());
    for limits in [
        ReceiveLimits {
            pack_bytes: 10,
            ..Default::default()
        },
        ReceiveLimits {
            objects: 0,
            ..Default::default()
        },
        ReceiveLimits {
            object_bytes: 0,
            ..Default::default()
        },
        ReceiveLimits {
            expanded_bytes: 0,
            ..Default::default()
        },
    ] {
        assert!(decode_received_pack(&good, &limits, &|_| Ok(None)).is_err());
    }
    let mut bomb = start(1);
    entry(&mut bomb, Header::Blob, 1, &vec![b'x'; 100_000]);
    assert!(decode_received_pack(&finish(bomb), &ReceiveLimits::default(), &|_| Ok(None)).is_err());
    let mut offset = start(2);
    entry(&mut offset, Header::Blob, 1, b"x");
    entry(
        &mut offset,
        Header::OfsDelta { base_distance: 1 },
        2,
        &[1, 0],
    );
    assert!(
        decode_received_pack(&finish(offset), &ReceiveLimits::default(), &|_| Ok(None)).is_err()
    );
    let mut extra = start(0);
    extra.extend(b"garbage");
    assert!(
        decode_received_pack(&finish(extra), &ReceiveLimits::default(), &|_| Ok(None)).is_err()
    );
    let base = base();
    let oid = GitOid::hash("blob", &base.bytes);
    let mut huge = start(1);
    let raw = [10, 0x80, 0x80, 0x80, 0x80, 0x80, 0x20, 1, b'x'];
    entry(
        &mut huge,
        Header::RefDelta {
            base_id: gix::hash::ObjectId::from_bytes_or_panic(&oid.0),
        },
        raw.len(),
        &raw,
    );
    assert!(
        decode_received_pack(&finish(huge), &ReceiveLimits::default(), &|_| Ok(Some(
            base.clone()
        )))
        .is_err()
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]
    #[test]
    fn untrusted_pack_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(),0..1024)) {
        let limits=ReceiveLimits {pack_bytes:1024,objects:32,object_bytes:4096,expanded_bytes:16_384,delta_depth:8};
        let _=decode_received_pack(&bytes,&limits,&|_|Ok(None));
        let mut framed=start(bytes.first().copied().unwrap_or_default() as u32 % 8);framed.extend(bytes);
        let _=decode_received_pack(&finish(framed),&ReceiveLimits {pack_bytes:2048,..limits},&|_|Ok(None));
    }
}

#[test]
fn delta_chain_depth_is_bounded_before_resolution() {
    let mut pack = start(5);
    entry(&mut pack, Header::Blob, 1, b"x");
    let mut previous = 12u64;
    for _ in 0..4 {
        let offset = pack.len() as u64;
        entry(
            &mut pack,
            Header::OfsDelta {
                base_distance: offset - previous,
            },
            4,
            &[1, 1, 0x90, 1],
        );
        previous = offset;
    }
    let pack = finish(pack);
    assert!(
        decode_received_pack(
            &pack,
            &ReceiveLimits {
                delta_depth: 3,
                ..Default::default()
            },
            &|_| Ok(None)
        )
        .is_err()
    );
    assert!(
        decode_received_pack(
            &pack,
            &ReceiveLimits {
                delta_depth: 4,
                ..Default::default()
            },
            &|_| Ok(None)
        )
        .is_ok()
    );
}
