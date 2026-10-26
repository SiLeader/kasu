use crate::{Result, Snapshot, SparseProfile, invalid};
use kasu_core::{BlobId, Entry, GitOid};
use kasu_store::{BlobStore, FsBlobStore, MAX_OBJECT_SIZE};
use rustix::fs::{self as r, AtFlags, FileType, Mode, OFlags, RenameFlags};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};

pub(crate) struct Root {
    fd: File,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stamp {
    pub size: u64,
    pub inode: u64,
    pub mtime: Option<(i64, i64)>,
}
pub(crate) type Scanned = BTreeMap<Vec<u8>, (Entry, Stamp)>;
impl Root {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            fd: r::open(
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?
            .into(),
        })
    }
    fn directory(&self, path: &[u8], create: bool) -> Result<File> {
        let mut fd = self.fd.try_clone()?;
        for component in path.split(|b| *b == b'/').filter(|s| !s.is_empty()) {
            if component == b"." || component == b".." || component.contains(&0) {
                return Err(invalid("invalid local path"));
            }
            match r::openat(
                &fd,
                component,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(next) => fd = next.into(),
                Err(rustix::io::Errno::NOENT) if create => {
                    r::mkdirat(
                        &fd,
                        component,
                        Mode::from_raw_mode(if component == b".kasu" { 0o700 } else { 0o755 }),
                    )?;
                    fd.sync_all()?;
                    fd = r::openat(
                        &fd,
                        component,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?
                    .into();
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(fd)
    }
    fn parent(&self, path: &[u8], create: bool) -> Result<(File, Vec<u8>)> {
        kasu_core::RepoPath::new(path)?;
        let split = path.iter().rposition(|b| *b == b'/');
        Ok(match split {
            Some(i) => (self.directory(&path[..i], create)?, path[i + 1..].to_vec()),
            None => (self.fd.try_clone()?, path.to_vec()),
        })
    }
    pub fn mkdir(&self, path: &[u8]) -> Result<()> {
        self.directory(path, true)?;
        Ok(())
    }
    pub fn open_file(&self, path: &[u8], write: bool, exclusive: bool) -> Result<File> {
        let (parent, name) = self.parent(path, write)?;
        let flags = if write {
            OFlags::RDWR
                | OFlags::CREATE
                | if exclusive {
                    OFlags::EXCL
                } else {
                    OFlags::TRUNC
                }
        } else {
            OFlags::RDONLY
        };
        let fd = r::openat(
            &parent,
            name.as_slice(),
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )?;
        if !File::from(fd.try_clone()?).metadata()?.is_file() {
            return Err(invalid("expected regular file"));
        }
        Ok(fd.into())
    }
    pub fn lock(&self) -> Result<File> {
        let parent = self.directory(b".kasu", false)?;
        let file: File = r::openat(
            &parent,
            b"lock".as_slice(),
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?
        .into();
        file.try_lock()
            .map_err(|_| invalid("working copy is locked by another process"))?;
        Ok(file)
    }
    pub fn kind(&self, path: &[u8]) -> Result<Option<FileType>> {
        let (parent, name) = match self.parent(path, false) {
            Ok(value) => value,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        match r::statat(&parent, name.as_slice(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(FileType::from_raw_mode(stat.st_mode))),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    pub fn read(&self, path: &[u8]) -> Result<Vec<u8>> {
        let mut bytes = vec![];
        self.open_file(path, false, false)?
            .take(MAX_OBJECT_SIZE + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_OBJECT_SIZE {
            return Err(invalid("file exceeds object limit"));
        }
        Ok(bytes)
    }
    pub fn write(
        &self,
        path: &[u8],
        bytes: &[u8],
        exclusive: bool,
        executable: bool,
    ) -> Result<()> {
        let mut file = self.open_file(path, true, exclusive)?;
        file.write_all(bytes)?;
        r::fchmod(
            &file,
            Mode::from_raw_mode(if executable { 0o755 } else { 0o644 }),
        )?;
        file.sync_all()?;
        self.parent(path, false)?.0.sync_all()?;
        Ok(())
    }
    pub fn rename(&self, from: &[u8], to: &[u8], replace: bool) -> Result<()> {
        let (source, sname) = self.parent(from, false)?;
        let (target, tname) = self.parent(to, true)?;
        if replace {
            r::renameat(&source, sname.as_slice(), &target, tname.as_slice())?;
        } else {
            r::renameat_with(
                &source,
                sname.as_slice(),
                &target,
                tname.as_slice(),
                RenameFlags::NOREPLACE,
            )?;
        }
        source.sync_all()?;
        target.sync_all()?;
        Ok(())
    }
    pub fn remove(&self, path: &[u8], directory: bool) -> Result<()> {
        let (parent, name) = self.parent(path, false)?;
        r::unlinkat(
            &parent,
            name.as_slice(),
            if directory {
                AtFlags::REMOVEDIR
            } else {
                AtFlags::empty()
            },
        )?;
        parent.sync_all()?;
        Ok(())
    }
    pub fn names(&self, path: &[u8]) -> Result<Vec<Vec<u8>>> {
        let fd = self.directory(path, false)?;
        let mut dir = r::Dir::read_from(&fd)?;
        let mut names = vec![];
        for value in &mut dir {
            let value = value?;
            let name = value.file_name().to_bytes();
            if name != b"." && name != b".." {
                names.push(name.to_vec());
            }
        }
        names.sort();
        Ok(names)
    }
    pub fn prune(&self, path: &[u8]) -> Result<()> {
        let mut path = path.to_vec();
        while let Some(i) = path.iter().rposition(|b| *b == b'/') {
            path.truncate(i);
            if path.starts_with(b".kasu") {
                break;
            }
            if !self.names(&path)?.is_empty() {
                break;
            }
            self.remove(&path, true)?;
        }
        Ok(())
    }
    pub fn install(&self, path: &[u8], entry: &Entry, cache: &FsBlobStore) -> Result<()> {
        match entry {
            Entry::File { blob, executable } => self.write(
                path,
                &cache.get(kasu_core::ObjectId(blob.0))?,
                true,
                *executable,
            )?,
            Entry::Symlink { blob } => {
                let target = cache.get(kasu_core::ObjectId(blob.0))?;
                let (parent, name) = self.parent(path, true)?;
                r::symlinkat(target.as_slice(), &parent, name.as_slice())?;
                parent.sync_all()?;
            }
            Entry::Gitlink { commit } => {
                self.mkdir(path)?;
                self.write(
                    &[path, b"/.kasu-gitlink"].concat(),
                    format!("{commit}\n").as_bytes(),
                    true,
                    false,
                )?;
            }
        }
        Ok(())
    }
    pub fn remove_leaf(&self, path: &[u8], entry: &Entry) -> Result<()> {
        if matches!(entry, Entry::Gitlink { .. }) {
            self.remove(&[path, b"/.kasu-gitlink"].concat(), false)?;
            self.remove(path, true)?;
        } else {
            self.remove(path, false)?;
        }
        self.prune(path)
    }
    fn leaf(&self, path: &[u8], cache: &FsBlobStore, gitlink: bool) -> Result<(Entry, Stamp)> {
        let (parent, name) = self.parent(path, false)?;
        let stat = r::statat(&parent, name.as_slice(), AtFlags::SYMLINK_NOFOLLOW)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => {
                let file = self.open_file(path, false, false)?;
                let before = file.metadata()?;
                let stamp = |m: &std::fs::Metadata| Stamp {
                    size: m.len(),
                    inode: m.ino(),
                    mtime: Some((m.mtime(), m.mtime_nsec())),
                };
                let mut bytes = vec![];
                (&file).take(MAX_OBJECT_SIZE + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_OBJECT_SIZE {
                    return Err(invalid("file exceeds object limit"));
                }
                if stamp(&before) != stamp(&file.metadata()?) {
                    return Err(invalid("file changed while reading"));
                }
                Ok((
                    Entry::File {
                        blob: BlobId(cache.put(&bytes)?.0),
                        executable: before.mode() & 0o111 != 0,
                    },
                    stamp(&before),
                ))
            }
            FileType::Symlink => {
                let target = r::readlinkat(&parent, name.as_slice(), vec![])?.into_bytes();
                let next = r::statat(&parent, name.as_slice(), AtFlags::SYMLINK_NOFOLLOW)?;
                if stat.st_ino != next.st_ino || stat.st_size != next.st_size {
                    return Err(invalid("symlink changed while reading"));
                }
                Ok((
                    Entry::Symlink {
                        blob: BlobId(cache.put(&target)?.0),
                    },
                    Stamp {
                        size: target.len() as u64,
                        inode: stat.st_ino,
                        mtime: None,
                    },
                ))
            }
            FileType::Directory if gitlink => {
                if self.names(path)? != vec![b".kasu-gitlink".to_vec()] {
                    return Err(invalid("gitlink placeholder contains unsupported files"));
                }
                let bytes = self.read(&[path, b"/.kasu-gitlink"].concat())?;
                let commit = std::str::from_utf8(&bytes)?.trim().parse::<GitOid>()?;
                Ok((
                    Entry::Gitlink { commit },
                    Stamp {
                        size: bytes.len() as u64,
                        inode: stat.st_ino,
                        mtime: None,
                    },
                ))
            }
            _ => Err(invalid("unsupported working copy file type")),
        }
    }
    pub fn matches(&self, path: &[u8], entry: &Entry, cache: &FsBlobStore) -> Result<bool> {
        if self.kind(path)?.is_none() {
            return Ok(false);
        }
        Ok(self
            .leaf(path, cache, matches!(entry, Entry::Gitlink { .. }))
            .is_ok_and(|(actual, _)| actual == *entry))
    }
    pub fn scan(
        &self,
        sparse: &SparseProfile,
        expected: &Snapshot,
        cache: &FsBlobStore,
    ) -> Result<Scanned> {
        let mut result = BTreeMap::new();
        let mut stack = vec![vec![]];
        while let Some(path) = stack.pop() {
            for name in self.names(&path)? {
                if path.is_empty() && name == b".kasu" {
                    continue;
                }
                let child = if path.is_empty() {
                    name
                } else {
                    [path.as_slice(), b"/", name.as_slice()].concat()
                };
                let gitlink = matches!(expected.get(&child), Some(Entry::Gitlink { .. }));
                if self.kind(&child)? == Some(FileType::Directory) && !gitlink {
                    if sparse.enters(&child) {
                        crate::validate_work_path(&child)?;
                        stack.push(child);
                    }
                } else if sparse.includes(&child) {
                    crate::validate_work_path(&child)?;
                    result.insert(child.clone(), self.leaf(&child, cache, gitlink)?);
                }
            }
        }
        Ok(result)
    }
}
fn missing(error: &crate::Error) -> bool {
    error
        .downcast_ref::<rustix::io::Errno>()
        .is_some_and(|e| matches!(*e, rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR))
}
