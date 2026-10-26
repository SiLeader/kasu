use kasu_git::{Packet, Protocol, parse_packets, parse_receive, parse_upload, write_packet};
use kasu_proto::v1 as p;
use prost::Message;
use std::fmt::Debug;
pub fn pktline(data: &[u8]) {
    if data.len() > crate::MAX_INPUT {
        return;
    }
    if let Ok(packets) = parse_packets(data) {
        let mut encoded = vec![];
        for p in &packets {
            match p {
                Packet::Data(raw) => write_packet(&mut encoded, raw).unwrap(),
                Packet::Flush => encoded.extend(b"0000"),
                Packet::Delimiter => encoded.extend(b"0001"),
                Packet::ResponseEnd => encoded.extend(b"0002"),
            }
        }
        assert_eq!(parse_packets(&encoded).unwrap(), packets);
    }
    for protocol in [Protocol::V0, Protocol::V1, Protocol::V2] {
        let _ = parse_upload(protocol, data);
    }
    if let Ok(receive) = parse_receive(data) {
        assert!(receive.commands.len() <= 1024);
        assert!(receive.shallow.len() <= 1024);
        assert!(receive.pack.len() <= data.len());
    }
    // Reframe arbitrary payload to exercise parsers beyond malformed length prefixes.
    if data.len() <= 4096 {
        let mut framed = vec![];
        for line in data.split(|b| *b == b'\n').take(32) {
            write_packet(&mut framed, line).unwrap();
        }
        framed.extend(b"0000");
        for protocol in [Protocol::V0, Protocol::V1, Protocol::V2] {
            let _ = parse_upload(protocol, &framed);
        }
        let _ = parse_receive(&framed);
    }
}
fn roundtrip<T: Message + Default + PartialEq + Debug>(data: &[u8]) {
    if let Ok(value) = T::decode(data) {
        let encoded = value.encode_to_vec();
        assert_eq!(T::decode(encoded.as_slice()).unwrap(), value);
    }
}
pub fn protobuf(data: &[u8]) {
    if data.len() > crate::MAX_INPUT {
        return;
    }
    let Some((&selector, raw)) = data.split_first() else {
        return;
    };
    macro_rules! messages{($($name:ident),* $(,)?)=>{let targets:&[fn(&[u8])]=&[$(roundtrip::<p::$name>),*];targets[selector as usize%targets.len()](raw);};}
    messages!(
        ObjectRequest,
        TreeObject,
        BlobChunk,
        LandRequest,
        LandResponse,
        BlobUpload,
        BlobReceipt,
        UploadBlobsResponse,
        UploadBlobsHttpRequest,
        CommitMetadata,
        CreateChangesetRequest,
        MergeRequest,
        RebaseRequest,
        MergeEntry,
        LineConflict,
        TreeConflict,
        RewriteResponse,
        UpdateRefRequest,
        UpdateRefResponse,
        ChangesetRequest,
        RootTreeResponse,
        Signature,
        File,
        Entry,
        FileChange,
        Changeset,
        RefRequest,
        Ref,
        ListRefsRequest,
        ListRefsResponse,
        LogRequest,
        LogResponse,
        CompareRequest,
        AncestorResponse,
        MergeBaseResponse,
        DiffRequest,
        ContentEdit,
        FileDiff,
        DiffResponse,
        PathRequest,
        LineOrigin,
        BlameResponse,
        ReadFileResponse,
        DirEntry,
        ListDirResponse,
        GitOidRequest,
        FindChangesetResponse
    );
}
