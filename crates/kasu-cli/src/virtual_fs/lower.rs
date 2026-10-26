use crate::{Remote, Result, Snapshot, invalid};
use fuser::*;
use kasu_core::{BlobId, Entry};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    os::unix::ffi::OsStrExt,
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

const TTL: Duration = Duration::from_secs(3600);
const CACHE_BYTES: usize = 32 * 1024 * 1024;
struct Node {
    parent: u64,
    kind: FileType,
    perm: u16,
    size: u64,
    blob: Option<BlobId>,
    inline: Option<Vec<u8>>,
    children: BTreeMap<Vec<u8>, u64>,
}
impl Node {
    fn directory(parent: u64) -> Self {
        Self {
            parent,
            kind: FileType::Directory,
            perm: 0o755,
            size: 4096,
            blob: None,
            inline: None,
            children: Default::default(),
        }
    }
}
struct Fetch {
    remote: Remote,
    cached: Option<(BlobId, Arc<Vec<u8>>)>,
}
pub(super) struct Lower {
    nodes: Vec<Node>,
    fetch: Mutex<Fetch>,
    runtime: tokio::runtime::Handle,
    uid: u32,
    gid: u32,
}
impl Lower {
    pub(super) fn new(
        snapshot: Snapshot,
        sizes: BTreeMap<BlobId, u64>,
        remote: Remote,
        uid: u32,
        gid: u32,
    ) -> Result<Self> {
        let mut fs = Self {
            nodes: vec![Node::directory(1)],
            fetch: Mutex::new(Fetch {
                remote,
                cached: None,
            }),
            runtime: tokio::runtime::Handle::current(),
            uid,
            gid,
        };
        for (path, entry) in snapshot {
            crate::validate_work_path(&path)?;
            let mut parts = path.split(|b| *b == b'/').peekable();
            let mut parent = 1;
            while let Some(part) = parts.next() {
                if parts.peek().is_some() {
                    parent = match fs.node(parent).unwrap().children.get(part) {
                        Some(id) if fs.node(*id).unwrap().kind == FileType::Directory => *id,
                        Some(_) => return Err(invalid("virtual file/directory collision")),
                        None => fs.add(parent, part.to_vec(), Node::directory(parent))?,
                    };
                    continue;
                }
                match &entry {
                    Entry::Gitlink { commit } => {
                        let directory = fs.add(parent, part.to_vec(), Node::directory(parent))?;
                        let bytes = format!("{commit}\n").into_bytes();
                        fs.add(
                            directory,
                            b".kasu-gitlink".to_vec(),
                            Node {
                                parent: directory,
                                kind: FileType::RegularFile,
                                perm: 0o644,
                                size: bytes.len() as u64,
                                blob: None,
                                inline: Some(bytes),
                                children: Default::default(),
                            },
                        )?;
                    }
                    _ => {
                        let blob = entry.blob().unwrap();
                        let size = *sizes
                            .get(&blob)
                            .ok_or_else(|| invalid("missing virtual blob size"))?;
                        let kind = if matches!(entry, Entry::Symlink { .. }) {
                            FileType::Symlink
                        } else {
                            FileType::RegularFile
                        };
                        let perm = if matches!(
                            entry,
                            Entry::File {
                                executable: true,
                                ..
                            }
                        ) {
                            0o755
                        } else if kind == FileType::Symlink {
                            0o777
                        } else {
                            0o644
                        };
                        fs.add(
                            parent,
                            part.to_vec(),
                            Node {
                                parent,
                                kind,
                                perm,
                                size,
                                blob: Some(blob),
                                inline: None,
                                children: Default::default(),
                            },
                        )?;
                    }
                }
            }
        }
        Ok(fs)
    }
    fn add(&mut self, parent: u64, name: Vec<u8>, node: Node) -> Result<u64> {
        let id = self.nodes.len() as u64 + 1;
        if self.nodes[(parent - 1) as usize]
            .children
            .insert(name, id)
            .is_some()
        {
            return Err(invalid("duplicate virtual path"));
        }
        self.nodes.push(node);
        Ok(id)
    }
    fn node(&self, ino: u64) -> Option<&Node> {
        self.nodes.get(usize::try_from(ino.checked_sub(1)?).ok()?)
    }
    fn attr(&self, ino: INodeNo) -> Option<FileAttr> {
        let node = self.node(ino.0)?;
        Some(FileAttr {
            ino,
            size: node.size,
            blocks: node.size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: node.kind,
            perm: node.perm,
            nlink: if node.kind == FileType::Directory {
                2
            } else {
                1
            },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
    fn bytes(&self, ino: INodeNo) -> std::result::Result<Arc<Vec<u8>>, Errno> {
        let node = self.node(ino.0).ok_or(Errno::ENOENT)?;
        if let Some(bytes) = &node.inline {
            return Ok(Arc::new(bytes.clone()));
        }
        let id = node.blob.ok_or(Errno::EISDIR)?;
        let mut fetch = self.fetch.lock().map_err(|_| Errno::EIO)?;
        if let Some((cached, bytes)) = &fetch.cached
            && *cached == id
        {
            return Ok(bytes.clone());
        }
        let result = self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(30), fetch.remote.fetch_blob_bytes(id)).await
        });
        let bytes = match result {
            Ok(Ok(bytes)) if bytes.len() as u64 == node.size => Arc::new(bytes),
            _ => {
                tracing::warn!("virtual blob read failed");
                return Err(Errno::EIO);
            }
        };
        if bytes.len() <= CACHE_BYTES {
            fetch.cached = Some((id, bytes.clone()));
        }
        Ok(bytes)
    }
}
impl Filesystem for Lower {
    fn lookup(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(node) = self.node(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let child = match name.as_bytes() {
            b"." => Some(parent.0),
            b".." => Some(node.parent),
            name => node.children.get(name).copied(),
        };
        if let Some(attr) = child.and_then(|id| self.attr(INodeNo(id))) {
            reply.entry(&TTL, &attr, Generation(0));
        } else {
            reply.error(Errno::ENOENT);
        }
    }
    fn getattr(&self, _: &Request, ino: INodeNo, _: Option<FileHandle>, reply: ReplyAttr) {
        if let Some(attr) = self.attr(ino) {
            reply.attr(&TTL, &attr);
        } else {
            reply.error(Errno::ENOENT);
        }
    }
    fn open(&self, _: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.0 & 3 != 0 {
            reply.error(Errno::EROFS);
        } else if self.node(ino.0).is_none() {
            reply.error(Errno::ENOENT);
        } else {
            reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
        }
    }
    fn read(
        &self,
        _: &Request,
        ino: INodeNo,
        _: FileHandle,
        offset: u64,
        size: u32,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.bytes(ino) {
            Ok(bytes) => {
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(bytes.len());
                let end = start.saturating_add(size as usize).min(bytes.len());
                reply.data(&bytes[start..end]);
            }
            Err(error) => reply.error(error),
        }
    }
    fn readlink(&self, _: &Request, ino: INodeNo, reply: ReplyData) {
        if self.node(ino.0).is_none_or(|n| n.kind != FileType::Symlink) {
            reply.error(Errno::EINVAL);
            return;
        }
        match self.bytes(ino) {
            Ok(bytes) => reply.data(&bytes),
            Err(error) => reply.error(error),
        }
    }
    fn readdir(
        &self,
        _: &Request,
        ino: INodeNo,
        _: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(node) = self.node(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if node.kind != FileType::Directory {
            reply.error(Errno::ENOTDIR);
            return;
        }
        let entries = [
            (ino.0, FileType::Directory, b".".as_slice()),
            (node.parent, FileType::Directory, b"..".as_slice()),
        ]
        .into_iter()
        .chain(
            node.children
                .iter()
                .map(|(name, id)| (*id, self.node(*id).unwrap().kind, name.as_slice())),
        );
        for (n, (id, kind, name)) in entries
            .enumerate()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        {
            if reply.add(INodeNo(id), n as u64 + 1, kind, OsStr::from_bytes(name)) {
                break;
            }
        }
        reply.ok();
    }
    fn getxattr(&self, _: &Request, _: INodeNo, _: &OsStr, _: u32, reply: ReplyXattr) {
        reply.error(Errno::ENODATA);
    }
    fn listxattr(&self, _: &Request, _: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }
}
