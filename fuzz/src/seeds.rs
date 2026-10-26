use crate::{entry, finish, ofs, start};
use kasu_core::GitOid;
use kasu_git::write_packet;
use kasu_proto::v1 as p;
use prost::Message;
use std::{fs, io, path::Path};
fn save(root: &Path, target: &str, name: &str, data: &[u8]) -> io::Result<()> {
    let folder = root.join(target);
    fs::create_dir_all(&folder)?;
    fs::write(folder.join(name), data)
}
pub fn generate(root: &Path) -> io::Result<()> {
    for name in ["pack", "pack_delta", "pktline", "protobuf", "native_api"] {
        fs::create_dir_all(root.join(name))?;
    }
    let mut empty = start(0);
    finish(&mut empty);
    save(root, "pack", "empty-pack", &empty)?;
    let base = b"0123456789";
    let oid = GitOid::hash("blob", base);
    let delta = b"\x0a\x0f\x90\x0a\x05HELLO";
    let mut blob = start(1);
    entry(&mut blob, 3, base.len(), base, &[]);
    finish(&mut blob);
    save(root, "pack", "blob", &blob)?;
    let mut pack = start(2);
    entry(&mut pack, 3, base.len(), base, &[]);
    let distance = pack.len() as u64 - 12;
    entry(&mut pack, 6, delta.len(), delta, &ofs(distance));
    finish(&mut pack);
    save(root, "pack", "ofs-delta", &pack)?;
    let mut pack = start(2);
    entry(&mut pack, 7, delta.len(), delta, &oid.0);
    entry(&mut pack, 3, base.len(), base, &[]);
    finish(&mut pack);
    save(root, "pack", "forward-ref-delta", &pack)?;
    for mode in 0..5 {
        for (name, payload) in [
            ("valid", delta.as_slice()),
            (
                "overflow",
                b"\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\x7f".as_slice(),
            ),
            ("truncated", b"\x0a\x0a\x91".as_slice()),
            ("zero-opcode", b"\x0a\x00\x00".as_slice()),
        ] {
            let mut input = vec![mode];
            input.extend(payload);
            save(root, "pack_delta", &format!("mode-{mode}-{name}"), &input)?;
        }
    }
    for (name, raw) in [
        ("empty", b"".as_slice()),
        ("special", b"0000000100020004"),
        ("reserved", b"0003"),
        ("overlong", b"ffff"),
        ("truncated", b"0010abc"),
    ] {
        save(root, "pktline", name, raw)?;
    }
    let want = format!("want {oid} multi_ack_detailed side-band-64k ofs-delta\n");
    let mut v0 = vec![];
    write_packet(&mut v0, want.as_bytes())?;
    v0.extend(b"0000");
    write_packet(&mut v0, b"done\n")?;
    save(root, "pktline", "fetch-v0", &v0)?;
    let mut v2 = vec![];
    write_packet(&mut v2, b"command=fetch\n")?;
    v2.extend(b"0001");
    write_packet(&mut v2, format!("want {oid}\n").as_bytes())?;
    write_packet(&mut v2, b"filter blob:none\n")?;
    write_packet(&mut v2, b"done\n")?;
    v2.extend(b"0000");
    save(root, "pktline", "fetch-v2", &v2)?;
    let mut receive = vec![];
    write_packet(
        &mut receive,
        format!(
            "{} {oid} refs/heads/main\0report-status atomic ofs-delta\n",
            GitOid([0; 20])
        )
        .as_bytes(),
    )?;
    receive.extend(b"0000");
    receive.extend(blob);
    save(root, "pktline", "receive-pack", &receive)?;
    for selector in 0..47 {
        save(
            root,
            "protobuf",
            &format!("default-{selector:02}"),
            &[selector],
        )?;
    }
    let signature = p::Signature {
        name: b"A".to_vec(),
        email: b"a@b".to_vec(),
        timestamp: 0,
        tz: b"+0000".to_vec(),
    };
    let metadata = p::CommitMetadata {
        author: Some(signature.clone()),
        committer: Some(signature.clone()),
        message: b"fuzz".to_vec(),
    };
    let id = crate::api::seed_id().0.to_vec();
    macro_rules! native {
        ($n:expr,$name:expr,$q:expr) => {{
            let raw = $q.encode_to_vec();
            for mode in [0, 1, 2, 3] {
                let mut input = vec![$n + 17 * mode];
                input.extend(&raw);
                save(root, "native_api", &format!("{}-{mode}", $name), &input)?;
            }
        }};
    }
    native!(
        0,
        "changeset",
        p::ChangesetRequest {
            repo: b"repo".to_vec(),
            id: id.clone()
        }
    );
    native!(
        1,
        "root-tree",
        p::ChangesetRequest {
            repo: b"repo".to_vec(),
            id: id.clone()
        }
    );
    native!(
        2,
        "resolve-ref",
        p::RefRequest {
            repo: b"repo".to_vec(),
            name: b"refs/heads/main".to_vec()
        }
    );
    native!(
        3,
        "list-refs",
        p::ListRefsRequest {
            repo: b"repo".to_vec(),
            prefix: b"refs/".to_vec()
        }
    );
    native!(
        4,
        "log",
        p::LogRequest {
            repo: b"repo".to_vec(),
            start: id.clone(),
            limit: u32::MAX,
            ..Default::default()
        }
    );
    native!(
        5,
        "ancestor",
        p::CompareRequest {
            repo: b"repo".to_vec(),
            base: id.clone(),
            head: id.clone()
        }
    );
    native!(
        6,
        "merge-base",
        p::CompareRequest {
            repo: b"repo".to_vec(),
            base: id.clone(),
            head: id.clone()
        }
    );
    native!(
        7,
        "diff",
        p::DiffRequest {
            repo: b"repo".to_vec(),
            base: id.clone(),
            head: id.clone(),
            content: true,
            ..Default::default()
        }
    );
    for (selector, name) in [(8, "blame"), (9, "read-file"), (10, "list-dir")] {
        native!(
            selector,
            name,
            p::PathRequest {
                repo: b"repo".to_vec(),
                id: id.clone(),
                path: b"file".to_vec(),
                follow_copies: true,
                follow_moves: selector == 8,
            }
        );
    }
    native!(
        11,
        "find-oid",
        p::GitOidRequest {
            repo: b"repo".to_vec(),
            oid: oid.0.to_vec()
        }
    );
    native!(
        12,
        "create",
        p::CreateChangesetRequest {
            repo: b"repo".to_vec(),
            parents: vec![id.clone()],
            metadata: Some(metadata.clone()),
            changes: vec![p::FileChange {
                path: b"file".to_vec(),
                entry: None
            }]
        }
    );
    native!(
        13,
        "merge",
        p::MergeRequest {
            repo: b"repo".to_vec(),
            a: id.clone(),
            b: id.clone(),
            metadata: Some(metadata.clone())
        }
    );
    native!(
        14,
        "rebase",
        p::RebaseRequest {
            repo: b"repo".to_vec(),
            id: id.clone(),
            onto: id.clone(),
            committer: Some(signature.clone())
        }
    );
    native!(
        15,
        "update-ref",
        p::UpdateRefRequest {
            repo: b"repo".to_vec(),
            name: b"refs/heads/main".to_vec(),
            expected: Some(p::Ref {
                name: b"refs/heads/main".to_vec(),
                target: Some(id.clone()),
                version: 1
            }),
            target: Some(id.clone()),
            force: false
        }
    );
    native!(
        16,
        "land",
        p::LandRequest {
            repo: b"repo".to_vec(),
            id: id.clone(),
            name: b"refs/heads/main".to_vec(),
            idempotency_key: b"fuzz-key".to_vec(),
            max_attempts: 1,
            timeout_ms: 1,
            required_ancestor: id
        }
    );
    Ok(())
}
