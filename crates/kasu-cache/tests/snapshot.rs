use kasu_cache::{DiskStore, Key, MemoryStore, Store};
use kasu_core::{Canonical, ObjectId, Tree};
use kasu_object::Kind;
use kasu_store::{BlobStore, FsBlobStore};
use std::sync::Arc;

#[test]
fn disk_reopen_and_snapshot_preserve_exact_frames_and_repo_scope() {
    let temp = tempfile::tempdir().unwrap();
    let store = DiskStore::open(&temp.path().join("source"), b"repo").unwrap();
    let raw = vec![
        (Kind::Tree, Tree::default().encode()),
        (Kind::Blob, b"snapshot blob".to_vec()),
    ];
    let mut expected = vec![];
    for (kind, raw) in raw {
        let key = Key {
            kind,
            id: ObjectId::hash(&raw),
        };
        let frame = Arc::new(kasu_store::compress(&raw).unwrap());
        store.put(key, frame.clone()).unwrap();
        expected.push((key, frame));
    }
    assert_eq!(store.usage().0, 2);
    let path = temp.path().join("snapshot.tar");
    assert_eq!(store.export_snapshot(&path).unwrap(), 2);
    assert!(store.export_snapshot(&path).is_err());
    let reopened = DiskStore::open(&temp.path().join("source"), b"repo").unwrap();
    assert_eq!(reopened.usage(), store.usage());
    assert!(DiskStore::open(&temp.path().join("source"), b"foreign").is_err());
    let restored = DiskStore::open(&temp.path().join("restored"), b"repo").unwrap();
    assert_eq!(restored.import_snapshot(&path, 1024 * 1024).unwrap(), 2);
    for (key, bytes) in expected {
        assert_eq!(
            restored.get(key).unwrap().unwrap().as_slice(),
            bytes.as_slice()
        );
        assert_eq!(
            reopened.get(key).unwrap().unwrap().as_slice(),
            bytes.as_slice()
        );
    }
    let foreign = DiskStore::open(&temp.path().join("foreign"), b"foreign").unwrap();
    assert!(foreign.import_snapshot(&path, 1024 * 1024).is_err());
    assert_eq!(foreign.usage(), (0, 0));
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
        let store = DiskStore::open(&temp.path().join(format!("target-{n}")), b"repo").unwrap();
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
    let store = DiskStore::open(&temp.path().join("limited"), b"repo").unwrap();
    assert!(store.import_snapshot(&path, 1).is_err());
    assert_eq!(store.usage(), (0, 0));
}

#[test]
fn lru_budget_and_disk_type_index_do_not_expose_unregistered_data() {
    let temp = tempfile::tempdir().unwrap();
    let disk = DiskStore::open(temp.path(), b"repo").unwrap();
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
    memory.bind(b"repo").unwrap();
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
