use kasu_core::*;
use kasu_git::*;
use proptest::prelude::*;
fn encoded(lines: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        write_packet(&mut out, line).unwrap();
    }
    out
}
#[test]
fn framing_and_unadvertised_capabilities_are_rejected() {
    for bytes in [
        b"ffff".as_slice(),
        b"0003",
        b"0008x",
        b"00zz",
        b"000",
        b"0004x",
    ] {
        assert!(parse_packets(bytes).is_err(), "{bytes:?}");
    }
    assert_eq!(
        parse_packets(b"0000000100020004").unwrap(),
        vec![
            Packet::Flush,
            Packet::Delimiter,
            Packet::ResponseEnd,
            Packet::Data(b"")
        ]
    );
    let mut out = Vec::new();
    assert!(write_packet(&mut out, &vec![0; 65517]).is_err());
    let oid = GitOid([1; 20]);
    let mut req = encoded(&[format!("want {oid} sideband-all\n").as_bytes()]);
    req.extend(b"0000");
    assert!(parse_upload(Protocol::V0, &req).is_err());
    let mut req = encoded(&[b"command=fetch\n"]);
    req.extend(b"0001");
    req.extend(encoded(&[
        format!("want {oid}\n").as_bytes(),
        b"filter tree:0\n",
        b"done\n",
    ]));
    req.extend(b"0000");
    assert!(parse_upload(Protocol::V2, &req).is_err());
}
#[test]
fn blob_filter_negotiation_is_protocol_specific_and_strict() {
    let oid = GitOid([1; 20]);
    for protocol in [Protocol::V0, Protocol::V1, Protocol::V2] {
        for (filters, capability, valid) in [
            (vec!["blob:none"], true, true),
            (vec!["blob:none"], false, protocol == Protocol::V2),
            (vec!["blob:none", "blob:none"], true, false),
            (vec!["blob:limit=1"], true, false),
            (vec![""], true, false),
            (vec!["combine:blob:none+tree:0"], true, false),
        ] {
            let mut bytes = Vec::new();
            if protocol == Protocol::V2 {
                write_packet(&mut bytes, b"command=fetch\n").unwrap();
                bytes.extend(b"0001");
            }
            let caps = if protocol != Protocol::V2 && capability {
                " filter"
            } else {
                ""
            };
            write_packet(&mut bytes, format!("want {oid}{caps}\n").as_bytes()).unwrap();
            for filter in filters {
                write_packet(&mut bytes, format!("filter {filter}\n").as_bytes()).unwrap();
            }
            if protocol != Protocol::V2 {
                bytes.extend(b"0000");
            }
            write_packet(&mut bytes, b"done\n").unwrap();
            if protocol == Protocol::V2 {
                bytes.extend(b"0000");
            }
            let result = parse_upload(protocol, &bytes);
            assert_eq!(result.is_ok(), valid, "{protocol:?}: {result:?}");
            if let Ok(UploadRequest::Fetch(q)) = result {
                assert_eq!(q.filter, Some(FetchFilter::BlobNone));
            }
        }
    }
}
#[test]
fn unborn_head_and_tag_peeling_use_separate_protocol_encodings() {
    let refs = vec![ResolvedRef {
        name: b"HEAD".to_vec(),
        oid: None,
        peeled: None,
        symbolic: Some(b"refs/heads/main".to_vec()),
    }];
    let v0 = advertisement(Protocol::V0, &refs).unwrap();
    let packets = parse_packets(&v0).unwrap();
    assert!(
        packets.iter().any(
            |p| matches!(p,Packet::Data(line) if line.windows(15).any(|w|w==b"capabilities^{}"))
        )
    );
    let v2 = ls_refs(
        &LsRefsRequest {
            unborn: true,
            symrefs: true,
            ..Default::default()
        },
        &refs,
    )
    .unwrap();
    assert_eq!(
        parse_packets(&v2).unwrap()[0],
        Packet::Data(b"unborn HEAD symref-target:refs/heads/main\n")
    );
    assert!(
        ls_refs(
            &LsRefsRequest {
                prefixes: vec![b"refs/tags".to_vec()],
                ..Default::default()
            },
            &refs
        )
        .unwrap()
            == b"0000"
    );
}
proptest! {
    #[test]
    fn pktline_and_command_parsers_never_panic(bytes in prop::collection::vec(any::<u8>(),0..4096)) {
        let _=parse_packets(&bytes);let _=parse_upload(Protocol::V0,&bytes);let _=parse_upload(Protocol::V2,&bytes);
    }
    #[test]
    fn pktline_roundtrip(payload in prop::collection::vec(any::<u8>(),0..65516)) {
        let mut out=Vec::new();write_packet(&mut out,&payload).unwrap();prop_assert_eq!(parse_packets(&out).unwrap(),vec![Packet::Data(payload.as_slice())]);
    }
}
