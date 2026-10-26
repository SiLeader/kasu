use kasu_core::*;
use proptest::prelude::*;

fn signature() -> Signature {
    Signature {
        name: b"a\xff".as_slice().into(),
        email: b"e".as_slice().into(),
        timestamp: -1,
        tz: b"+0900".as_slice().into(),
    }
}
fn empty() -> Changeset {
    Changeset {
        parents: vec![],
        author: signature(),
        committer: signature(),
        message: b"\xff\0message".as_slice().into(),
        changes: vec![],
        git_provenance: None,
    }
}
#[test]
fn root_changeset_golden() {
    let cs = empty();
    let expected = "6b6173752e6368616e67657365740001000000000000000261ff0000000165ffffffffffffffff000000052b303930300000000261ff0000000165ffffffffffffffff000000052b3039303000000009ff006d6573736167650000000000";
    assert_eq!(hex::encode(cs.encode()), expected);
    assert_eq!(
        Changeset::decode(&hex::decode(expected).unwrap()).unwrap(),
        cs
    );
    assert_eq!(
        cs.id().to_string(),
        "a9f24856ae4915367e6b049f8aa0e9b0d181650041e91a8ffead98f8fa14d082"
    );
}
#[test]
fn blob_git_golden() {
    assert_eq!(
        GitOid::hash("blob", b"hello\n").to_string(),
        "ce013625030ba8dba906f756967f9e9ca394464a"
    );
    assert_eq!(
        BlobId::hash(b"").to_string(),
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    );
}
#[test]
fn paths_and_structural_validation() {
    for path in [
        b"".as_slice(),
        b"/a",
        b"a/",
        b"a//b",
        b".",
        b"a/../b",
        b"a\0b",
    ] {
        assert!(RepoPath::new(path).is_err());
    }
    assert!(RepoPath::new(b"a/\xff".as_slice()).is_ok());
    let mut cs = empty();
    cs.changes = vec![
        FileChange::Delete {
            path: RepoPath::new(b"a".as_slice()).unwrap()
        };
        2
    ];
    assert!(cs.validate().is_err());
    assert!(Changeset::decode(&cs.encode()).is_err());
    let mut raw = empty().encode();
    raw.push(0);
    assert!(Changeset::decode(&raw).is_err());
    assert!(Entry::decode(&[4]).is_err());
}

proptest! {
    #[test]
    fn changeset_roundtrip(name in prop::collection::vec(any::<u8>(), 0..128), email in prop::collection::vec(any::<u8>(), 0..64), message in prop::collection::vec(any::<u8>(), 0..1024), timestamp in any::<i64>(), tz in prop::collection::vec(any::<u8>(), 0..16), ids in prop::collection::btree_set(any::<[u8;32]>(), 0..8), blob in any::<[u8;32]>(), git in any::<[u8;20]>(), raw in prop::option::of(any::<[u8;32]>()), executable in any::<bool>()) {
        let signature = Signature { name: name.into(), email: email.into(), timestamp, tz: tz.into() };
        let changes = vec![
            FileChange::Set { path: RepoPath::new(b"a\xff".as_slice()).unwrap(), entry: Entry::File { blob: BlobId(blob), executable } },
            FileChange::Set { path: RepoPath::new(b"b".as_slice()).unwrap(), entry: Entry::Symlink { blob: BlobId(blob) } },
            FileChange::Set { path: RepoPath::new(b"c".as_slice()).unwrap(), entry: Entry::Gitlink { commit: GitOid(git) } },
            FileChange::Delete { path: RepoPath::new(b"d".as_slice()).unwrap() },
        ];
        let cs = Changeset { parents: ids.into_iter().map(ChangesetId).collect(), author: signature.clone(), committer: signature, message: message.into(), changes, git_provenance: raw.map(|id| GitProvenance { raw_commit: ObjectId(id) }) };
        prop_assert_eq!(Changeset::decode(&cs.encode()).unwrap(), cs);
    }
    #[test]
    fn arbitrary_decoding_never_panics(raw in prop::collection::vec(any::<u8>(), 0..4096)) {
        let _ = Changeset::decode(&raw); let _ = Tree::decode(&raw); let _ = Entry::decode(&raw);
    }
    #[test]
    fn tree_roundtrip(names in prop::collection::btree_set("[a-z]{1,10}", 0..50), id in any::<[u8;32]>()) {
        let tree = Tree { entries: names.into_iter().map(|name| (BString::from(name), TreeEntry::SubTree(TreeId(id)))).collect() };
        prop_assert_eq!(Tree::decode(&tree.encode()).unwrap(), tree);
    }
}
