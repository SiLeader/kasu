use crate::{
    Remote, Result, Snapshot, SparseProfile,
    filesystem::{Root, Scanned, Stamp},
    invalid, prefix, validate_work_path,
};
use kasu_core::{Canonical, ChangesetId, Entry, ObjectId};
use kasu_store::FsBlobStore;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tracked {
    pub path: Vec<u8>,
    pub entry: Vec<u8>,
    pub stamp: Stamp,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    pub generation: u64,
    pub server: String,
    pub objects: String,
    pub repo: Vec<u8>,
    pub base: Vec<u8>,
    pub reference: Vec<u8>,
    pub draft: bool,
    pub sparse: SparseProfile,
    pub files: Vec<Tracked>,
    pub land_key: Option<Vec<u8>>,
    #[serde(default)]
    pub land_outcome: Option<i32>,
}
impl State {
    pub fn id(&self) -> Result<ChangesetId> {
        Ok(ChangesetId(
            self.base
                .as_slice()
                .try_into()
                .map_err(|_| invalid("invalid working copy base"))?,
        ))
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        let mut result = BTreeMap::new();
        for file in &self.files {
            validate_work_path(&file.path)?;
            if result
                .insert(file.path.clone(), Entry::decode(&file.entry)?)
                .is_some()
            {
                return Err(invalid("duplicate tracked path"));
            }
        }
        Ok(result)
    }
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct LocalChange {
    pub path: Vec<u8>,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
}
pub struct WorkingCopy {
    pub path: PathBuf,
    root: Root,
    _lock: File,
    pub cache: FsBlobStore,
    pub state: State,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Transition {
    path: Vec<u8>,
    old: Option<Vec<u8>>,
    new: Option<Vec<u8>>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Journal {
    old_generation: u64,
    new_generation: u64,
    changes: Vec<Transition>,
}
fn backup(path: &[u8]) -> Vec<u8> {
    format!(".kasu/transaction/backup/{}", ObjectId::hash(path)).into_bytes()
}
fn staged(path: &[u8]) -> Vec<u8> {
    format!(".kasu/transaction/stage/{}", ObjectId::hash(path)).into_bytes()
}
impl WorkingCopy {
    pub fn find(start: &Path) -> Result<Self> {
        let mut path = start.canonicalize()?;
        loop {
            if path.join(".kasu").symlink_metadata().is_ok() {
                return Self::open(&path);
            }
            if !path.pop() {
                return Err(invalid("not in a kasu working copy"));
            }
        }
    }
    pub fn open(path: &Path) -> Result<Self> {
        let path = path.canonicalize()?;
        let root = Root::open(&path)?;
        let lock = root.lock()?;
        let state: State = serde_json::from_slice(&root.read(b".kasu/state.json")?)?;
        if state.version != 1 {
            return Err(invalid("unsupported working copy version"));
        }
        state.id()?;
        state.sparse.validate()?;
        state.snapshot()?;
        root.mkdir(b".kasu/objects")?;
        let cache = FsBlobStore::open(path.join(".kasu/objects"))?;
        let mut copy = Self {
            path,
            root,
            _lock: lock,
            cache,
            state,
        };
        copy.recover()?;
        Ok(copy)
    }
    pub async fn checkout(
        remote: &mut Remote,
        server: String,
        objects: String,
        reference: &[u8],
        destination: &Path,
        sparse: SparseProfile,
    ) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let destination = std::path::absolute(destination)?;
        if destination.symlink_metadata().is_ok() {
            return Err(invalid("checkout destination already exists"));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| invalid("checkout needs a directory name"))?;
        let temp = tempfile::Builder::new()
            .prefix(".kasu-checkout-")
            .tempdir_in(parent)?;
        let root = Root::open(temp.path())?;
        root.mkdir(b".kasu")?;
        root.mkdir(b".kasu/objects")?;
        let lock = root.lock()?;
        let cache = FsBlobStore::open(temp.path().join(".kasu/objects"))?;
        let (id, reference) = remote.resolve(reference).await?;
        let snapshot = remote.snapshot(&cache, id, &sparse).await?;
        let state = State {
            version: 1,
            generation: 0,
            server,
            objects,
            repo: remote.repo.clone(),
            base: id.0.to_vec(),
            reference: reference.unwrap_or_default(),
            draft: false,
            sparse,
            files: vec![],
            land_key: None,
            land_outcome: None,
        };
        let mut copy = Self {
            path: temp.path().to_owned(),
            root,
            _lock: lock,
            cache,
            state,
        };
        copy.save()?;
        copy.transition(snapshot, copy.state.clone())?;
        drop(copy);
        let parent_root = Root::open(parent)?;
        parent_root.rename(
            temp.path().file_name().unwrap().as_bytes(),
            destination.file_name().unwrap().as_bytes(),
            false,
        )?;
        let _ = temp.keep();
        Self::open(&destination)
    }
    pub async fn remote(&self, token: Option<&str>) -> Result<Remote> {
        Remote::connect(
            &self.state.server,
            &self.state.objects,
            self.state.repo.clone(),
            token,
        )
        .await
    }
    pub fn save(&mut self) -> Result<()> {
        if self.root.kind(b".kasu/state.next")?.is_some() {
            self.root.remove(b".kasu/state.next", false)?;
        }
        self.root.write(
            b".kasu/state.next",
            &serde_json::to_vec(&self.state)?,
            true,
            false,
        )?;
        self.root
            .rename(b".kasu/state.next", b".kasu/state.json", true)
    }
    pub fn scan(&self) -> Result<Scanned> {
        self.root
            .scan(&self.state.sparse, &self.state.snapshot()?, &self.cache)
    }
    pub fn changes(&self) -> Result<Vec<LocalChange>> {
        let before = self.state.snapshot()?;
        let after = self.scan()?;
        Ok(before
            .keys()
            .chain(after.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|path| {
                let a = before.get(&path);
                let b = after.get(&path).map(|v| &v.0);
                (a != b).then(|| LocalChange {
                    path,
                    before: a.map(Canonical::encode),
                    after: b.map(Canonical::encode),
                })
            })
            .collect())
    }
    pub fn require_clean(&self) -> Result<()> {
        if !self.changes()?.is_empty() {
            return Err(invalid(
                "working copy has local changes; commit them before updating",
            ));
        }
        Ok(())
    }
    pub fn record_scanned(&mut self, scanned: Scanned) -> Result<()> {
        self.state.files = scanned
            .into_iter()
            .map(|(path, (entry, stamp))| Tracked {
                path,
                entry: entry.encode(),
                stamp,
            })
            .collect();
        self.state.generation = self
            .state
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("working copy generation exhausted"))?;
        self.save()
    }
    pub fn conflict(&self, response: &impl serde::Serialize) -> Result<()> {
        self.root.write(
            b".kasu/conflicts.json",
            &serde_json::to_vec_pretty(response)?,
            false,
            false,
        )
    }
    fn cleanup(&self) -> Result<()> {
        let stage = self.path.join(".kasu/transaction");
        if stage.symlink_metadata().is_ok() {
            std::fs::remove_dir_all(stage)?;
        }
        if self.root.kind(b".kasu/transaction.json")?.is_some() {
            self.root.remove(b".kasu/transaction.json", false)?;
        }
        Ok(())
    }
    fn recover(&mut self) -> Result<()> {
        if self.root.kind(b".kasu/transaction.json")?.is_none() {
            return self.cleanup();
        }
        let journal: Journal = serde_json::from_slice(&self.root.read(b".kasu/transaction.json")?)?;
        let mut seen = BTreeSet::new();
        for change in &journal.changes {
            validate_work_path(&change.path)?;
            if !seen.insert(&change.path) {
                return Err(invalid("duplicate journal path"));
            }
            for entry in [&change.old, &change.new].into_iter().flatten() {
                Entry::decode(entry)?;
            }
        }
        if journal.new_generation
            != journal
                .old_generation
                .checked_add(1)
                .ok_or_else(|| invalid("invalid journal generation"))?
        {
            return Err(invalid("invalid journal generation"));
        }
        if self.state.generation == journal.new_generation {
            return self.cleanup();
        }
        if self.state.generation != journal.old_generation {
            return Err(invalid("journal does not match working copy state"));
        }
        for change in &journal.changes {
            if self.root.kind(&backup(&change.path))?.is_some()
                && !change.old.as_ref().is_some_and(|raw| {
                    Entry::decode(raw).is_ok_and(|entry| {
                        self.root
                            .matches(&backup(&change.path), &entry, &self.cache)
                            .unwrap_or(false)
                    })
                })
            {
                return Err(invalid("working copy backup is corrupt; recovery stopped"));
            }
        }
        let mut paths: Vec<_> = journal.changes.iter().collect();
        paths.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        for change in paths {
            if let Some(raw) = &change.new
                && self.root.kind(&change.path)?.is_some()
                && self.root.kind(&staged(&change.path))?.is_none()
                && (change.old.is_none() || self.root.kind(&backup(&change.path))?.is_some())
            {
                let new = Entry::decode(raw)?;
                if self.root.matches(&change.path, &new, &self.cache)? {
                    self.root.remove_leaf(&change.path, &new)?;
                } else if change.old.as_ref().is_some_and(|raw| {
                    Entry::decode(raw).is_ok_and(|entry| {
                        self.root
                            .matches(&change.path, &entry, &self.cache)
                            .unwrap_or(false)
                    })
                }) || (self.root.kind(&change.path)?
                    == Some(rustix::fs::FileType::Directory)
                    && journal.changes.iter().any(|other| {
                        other.old.is_some()
                            && other.path != change.path
                            && prefix(&change.path, &other.path)
                    }))
                {
                } else {
                    return Err(invalid(
                        "file changed during interrupted update; recovery preserves it and its backup",
                    ));
                }
            }
        }
        for change in &journal.changes {
            if self.root.kind(&backup(&change.path))?.is_some() {
                let old = Entry::decode(change.old.as_ref().unwrap())?;
                if !self.root.matches(&change.path, &old, &self.cache)? {
                    if self.root.kind(&change.path)?.is_some() {
                        return Err(invalid("path blocks recovery; backup preserved"));
                    }
                    self.root
                        .rename(&backup(&change.path), &change.path, false)?;
                }
            }
        }
        self.cleanup()
    }
    pub fn transition(&mut self, snapshot: Snapshot, mut state: State) -> Result<()> {
        self.require_clean()?;
        let old = self.state.snapshot()?;
        for path in snapshot.keys() {
            validate_work_path(path)?;
        }
        let observed = self.root.scan(&state.sparse, &old, &self.cache)?;
        if observed.keys().any(|path| !old.contains_key(path)) {
            return Err(invalid("untracked files block sparse/update operation"));
        }
        for path in snapshot.keys() {
            if self.root.kind(path)? == Some(rustix::fs::FileType::Directory)
                && !old.keys().any(|old| prefix(path, old))
            {
                return Err(invalid("untracked directory blocks update"));
            }
        }
        let changes = old
            .keys()
            .chain(snapshot.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|path| {
                let a = old.get(&path);
                let b = snapshot.get(&path);
                (a != b).then(|| Transition {
                    path,
                    old: a.map(Canonical::encode),
                    new: b.map(Canonical::encode),
                })
            })
            .collect::<Vec<_>>();
        self.cleanup()?;
        self.root.mkdir(b".kasu/transaction/stage")?;
        self.root.mkdir(b".kasu/transaction/backup")?;
        for change in &changes {
            if let Some(raw) = &change.new {
                self.root
                    .install(&staged(&change.path), &Entry::decode(raw)?, &self.cache)?;
            }
        }
        let generation = self
            .state
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("working copy generation exhausted"))?;
        let journal = Journal {
            old_generation: self.state.generation,
            new_generation: generation,
            changes,
        };
        self.root.write(
            b".kasu/transaction/intent.json",
            &serde_json::to_vec(&journal)?,
            true,
            false,
        )?;
        self.root.rename(
            b".kasu/transaction/intent.json",
            b".kasu/transaction.json",
            false,
        )?;
        for change in &journal.changes {
            if let Some(raw) = &change.old {
                if !self
                    .root
                    .matches(&change.path, &Entry::decode(raw)?, &self.cache)?
                {
                    return Err(invalid("file changed during update; journal preserved"));
                }
                self.root
                    .rename(&change.path, &backup(&change.path), false)?;
                self.root.prune(&change.path)?;
            }
        }
        for change in &journal.changes {
            if change.new.is_some() {
                self.root
                    .rename(&staged(&change.path), &change.path, false)?;
            }
        }
        let actual = self.root.scan(&state.sparse, &snapshot, &self.cache)?;
        if actual
            .iter()
            .map(|(path, (entry, _))| (path.clone(), entry.clone()))
            .collect::<Snapshot>()
            != snapshot
        {
            return Err(invalid("files changed during update; journal preserved"));
        }
        state.files = actual
            .into_iter()
            .map(|(path, (entry, stamp))| Tracked {
                path,
                entry: entry.encode(),
                stamp,
            })
            .collect();
        state.generation = generation;
        self.state = state;
        self.save()?;
        self.cleanup()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kasu_store::BlobStore;
    fn fixture() -> (tempfile::TempDir, WorkingCopy, Entry, Entry) {
        let temp = tempfile::tempdir().unwrap();
        let root = Root::open(temp.path()).unwrap();
        root.mkdir(b".kasu/objects").unwrap();
        let cache = FsBlobStore::open(temp.path().join(".kasu/objects")).unwrap();
        let old = Entry::File {
            blob: kasu_core::BlobId(cache.put(b"old").unwrap().0),
            executable: false,
        };
        let new = Entry::File {
            blob: kasu_core::BlobId(cache.put(b"new").unwrap().0),
            executable: true,
        };
        root.install(b"app/a", &old, &cache).unwrap();
        let sparse = SparseProfile::new(vec![]).unwrap();
        let expected = Snapshot::from([(b"app/a".to_vec(), old.clone())]);
        let files = root
            .scan(&sparse, &expected, &cache)
            .unwrap()
            .into_iter()
            .map(|(path, (entry, stamp))| Tracked {
                path,
                entry: entry.encode(),
                stamp,
            })
            .collect();
        let state = State {
            version: 1,
            generation: 0,
            server: "http://localhost:1".into(),
            objects: "http://localhost:1".into(),
            repo: b"repo".to_vec(),
            base: vec![1; 32],
            reference: b"refs/heads/main".to_vec(),
            draft: false,
            sparse,
            files,
            land_key: None,
            land_outcome: None,
        };
        root.write(
            b".kasu/state.json",
            &serde_json::to_vec(&state).unwrap(),
            true,
            false,
        )
        .unwrap();
        let copy = WorkingCopy::open(temp.path()).unwrap();
        (temp, copy, old, new)
    }
    fn intent(copy: &WorkingCopy, old: Option<&Entry>, new: &Entry, path: &[u8]) {
        copy.root.mkdir(b".kasu/transaction/stage").unwrap();
        copy.root.mkdir(b".kasu/transaction/backup").unwrap();
        copy.root.install(&staged(path), new, &copy.cache).unwrap();
        let journal = Journal {
            old_generation: 0,
            new_generation: 1,
            changes: vec![Transition {
                path: path.to_vec(),
                old: old.map(Canonical::encode),
                new: Some(new.encode()),
            }],
        };
        copy.root
            .write(
                b".kasu/transaction.json",
                &serde_json::to_vec(&journal).unwrap(),
                true,
                false,
            )
            .unwrap();
    }
    #[test]
    fn interrupted_install_rolls_back_and_committed_install_survives_cleanup_crash() {
        for committed in [false, true] {
            let (temp, mut copy, old, new) = fixture();
            intent(&copy, Some(&old), &new, b"app/a");
            copy.root
                .rename(b"app/a", &backup(b"app/a"), false)
                .unwrap();
            copy.root
                .rename(&staged(b"app/a"), b"app/a", false)
                .unwrap();
            if committed {
                let files = copy
                    .root
                    .scan(
                        &copy.state.sparse,
                        &Snapshot::from([(b"app/a".to_vec(), new.clone())]),
                        &copy.cache,
                    )
                    .unwrap();
                copy.record_scanned(files).unwrap();
            }
            drop(copy);
            let recovered = WorkingCopy::open(temp.path()).unwrap();
            assert_eq!(
                std::fs::read(temp.path().join("app/a")).unwrap(),
                if committed { b"new" } else { b"old" }
            );
            assert!(recovered.changes().unwrap().is_empty());
            assert!(!temp.path().join(".kasu/transaction.json").exists());
        }
    }
    #[test]
    fn recovery_preserves_edits_after_install_and_keeps_the_original_backup() {
        let (temp, copy, old, new) = fixture();
        intent(&copy, Some(&old), &new, b"app/a");
        copy.root
            .rename(b"app/a", &backup(b"app/a"), false)
            .unwrap();
        copy.root
            .rename(&staged(b"app/a"), b"app/a", false)
            .unwrap();
        std::fs::write(temp.path().join("app/a"), b"external edit").unwrap();
        drop(copy);
        assert!(WorkingCopy::open(temp.path()).is_err());
        assert_eq!(
            std::fs::read(temp.path().join("app/a")).unwrap(),
            b"external edit"
        );
        assert_eq!(
            std::fs::read(
                temp.path()
                    .join(std::str::from_utf8(&backup(b"app/a")).unwrap())
            )
            .unwrap(),
            b"old"
        );
    }
    #[test]
    fn recovery_does_not_remove_matching_user_files_before_install() {
        for added in [false, true] {
            let (temp, copy, old, new) = fixture();
            let path = if added {
                b"app/added".as_slice()
            } else {
                b"app/a".as_slice()
            };
            intent(&copy, if added { None } else { Some(&old) }, &new, path);
            std::fs::write(temp.path().join(std::str::from_utf8(path).unwrap()), b"new").unwrap();
            drop(copy);
            let recovered = WorkingCopy::open(temp.path()).unwrap();
            assert_eq!(
                std::fs::read(temp.path().join(std::str::from_utf8(path).unwrap())).unwrap(),
                b"new"
            );
            assert!(!recovered.changes().unwrap().is_empty());
        }
    }
}
