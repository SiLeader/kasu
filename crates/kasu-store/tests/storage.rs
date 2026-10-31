use kasu_core::*;
use kasu_store::*;
use std::sync::{Arc, Barrier};

#[test]
fn oversized_local_frames_are_rejected_before_reading_or_decompression() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let id = blobs.put(b"small object").unwrap();
    let hex = id.to_string();
    let path = dir.path().join(&hex[..2]).join(&hex[2..]);
    // A sparse file exercises the on-disk limit without allocating a huge payload.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(MAX_COMPRESSED_OBJECT_SIZE as u64 + 1)
        .unwrap();
    for result in [blobs.get_compressed(id), blobs.get(id)] {
        assert!(
            matches!(result, Err(Error::Invalid(message)) if message == "compressed object too large")
        );
    }
}

#[test]
fn local_compressed_reads_detect_corrupt_and_misplaced_frames() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let id = blobs.put(b"original").unwrap();
    let other = blobs.put(b"different").unwrap();
    let hex = id.to_string();
    let path = dir.path().join(&hex[..2]).join(&hex[2..]);
    std::fs::write(&path, blobs.get_compressed(other).unwrap()).unwrap();
    assert!(
        matches!(blobs.get_compressed(id), Err(Error::Corrupt(message)) if message == "object hash mismatch")
    );
    std::fs::write(&path, b"not zstd").unwrap();
    assert!(blobs.get_compressed(id).is_err());
}

#[test]
fn immutable_and_compressed_objects_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("meta.db");
    let blobs = FsBlobStore::open(dir.path().join("objects")).unwrap();
    let meta = SqliteStore::open(&db).unwrap();
    meta.put_if_absent(b"key", b"value").unwrap();
    meta.put_if_absent(b"key", b"value").unwrap();
    assert!(meta.put_if_absent(b"key", b"other").is_err());
    let repo = Repository {
        meta: &meta,
        blobs: &blobs,
    };
    let id = repo.put_blob(b"bytes\xff").unwrap();
    assert_eq!(repo.blob(id).unwrap(), b"bytes\xff");
    let compressed = blobs.get_compressed(ObjectId(id.0)).unwrap();
    assert_eq!(decompress(&compressed).unwrap(), b"bytes\xff");
    assert!(
        blobs
            .put_compressed(ObjectId([0; 32]), &compressed)
            .is_err()
    );
    drop(meta);
    assert_eq!(
        SqliteStore::open(&db).unwrap().get(b"key").unwrap(),
        Some(b"value".to_vec())
    );
    assert_eq!(
        FsBlobStore::open(dir.path().join("objects"))
            .unwrap()
            .get(ObjectId(id.0))
            .unwrap(),
        b"bytes\xff"
    );
}
fn update(name: &[u8], expected: RefState, new: Option<ChangesetId>) -> RefUpdate {
    RefUpdate {
        name: name.to_vec(),
        expected,
        new,
    }
}
#[test]
fn atomic_batch_log_and_aba_protection() {
    let store = SqliteStore::memory().unwrap();
    let target = Some(ChangesetId([1; 32]));
    let absent = RefState::default();
    let first = RefState { target, version: 1 };
    assert_eq!(
        store
            .compare_and_swap(
                b"repo",
                &[update(b"main", absent, target)],
                b"actor\xff",
                12
            )
            .unwrap(),
        CasResult::Applied(vec![first])
    );
    assert!(matches!(
        store
            .compare_and_swap(
                b"repo",
                &[
                    update(b"other", absent, target),
                    update(b"main", absent, target)
                ],
                b"a",
                13
            )
            .unwrap(),
        CasResult::Mismatch { .. }
    ));
    assert_eq!(store.resolve(b"repo", b"other").unwrap(), absent);
    assert_eq!(store.tail(b"repo", 0, 100).unwrap().len(), 1);
    store
        .compare_and_swap(b"repo", &[update(b"main", first, None)], b"a", 14)
        .unwrap();
    let deleted = store.resolve(b"repo", b"main").unwrap();
    assert_eq!(
        deleted,
        RefState {
            target: None,
            version: 2
        }
    );
    assert!(matches!(
        store
            .compare_and_swap(b"repo", &[update(b"main", absent, target)], b"a", 15)
            .unwrap(),
        CasResult::Mismatch { .. }
    ));
    store
        .compare_and_swap(b"repo", &[update(b"main", deleted, target)], b"a", 16)
        .unwrap();
    let log = store.tail(b"repo", 0, 100).unwrap();
    assert_eq!(log.len(), 3);
    assert_eq!(log[0].actor, b"actor\xff");
    assert_eq!(log[1].old, target);
    assert_eq!(log[1].new, None);
    assert_eq!(
        store.tail(b"repo", log[0].seq, 1).unwrap(),
        vec![log[1].clone()]
    );
    assert!(store.list(b"repo", b"ma").unwrap().len() == 1);
    assert!(store.tail(b"other", 0, 100).unwrap().is_empty());
}
#[test]
fn independent_connections_race_one_winner_and_one_log() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("meta.db");
    SqliteStore::open(&db).unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "ALTER TABLE ref_log DROP COLUMN via; ALTER TABLE git_ref_log DROP COLUMN via;",
        )
        .unwrap();
    let barrier = Arc::new(Barrier::new(16));
    let workers: Vec<_> = (0..16)
        .map(|n| {
            let db = db.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = SqliteStore::open(db).unwrap();
                barrier.wait();
                store
                    .compare_and_swap(
                        b"repo",
                        &[update(
                            b"main",
                            RefState::default(),
                            Some(ChangesetId([n; 32])),
                        )],
                        b"a",
                        0,
                    )
                    .unwrap()
            })
        })
        .collect();
    let winners = workers
        .into_iter()
        .filter_map(|t| {
            if matches!(t.join().unwrap(), CasResult::Applied(_)) {
                Some(())
            } else {
                None
            }
        })
        .count();
    assert_eq!(winners, 1);
    assert_eq!(
        SqliteStore::open(db)
            .unwrap()
            .tail(b"repo", 0, 100)
            .unwrap()
            .len(),
        1
    );
}
