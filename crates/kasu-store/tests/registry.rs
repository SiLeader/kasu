use kasu_core::*;
use kasu_store::*;

fn name(value: &str) -> RepoName {
    RepoName::new(value).unwrap()
}
fn create(store: &dyn RepositoryStore, value: &str) -> RepositoryRecord {
    let record = RepositoryRecord::new(RepoKey::generate().unwrap(), name(value), b"test", 10);
    match store.create_repository(&record).unwrap() {
        RepositoryWrite::Applied(record) => record,
        other => panic!("create failed: {other:?}"),
    }
}
fn applied(write: RepositoryWrite) -> RepositoryRecord {
    match write {
        RepositoryWrite::Applied(record) => record,
        other => panic!("expected an applied write: {other:?}"),
    }
}
fn update(native: RefState, n: u8) -> RefUpdate {
    RefUpdate {
        name: b"refs/heads/main".to_vec(),
        expected: native,
        new: Some(ChangesetId([n; 32])),
    }
}

/// Behavior shared by every registry backend. `refs` is the ref store that
/// enforces the repository state inside its transactions, when there is one.
fn registry_contract(store: &dyn RepositoryStore, refs: Option<&dyn GitRefStore>, unique: &str) {
    let team = format!("{unique}/Infra");
    let record = create(store, &team);
    assert_eq!(record.version, 1);
    assert_eq!(record.head(), b"refs/heads/main");
    // Names are unique case-insensitively and keep their registered spelling.
    let duplicate = RepositoryRecord::new(
        RepoKey::generate().unwrap(),
        name(&team.to_ascii_lowercase()),
        b"test",
        10,
    );
    assert_eq!(
        store.create_repository(&duplicate).unwrap(),
        RepositoryWrite::NameTaken
    );
    let resolved = store
        .resolve_repository_name(&name(&team.to_ascii_uppercase()), 11)
        .unwrap()
        .unwrap();
    assert_eq!(resolved.key, record.key);
    assert!(!resolved.alias);
    assert_eq!(
        store
            .repository(&record.key)
            .unwrap()
            .unwrap()
            .name
            .as_str(),
        team
    );

    // Configuration changes are compare-and-swap on the record version.
    let mut next = record.clone();
    next.config_version = 1;
    next.config = br#"{"default":true}"#.to_vec();
    let updated = applied(
        store
            .update_repository(&next, "config", b"admin", 12)
            .unwrap(),
    );
    assert_eq!(updated.version, 2);
    assert!(matches!(
        store.update_repository(&next, "config", b"admin", 13).unwrap(),
        RepositoryWrite::Mismatch(Some(current)) if current.version == 2
    ));
    let mut renamed_key = updated.clone();
    renamed_key.name = name(&format!("{unique}/other"));
    assert!(
        store
            .update_repository(&renamed_key, "x", b"admin", 13)
            .is_err()
    );

    // Renaming keeps the old name as an alias until it expires; nobody else can
    // take an alias while it is live.
    let moved = format!("{unique}/platform");
    let renamed = applied(
        store
            .rename_repository(&record.key, 2, &name(&moved), Some(100), b"admin", 20)
            .unwrap(),
    );
    assert_eq!(renamed.name.as_str(), moved);
    let alias = store
        .resolve_repository_name(&name(&team), 21)
        .unwrap()
        .unwrap();
    assert!(alias.alias);
    assert_eq!(alias.key, record.key);
    let squatter = RepositoryRecord::new(RepoKey::generate().unwrap(), name(&team), b"test", 30);
    assert_eq!(
        store.create_repository(&squatter).unwrap(),
        RepositoryWrite::NameTaken
    );
    assert!(
        store
            .resolve_repository_name(&name(&team), 100)
            .unwrap()
            .is_none()
    );
    let mut late = squatter.clone();
    late.created_at = 101;
    let reused = applied(store.create_repository(&late).unwrap());
    assert_ne!(reused.key, record.key);
    // Listings return primary names only, in normalized order.
    let listed = store
        .list_repository_names(&format!("{unique}/"), "", 10)
        .unwrap();
    assert_eq!(
        listed.iter().map(|n| n.name.clone()).collect::<Vec<_>>(),
        vec![team.to_ascii_lowercase(), moved.clone()]
    );
    let page = store
        .list_repository_names(&format!("{unique}/"), &listed[0].name, 10)
        .unwrap();
    assert_eq!(page.len(), 1);

    // Writes are refused once a repository leaves Active, inside the ref
    // transaction itself, and Deleting releases every name.
    if let Some(refs) = refs {
        let key = renamed.key.as_bytes();
        assert!(matches!(
            refs.compare_and_swap(key, &[update(RefState::default(), 1)], b"t", 1)
                .unwrap(),
            CasResult::Applied(_)
        ));
        let mut read_only = renamed.clone();
        read_only.state = RepositoryState::ReadOnly;
        let read_only = applied(
            store
                .update_repository(&read_only, "state", b"a", 30)
                .unwrap(),
        );
        let current = refs.resolve(key, b"refs/heads/main").unwrap();
        assert!(matches!(
            refs.compare_and_swap(key, &[update(current, 2)], b"t", 2),
            Err(Error::Precondition(_))
        ));
        let mut deleting = read_only.clone();
        deleting.state = RepositoryState::Deleting;
        deleting.deleted_at = 31;
        let deleting = applied(
            store
                .update_repository(&deleting, "deleted", b"a", 31)
                .unwrap(),
        );
        assert!(matches!(
            refs.compare_and_swap(key, &[update(current, 2)], b"t", 3),
            Err(Error::NotFound(_))
        ));
        assert!(
            store
                .resolve_repository_name(&name(&moved), 32)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .resolve_repository_name(&name(&team), 32)
                .unwrap()
                .is_none()
                || {
                    // The squatter registered after expiry still owns the old name.
                    store
                        .resolve_repository_name(&name(&team), 32)
                        .unwrap()
                        .unwrap()
                        .key
                        == reused.key
                }
        );
        // A deleted repository can never become visible again.
        let mut revive = deleting.clone();
        revive.state = RepositoryState::Active;
        assert!(store.update_repository(&revive, "state", b"a", 33).is_err());
        store.purge_repository_refs(&deleting.key, false).unwrap();
        assert_eq!(
            refs.resolve(key, b"refs/heads/main").unwrap(),
            RefState::default()
        );
        assert_eq!(refs.tail(key, 0, 10).unwrap().len(), 1);
        store.purge_repository_refs(&deleting.key, true).unwrap();
        assert!(refs.tail(key, 0, 10).unwrap().is_empty());
        assert!(store.purge_repository_refs(&reused.key, false).is_err());
    }

    let events = store.tail_repositories(0, 1000).unwrap();
    let ours: Vec<_> = events
        .iter()
        .filter(|e| e.key == record.key)
        .map(|e| e.event.as_str())
        .collect();
    assert_eq!(ours[..3], ["created", "config", "renamed"]);
    assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));
    let after = events.last().unwrap().seq;
    assert!(store.tail_repositories(after, 10).unwrap().is_empty());
}

#[test]
fn memory_registry_follows_the_registry_contract() {
    registry_contract(&MemoryRepositoryStore::new(), None, "memory");
}
#[test]
fn sqlite_registry_and_ref_transactions_follow_the_registry_contract() {
    let store = SqliteStore::memory().unwrap();
    registry_contract(&store, Some(&store), "sqlite");
}
#[test]
fn sqlite_registry_survives_reopen_and_unregistered_keys_keep_legacy_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let key = {
        let store = SqliteStore::open(&path).unwrap();
        create(&store, "team/app").key
    };
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(
        store
            .resolve_repository_name(&name("team/app"), 0)
            .unwrap()
            .unwrap()
            .key,
        key
    );
    // A database written before the registry has no record for its key.
    assert!(matches!(
        store
            .compare_and_swap(b"legacy", &[update(RefState::default(), 1)], b"t", 1)
            .unwrap(),
        CasResult::Applied(_)
    ));
}
#[test]
fn legacy_memory_registry_uses_the_name_bytes_as_key() {
    let store = MemoryRepositoryStore::legacy(&name("demo")).unwrap();
    let resolved = store
        .resolve_repository_name(&name("demo"), 0)
        .unwrap()
        .unwrap();
    assert_eq!(resolved.key.as_bytes(), b"demo");
    assert!(resolved.key.is_legacy());
}
#[test]
fn prefix_deletion_removes_only_the_requested_keys_in_batches() {
    let store = SqliteStore::memory().unwrap();
    for n in 0..5u8 {
        store
            .put_if_absent(&[b"repo/a/", &[n][..]].concat(), b"x")
            .unwrap();
    }
    store.put_if_absent(b"repo/b/0", b"x").unwrap();
    store.put_if_absent(b"repo/a", b"x").unwrap();
    assert_eq!(store.delete_prefix(b"repo/a/", 3).unwrap(), 3);
    assert_eq!(store.delete_prefix(b"repo/a/", 3).unwrap(), 2);
    assert_eq!(store.delete_prefix(b"repo/a/", 3).unwrap(), 0);
    assert!(store.get(b"repo/b/0").unwrap().is_some());
    assert!(store.get(b"repo/a").unwrap().is_some());
    assert!(store.delete_prefix(b"", 1).is_err());
    assert_eq!(prefix_end(b"a\xff\xff"), Some(b"b".to_vec()));
    assert_eq!(prefix_end(b"\xff"), None);
}
#[test]
fn repository_names_follow_the_naming_rules() {
    for valid in ["a", "team-a/infra", "A.b_c-d/e/f/g", &"x".repeat(64)] {
        RepoName::new(valid).unwrap();
    }
    for invalid in [
        "",
        "a/b/c/d/e",
        "-",
        "a/-",
        ".",
        "..",
        "a/../b",
        "repo.git",
        "repo.GIT",
        "metrics",
        "Admin/x",
        "v1",
        "a//b",
        "/a",
        "a/",
        "a b",
        "ä",
        &"x".repeat(65),
    ] {
        assert!(RepoName::new(invalid).is_err(), "{invalid}");
    }
    let key = RepoKey::generate().unwrap();
    assert!(!key.is_legacy());
    assert_eq!(key.as_bytes().len(), 19);
    assert_ne!(key, RepoKey::generate().unwrap());
    assert!(RepoKey::from_bytes(key.as_bytes().to_vec()).is_ok());
    assert!(RepoKey::from_bytes(b"r1\0short".to_vec()).is_err());
    assert!(RepoKey::from_bytes(b"bad name".to_vec()).is_err());
}

#[cfg(feature = "postgres")]
#[test]
#[ignore = "requires KASU_TEST_POSTGRES"]
fn postgres_registry_and_ref_transactions_follow_the_registry_contract() {
    let mut options =
        PostgresOptions::new(std::env::var("KASU_TEST_POSTGRES").expect("KASU_TEST_POSTGRES"));
    options.allow_plaintext = true;
    let store = PostgresStore::connect(options).unwrap();
    let unique = format!(
        "pg{}x{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    registry_contract(&store, Some(&store), &unique);
    store
        .put_if_absent(format!("{unique}/a/1").as_bytes(), b"x")
        .unwrap();
    store
        .put_if_absent(format!("{unique}/b/1").as_bytes(), b"x")
        .unwrap();
    assert_eq!(
        store
            .delete_prefix(format!("{unique}/a/").as_bytes(), 10)
            .unwrap(),
        1
    );
    assert!(
        store
            .get(format!("{unique}/b/1").as_bytes())
            .unwrap()
            .is_some()
    );
}
