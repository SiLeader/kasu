use kasu_cache::{DiskStore, Key, MemoryStore, Store};
use kasu_core::{Canonical, ObjectId, Tree};
use kasu_object::Kind;
use kasu_store::{BlobStore, FsBlobStore};
use std::sync::Arc;

#[test]
fn disk_reopen_and_snapshot_preserve_exact_frames_and_repo_scope() {
    let temp = tempfile::tempdir().unwrap();
    let store = DiskStore::open(&temp.path().join("source")).unwrap();
    let raw = vec![
        (Kind::Tree, Tree::default().encode()),
        (Kind::Blob, b"snapshot blob".to_vec()),
        (Kind::Blob, b"other repository blob".to_vec()),
    ];
    let mut expected = vec![];
    for (n, (kind, raw)) in raw.into_iter().enumerate() {
        let key = Key {
            kind,
            id: ObjectId::hash(&raw),
        };
        let frame = Arc::new(kasu_store::compress(&raw).unwrap());
        store.put(key, frame.clone()).unwrap();
        store
            .add_member(if n < 2 { b"team/repo" } else { b"team/other" }, key)
            .unwrap();
        expected.push((key, frame));
    }
    store.identify(b"team/repo", "identity-a").unwrap();
    assert_eq!(store.usage().0, 3);
    let path = temp.path().join("snapshot.tar");
    assert_eq!(store.export_snapshot(&path).unwrap(), 3);
    assert!(store.export_snapshot(&path).is_err());
    let reopened = DiskStore::open(&temp.path().join("source")).unwrap();
    assert_eq!(reopened.usage(), store.usage());
    let restored = DiskStore::open(&temp.path().join("restored")).unwrap();
    assert_eq!(restored.import_snapshot(&path, 1024 * 1024).unwrap(), 3);
    for (n, (key, bytes)) in expected.iter().enumerate() {
        for store in [&restored, &reopened] {
            assert_eq!(
                store.get(*key).unwrap().unwrap().as_slice(),
                bytes.as_slice()
            );
            assert_eq!(store.member(b"team/repo", *key).unwrap(), n < 2);
            assert_eq!(store.member(b"team/other", *key).unwrap(), n == 2);
        }
    }
    assert_eq!(
        restored.identity(b"team/repo").unwrap().as_deref(),
        Some("identity-a")
    );
    // A snapshot of a repository set carries only frames confirmed for it.
    let subset = temp.path().join("subset.tar");
    assert_eq!(
        store
            .export_snapshot_for(&subset, &[b"Team/Other".to_vec()])
            .unwrap(),
        1
    );
    let partial = DiskStore::open(&temp.path().join("partial")).unwrap();
    assert_eq!(partial.import_snapshot(&subset, 1024 * 1024).unwrap(), 1);
    assert!(partial.member(b"team/other", expected[2].0).unwrap());
    assert!(partial.get(expected[0].0).unwrap().is_none());
    // A new identity for a name means another repository: forget its frames.
    restored.identify(b"team/repo", "identity-b").unwrap();
    assert!(!restored.member(b"team/repo", expected[0].0).unwrap());
    assert!(restored.member(b"team/other", expected[2].0).unwrap());
    assert!(
        store
            .export_snapshot_for(&subset, &[b"bad name".to_vec()])
            .is_err()
    );
}

fn archive(
    path: &std::path::Path,
    manifest: &serde_json::Value,
    entries: Vec<(String, Vec<u8>, bool)>,
) {
    let file = std::fs::File::create(path).unwrap();
    let mut tar = tar::Builder::new(file);
    let body = serde_json::to_vec(manifest).unwrap();
    let mut h = tar::Header::new_ustar();
    h.set_size(body.len() as u64);
    h.set_mode(0o600);
    h.set_cksum();
    tar.append_data(&mut h, "manifest.json", body.as_slice())
        .unwrap();
    for (name, bytes, link) in entries {
        let mut h = tar::Header::new_ustar();
        h.set_size(if link { 0 } else { bytes.len() as u64 });
        h.set_mode(0o600);
        h.set_entry_type(if link {
            tar::EntryType::Symlink
        } else {
            tar::EntryType::Regular
        });
        let raw = h.as_mut_bytes();
        raw[..name.len()].copy_from_slice(name.as_bytes());
        if link {
            h.set_link_name("../../outside").unwrap();
        }
        h.set_cksum();
        tar.append(&h, if link { &[][..] } else { bytes.as_slice() })
            .unwrap();
    }
    tar.finish().unwrap();
}
#[test]
fn invalid_snapshot_never_publishes_a_prefix_and_never_extracts_archive_paths() {
    let temp = tempfile::tempdir().unwrap();
    let first = ObjectId::hash(b"good");
    let second = ObjectId::hash(b"second");
    let a = kasu_store::compress(b"good").unwrap();
    let b = kasu_store::compress(b"second").unwrap();
    let manifest = serde_json::json!({"version":1,"repository":b"repo","entries":[{"kind":"blobs","id":first.to_string(),"size":a.len()},{"kind":"blobs","id":second.to_string(),"size":b.len()}]});
    for (n, name, bytes, link) in [
        (
            0,
            format!("objects/blobs/{second}.zst"),
            kasu_store::compress(b"hacked").unwrap(),
            false,
        ),
        (1, "../outside".into(), b.clone(), false),
        (2, format!("objects/blobs/{second}.zst"), b.clone(), true),
    ] {
        let path = temp.path().join(format!("bad-{n}.tar"));
        archive(
            &path,
            &manifest,
            vec![
                (format!("objects/blobs/{first}.zst"), a.clone(), false),
                (name, bytes, link),
            ],
        );
        let store = DiskStore::open(&temp.path().join(format!("target-{n}"))).unwrap();
        assert!(store.import_snapshot(&path, 1024 * 1024).is_err());
        assert_eq!(store.usage(), (0, 0));
        assert!(
            store
                .get(Key {
                    kind: Kind::Blob,
                    id: first
                })
                .unwrap()
                .is_none()
        );
    }
    assert!(!temp.path().join("outside").exists());
    let path = temp.path().join("valid-too-large.tar");
    archive(
        &path,
        &manifest,
        vec![
            (format!("objects/blobs/{first}.zst"), a, false),
            (format!("objects/blobs/{second}.zst"), b, false),
        ],
    );
    let store = DiskStore::open(&temp.path().join("limited")).unwrap();
    assert!(store.import_snapshot(&path, 1).is_err());
    assert_eq!(store.usage(), (0, 0));
}

#[test]
fn lru_budget_and_disk_type_index_do_not_expose_unregistered_data() {
    let temp = tempfile::tempdir().unwrap();
    let disk = DiskStore::open(temp.path()).unwrap();
    let raw = b"unindexed";
    let id = ObjectId::hash(raw);
    let blobs = FsBlobStore::open(temp.path().join("objects")).unwrap();
    blobs.put(raw).unwrap();
    assert!(
        disk.get(Key {
            kind: Kind::Blob,
            id
        })
        .unwrap()
        .is_none()
    );
    let a = Arc::new(kasu_store::compress(b"one").unwrap());
    let b = Arc::new(kasu_store::compress(b"two").unwrap());
    let memory = MemoryStore::new(a.len().max(b.len()));
    let ka = Key {
        kind: Kind::Blob,
        id: ObjectId::hash(b"one"),
    };
    let kb = Key {
        kind: Kind::Blob,
        id: ObjectId::hash(b"two"),
    };
    memory.put(ka, a).unwrap();
    memory.put(kb, b.clone()).unwrap();
    assert!(memory.get(ka).unwrap().is_none());
    assert_eq!(memory.get(kb).unwrap().unwrap().as_slice(), b.as_slice());
    assert!(memory.usage().1 <= b.len());
}
