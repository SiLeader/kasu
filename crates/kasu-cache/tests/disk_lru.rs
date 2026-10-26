use kasu_cache::{DiskLimits, DiskStore, Key, Store};
use kasu_core::{Canonical, ObjectId, Tree};
use kasu_object::Kind;
use kasu_store::{BlobStore, FsBlobStore};
use std::{
    fs,
    path::Path,
    sync::{Arc, Barrier},
    time::Duration,
};
fn object(raw: &[u8]) -> (Key, Arc<Vec<u8>>) {
    (
        Key {
            kind: Kind::Blob,
            id: ObjectId::hash(raw),
        },
        Arc::new(kasu_store::compress(raw).unwrap()),
    )
}
fn frame_path(root: &Path, id: ObjectId) -> std::path::PathBuf {
    let hex = id.to_string();
    root.join("objects").join(&hex[..2]).join(&hex[2..])
}
fn assert_disk(store: &DiskStore, root: &Path) {
    let stats = store.stats().unwrap();
    let catalog = store.catalog().unwrap();
    assert_eq!(stats.entries, catalog.len() as u64);
    let mut ids = std::collections::BTreeSet::new();
    let mut sum = 0;
    for (key, size) in catalog {
        let bytes = store.get(key).unwrap().unwrap();
        assert_eq!(size, bytes.len() as u64);
        kasu_object::validate(key.kind, key.id, &bytes).unwrap();
        if ids.insert(key.id) {
            sum += size;
        }
    }
    assert_eq!(sum, stats.bytes);
    assert!(sum <= stats.limits.bytes);
    assert!(stats.entries <= stats.limits.entries);
    let mut physical = 0;
    for prefix in fs::read_dir(root.join("objects")).unwrap() {
        for file in fs::read_dir(prefix.unwrap().path()).unwrap() {
            physical += file.unwrap().metadata().unwrap().len();
        }
    }
    assert_eq!(physical, sum, "no unaccounted payload files");
}
#[test]
fn byte_and_entry_limits_persist_lru_and_inflight_frames_survive_eviction() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (a, fa) = object(b"aaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let (b, fb) = object(b"bbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let (c, fc) = object(b"ccccccccccccccccccccccccccc");
    let limit = (fa.len() + fb.len()) as u64;
    let store = DiskStore::open_with_limits(
        root,
        DiskLimits {
            bytes: limit,
            entries: 10,
        },
    )
    .unwrap();
    store.put(a, fa.clone()).unwrap();
    store.put(b, fb).unwrap();
    let pinned = store.get(a).unwrap().unwrap();
    let reopened = DiskStore::open(root).unwrap();
    assert_eq!(reopened.limits().unwrap().bytes, limit);
    reopened.put(c, fc).unwrap();
    assert!(store.get(b).unwrap().is_none());
    assert!(!frame_path(root, b.id).exists());
    assert!(store.get(a).unwrap().is_some());
    assert_eq!(pinned.as_slice(), fa.as_slice());
    let tightened = DiskStore::open_with_limits(
        root,
        DiskLimits {
            bytes: limit,
            entries: 1,
        },
    )
    .unwrap();
    assert!(tightened.get(c).unwrap().is_none());
    assert!(tightened.get(a).unwrap().is_some());
    assert_eq!(tightened.stats().unwrap().evictions, 2);
    assert_disk(&tightened, root);
}
#[test]
fn shared_tree_blob_frame_counts_once_and_type_eviction_preserves_other_registration() {
    let temp = tempfile::tempdir().unwrap();
    let raw = Tree::default().encode();
    let (blob, frame) = object(&raw);
    let tree = Key {
        kind: Kind::Tree,
        ..blob
    };
    let store = DiskStore::open_with_limits(
        temp.path(),
        DiskLimits {
            bytes: frame.len() as u64,
            entries: 2,
        },
    )
    .unwrap();
    store.put(tree, frame.clone()).unwrap();
    store.put(blob, frame.clone()).unwrap();
    assert_eq!(store.usage(), (2, frame.len()));
    let one = DiskStore::open_with_limits(
        temp.path(),
        DiskLimits {
            bytes: frame.len() as u64,
            entries: 1,
        },
    )
    .unwrap();
    assert_eq!(one.usage(), (1, frame.len()));
    assert_eq!(one.stats().unwrap().evicted_bytes, 0);
    // Registering another type at a one-entry limit may evict the old marker, retaining the exact frame.
    one.put(blob, frame.clone()).unwrap();
    assert!(one.get(tree).unwrap().is_none());
    assert_eq!(one.get(blob).unwrap().unwrap(), frame);
    assert_disk(&one, temp.path());
}
#[test]
fn oversized_frames_are_served_uncached_without_evicting_existing_entries() {
    let temp = tempfile::tempdir().unwrap();
    let (a, fa) = object(b"small");
    let store = DiskStore::open_with_limits(
        temp.path(),
        DiskLimits {
            bytes: fa.len() as u64,
            entries: 1,
        },
    )
    .unwrap();
    store.put(a, fa.clone()).unwrap();
    let raw: Vec<_> = (0..4096u32).flat_map(u32::to_le_bytes).collect();
    let (b, fb) = object(&raw);
    assert!(fb.len() > fa.len());
    store.put(b, fb).unwrap();
    assert!(store.get(b).unwrap().is_none());
    assert_eq!(store.get(a).unwrap().unwrap(), fa);
    assert_eq!(store.stats().unwrap().admission_skips, 1);
    assert_eq!(store.stats().unwrap().evictions, 0);
    assert_disk(&store, temp.path());
}
#[test]
fn legacy_markers_migrate_but_orphans_and_interrupted_owned_staging_are_removed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("index/blobs")).unwrap();
    fs::create_dir_all(root.join("index/trees")).unwrap();
    fs::write(
        root.join("scope.json"),
        serde_json::to_vec(&serde_json::json!({"version":1,"repository":b"repo"})).unwrap(),
    )
    .unwrap();
    let blobs = FsBlobStore::open(root.join("objects")).unwrap();
    let (a, fa) = object(b"legacy");
    blobs.put_compressed(a.id, &fa).unwrap();
    fs::write(
        root.join("index/blobs").join(a.id.to_string()),
        (fa.len() as u64).to_le_bytes(),
    )
    .unwrap();
    let orphan = blobs.put(b"not indexed").unwrap();
    let missing = ObjectId::hash(b"missing");
    fs::write(
        root.join("index/blobs").join(missing.to_string()),
        123u64.to_le_bytes(),
    )
    .unwrap();
    fs::create_dir(root.join(".snapshot-import-abcdef")).unwrap();
    fs::write(root.join(".snapshot-import-abcdef/file"), b"unfinished").unwrap();
    fs::write(root.join("index/blobs/.kasu-index-abcdef"), b"partial").unwrap();
    fs::write(root.join("operator-note"), b"preserve").unwrap();
    let store = DiskStore::open(root).unwrap();
    assert_eq!(store.get(a).unwrap().unwrap(), fa);
    assert!(
        store
            .get(Key {
                kind: Kind::Blob,
                id: missing
            })
            .unwrap()
            .is_none()
    );
    assert!(!frame_path(root, orphan).exists());
    assert!(!root.join(".snapshot-import-abcdef").exists());
    assert_eq!(fs::read(root.join("operator-note")).unwrap(), b"preserve");
    assert!(store.stats().unwrap().repairs >= 4);
    assert_disk(&store, root);
}
#[test]
fn valid_snapshot_import_pins_every_manifest_key_and_capacity_rejection_has_no_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let raw = Tree::default().encode();
    let (shared, frame) = object(&raw);
    let tree = Key {
        kind: Kind::Tree,
        ..shared
    };
    let (other, second) = object(b"another file");
    let source = DiskStore::open(&temp.path().join("source")).unwrap();
    source.put(shared, frame.clone()).unwrap();
    source.put(other, second.clone()).unwrap();
    let archive = temp.path().join("seed.tar");
    source.export_snapshot(&archive).unwrap();
    let limit = (frame.len() + second.len()) as u64;
    let dest = DiskStore::open_with_limits(
        &temp.path().join("dest"),
        DiskLimits {
            bytes: limit,
            entries: 2,
        },
    )
    .unwrap();
    dest.put(tree, frame.clone()).unwrap();
    dest.put(shared, frame.clone()).unwrap();
    assert_eq!(dest.import_snapshot(&archive, 1024 * 1024).unwrap(), 2);
    assert_eq!(dest.get(shared).unwrap().unwrap(), frame);
    assert_eq!(dest.get(other).unwrap().unwrap(), second);
    assert!(dest.get(tree).unwrap().is_none());
    assert_disk(&dest, &temp.path().join("dest"));
    let tiny = DiskStore::open_with_limits(
        &temp.path().join("tiny"),
        DiskLimits {
            bytes: limit,
            entries: 1,
        },
    )
    .unwrap();
    let (old, oldframe) = object(b"old");
    tiny.put(old, oldframe.clone()).unwrap();
    assert_eq!(
        tiny.import_snapshot(&archive, 1024 * 1024)
            .unwrap_err()
            .code(),
        tonic::Code::ResourceExhausted
    );
    assert_eq!(tiny.get(old).unwrap().unwrap(), oldframe);
    assert!(tiny.get(shared).unwrap().is_none());
    assert_eq!(tiny.stats().unwrap().evictions, 0);
}
#[test]
fn concurrent_instances_and_snapshots_share_one_budget_without_wrong_frames() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let first = DiskStore::open_with_limits(
        &root,
        DiskLimits {
            bytes: 4096,
            entries: 4,
        },
    )
    .unwrap();
    let stores: Vec<_> = (0..4).map(|_| DiskStore::open(&root).unwrap()).collect();
    let barrier = Barrier::new(4);
    std::thread::scope(|scope| {
        for (n, store) in stores.iter().enumerate() {
            let barrier = &barrier;
            let root = temp.path();
            scope.spawn(move || {
                barrier.wait();
                for round in 0..20 {
                    let raw = format!("thread {n} frame {round}");
                    let (key, frame) = object(raw.as_bytes());
                    store.put(key, frame.clone()).unwrap();
                    if let Some(found) = store.get(key).unwrap() {
                        assert_eq!(found, frame);
                    }
                    if round % 5 == 0 {
                        let path = root.join(format!("snapshot-{n}-{round}.tar"));
                        store.export_snapshot(&path).unwrap();
                    }
                }
            });
        }
    });
    assert_disk(&first, &root);
    assert_eq!(first.stats().unwrap().entries, 4);
    assert!(first.stats().unwrap().evictions > 0);
}
#[test]
fn managed_snapshot_retention_ignores_user_files_and_diagnostics_do_not_wait_for_disk_lock() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(DiskStore::open(temp.path()).unwrap());
    let folder = temp.path().join("snapshots");
    fs::create_dir(&folder).unwrap();
    for n in 1..=4 {
        store
            .export_snapshot(&folder.join(format!("kasu-snapshot-v1-{n}.tar")))
            .unwrap();
    }
    fs::write(folder.join("user.tar"), b"keep").unwrap();
    fs::write(folder.join("123.tar"), b"legacy").unwrap();
    fs::write(folder.join(".kasu-snapshot-write-abcdef"), b"partial").unwrap();
    assert_eq!(store.prune_snapshots(2).unwrap(), 3);
    assert!(!folder.join("kasu-snapshot-v1-1.tar").exists());
    assert!(folder.join("kasu-snapshot-v1-4.tar").exists());
    assert_eq!(fs::read(folder.join("user.tar")).unwrap(), b"keep");
    assert!(folder.join("123.tar").exists());
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp.path().join(".cache-lru.lock"))
        .unwrap();
    lock.lock().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let clone = store.clone();
    let worker =
        std::thread::spawn(move || tx.send((clone.usage(), clone.cached_stats())).unwrap());
    let value = rx.recv_timeout(Duration::from_secs(1));
    drop(lock);
    worker.join().unwrap();
    assert!(value.is_ok(), "diagnostics must not wait for disk access");
}
#[cfg(unix)]
#[test]
fn symlinked_managed_directories_and_files_fail_without_touching_targets() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("important"), b"preserve").unwrap();
    let root = temp.path().join("cache");
    fs::create_dir_all(root.join("index")).unwrap();
    symlink(&outside, root.join("index/blobs")).unwrap();
    assert!(DiskStore::open(&root).is_err());
    assert_eq!(fs::read(outside.join("important")).unwrap(), b"preserve");
}

#[test]
fn interrupted_publication_and_eviction_are_reconciled_before_the_next_operation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let store = DiskStore::open(root).unwrap();
    let (a, fa) = object(b"committed");
    store.put(a, fa).unwrap();
    let db = rusqlite::Connection::open(root.join("cache-lru-v1.sqlite3")).unwrap();
    let (b, fb) = object(b"frame before marker");
    let blobs = FsBlobStore::open(root.join("objects")).unwrap();
    blobs.put_compressed(b.id, &fb).unwrap();
    db.execute("UPDATE config SET dirty=1", []).unwrap();
    assert!(store.get(b).unwrap().is_none());
    assert!(!frame_path(root, b.id).exists());
    assert!(store.get(a).unwrap().is_some());
    // Marker publication can survive while its catalog transaction rolls back.
    blobs.put_compressed(b.id, &fb).unwrap();
    fs::write(
        root.join("index/blobs").join(b.id.to_string()),
        (fb.len() as u64).to_le_bytes(),
    )
    .unwrap();
    db.execute("UPDATE config SET dirty=1", []).unwrap();
    assert_eq!(store.get(b).unwrap().unwrap(), fb);
    // Eviction first removes visibility, then the frame, then commits SQL metadata.
    fs::remove_file(root.join("index/blobs").join(a.id.to_string())).unwrap();
    db.execute("UPDATE config SET dirty=1", []).unwrap();
    assert!(store.get(a).unwrap().is_none());
    assert!(!frame_path(root, a.id).exists());
    assert_disk(&store, root);
}
#[test]
fn lru_clock_and_cumulative_counters_handle_the_signed_integer_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let store = DiskStore::open_with_limits(
        temp.path(),
        DiskLimits {
            bytes: 4096,
            entries: 2,
        },
    )
    .unwrap();
    let (a, fa) = object(b"a");
    let (b, fb) = object(b"b");
    let (c, fc) = object(b"c");
    store.put(a, fa).unwrap();
    store.put(b, fb).unwrap();
    let db = rusqlite::Connection::open(temp.path().join("cache-lru-v1.sqlite3")).unwrap();
    db.execute("UPDATE config SET tick=9223372036854775807,evictions=9223372036854775807,evicted_bytes=9223372036854775807",[]).unwrap();
    store.get(a).unwrap();
    store.put(c, fc).unwrap();
    assert!(store.get(b).unwrap().is_none());
    assert!(store.get(a).unwrap().is_some());
    assert_eq!(store.stats().unwrap().evictions, i64::MAX as u64);
    assert_eq!(store.stats().unwrap().evicted_bytes, i64::MAX as u64);
}

#[test]
#[ignore = "subprocess fixture invoked by kill_during_a_physical_mutation_recovers_a_bounded_valid_cache"]
fn disk_writer_child() {
    let Some(root) = std::env::var_os("KASU_DISK_CRASH_CHILD") else {
        return;
    };
    let store = DiskStore::open(Path::new(&root)).unwrap();
    for n in 0..5000 {
        let (key, frame) = object(format!("crash writer {n}").as_bytes());
        store.put(key, frame).unwrap();
    }
}
#[test]
fn kill_during_a_physical_mutation_recovers_a_bounded_valid_cache() {
    use rustix::process::{Pid, Signal, WaitOptions, kill_process, waitpid};
    use std::collections::BTreeSet;
    use std::process::{Command, Stdio};
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let store = DiskStore::open_with_limits(
        root,
        DiskLimits {
            bytes: 4096,
            entries: 2,
        },
    )
    .unwrap();
    for raw in [b"seed a", b"seed b"] {
        let (key, frame) = object(raw);
        store.put(key, frame).unwrap();
    }
    let mut child = Child(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "disk_writer_child", "--nocapture"])
            .env("KASU_DISK_CRASH_CHILD", root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = Pid::from_raw(child.0.id() as i32).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let db = rusqlite::Connection::open(root.join("cache-lru-v1.sqlite3")).unwrap();
    db.busy_timeout(Duration::from_millis(50)).unwrap();
    let captured = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "did not observe a filesystem mutation before the catalog commit"
        );
        assert!(child.0.try_wait().unwrap().is_none());
        let dirty: bool = db
            .query_row("SELECT dirty FROM config", [], |r| r.get(0))
            .unwrap();
        if !dirty {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        kill_process(pid, Signal::STOP).unwrap();
        let (_, status) = waitpid(Some(pid), WaitOptions::UNTRACED).unwrap().unwrap();
        assert!(status.stopped());
        let dirty: bool = db
            .query_row("SELECT dirty FROM config", [], |r| r.get(0))
            .unwrap();
        let sql: BTreeSet<String> = db
            .prepare("SELECT id FROM frames")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let markers: BTreeSet<String> = fs::read_dir(root.join("index/blobs"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        let mut physical = BTreeSet::new();
        for prefix in fs::read_dir(root.join("objects")).unwrap() {
            let prefix = prefix.unwrap();
            let name = prefix.file_name().to_string_lossy().into_owned();
            for file in fs::read_dir(prefix.path()).unwrap() {
                let file = file.unwrap();
                let leaf = file.file_name().to_string_lossy().into_owned();
                if !leaf.starts_with('.') {
                    physical.insert(format!("{name}{leaf}"));
                }
            }
        }
        if dirty && (sql != markers || sql != physical) {
            break serde_json::json!({"catalog_ids":sql,"marker_ids":markers,"physical_ids":physical,"dirty":true});
        }
        kill_process(pid, Signal::CONT).unwrap();
    };
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    let recovered = DiskStore::open(root).unwrap();
    assert_disk(&recovered, root);
    let integrity: String = db
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let dirty: bool = db
        .query_row("SELECT dirty FROM config", [], |r| r.get(0))
        .unwrap();
    assert!(!dirty);
    if let Some(path) = std::env::var_os("KASU_DISK_CAPTURE") {
        let path = Path::new(&path);
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("disk-crash.json"),serde_json::to_vec_pretty(&serde_json::json!({"interrupted":captured,"recovered_entries":recovered.stats().unwrap().entries,"recovered_bytes":recovered.stats().unwrap().bytes,"integrity":"ok","dirty":false})).unwrap()).unwrap();
    }
}

#[test]
fn first_compressed_frame_is_preserved_and_snapshot_budget_uses_its_actual_size() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let raw = b"one immutable object with another valid zstd representation";
    let (key, plain) = object(raw);
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
    encoder.include_checksum(true).unwrap();
    encoder.write_all(raw).unwrap();
    let first = Arc::new(encoder.finish().unwrap());
    assert!(first.len() > plain.len());
    let (second, other) = object(b"another object");
    let source = DiskStore::open(&temp.path().join("source")).unwrap();
    source.put(key, plain.clone()).unwrap();
    source.put(second, other.clone()).unwrap();
    let snapshot = temp.path().join("seed.tar");
    source.export_snapshot(&snapshot).unwrap();
    let dest = DiskStore::open_with_limits(
        &temp.path().join("dest"),
        DiskLimits {
            bytes: (first.len() + other.len() - 1) as u64,
            entries: 2,
        },
    )
    .unwrap();
    dest.put(key, first.clone()).unwrap();
    dest.put(key, plain).unwrap();
    assert_eq!(dest.get(key).unwrap().unwrap(), first);
    assert_eq!(
        dest.import_snapshot(&snapshot, 1024 * 1024)
            .unwrap_err()
            .code(),
        tonic::Code::ResourceExhausted
    );
    assert_eq!(dest.get(key).unwrap().unwrap(), first);
    assert!(dest.get(second).unwrap().is_none());
    assert_eq!(dest.stats().unwrap().evictions, 0);
}
#[test]
fn invalid_limits_do_not_create_a_cache() {
    let temp = tempfile::tempdir().unwrap();
    for (n, limits) in [
        DiskLimits {
            bytes: 0,
            entries: 1,
        },
        DiskLimits {
            bytes: 1,
            entries: 0,
        },
        DiskLimits {
            bytes: (1 << 50) + 1,
            entries: 1,
        },
        DiskLimits {
            bytes: 1,
            entries: 1_000_001,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let root = temp.path().join(n.to_string());
        assert!(DiskStore::open_with_limits(&root, limits).is_err());
        assert!(!root.exists());
    }
}

#[test]
fn initialization_cannot_adopt_unrelated_files_or_authoritative_repository_storage() {
    let temp = tempfile::tempdir().unwrap();
    let unknown = temp.path().join("unknown");
    fs::create_dir(&unknown).unwrap();
    fs::write(unknown.join("operator-data"), b"preserve").unwrap();
    assert!(DiskStore::open(&unknown).is_err());
    assert!(!unknown.join("scope.json").exists());
    assert_eq!(
        fs::read(unknown.join("operator-data")).unwrap(),
        b"preserve"
    );
    let authority = temp.path().join("authority");
    fs::create_dir(&authority).unwrap();
    let metadata = kasu_store::SqliteStore::open(authority.join("meta.db")).unwrap();
    drop(metadata);
    let blobs = FsBlobStore::open(authority.join("objects")).unwrap();
    let id = blobs.put(b"authoritative bytes").unwrap();
    // A scope left by an older cache binary must not make repository storage eligible for collection.
    fs::write(
        authority.join("scope.json"),
        serde_json::to_vec(&serde_json::json!({"version":1,"repository":b"repo"})).unwrap(),
    )
    .unwrap();
    assert!(DiskStore::open(&authority).is_err());
    assert_eq!(blobs.get(id).unwrap(), b"authoritative bytes");
}
