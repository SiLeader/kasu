use super::{Scope, io};
use crate::{Frame, Key, Store};
use kasu_core::ObjectId;
use kasu_object::Kind;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{
    fs,
    io::{Read, Write},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tonic::Status;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiskLimits {
    pub bytes: u64,
    pub entries: u64,
}
impl Default for DiskLimits {
    fn default() -> Self {
        Self {
            bytes: 8 * 1024 * 1024 * 1024,
            entries: 100_000,
        }
    }
}
impl DiskLimits {
    pub fn validate(self) -> Result<(), Status> {
        if self.bytes == 0 || self.bytes > 1 << 50 || self.entries == 0 || self.entries > 1_000_000
        {
            return Err(Status::invalid_argument("invalid disk cache limits"));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiskStats {
    pub entries: u64,
    pub bytes: u64,
    pub evictions: u64,
    pub evicted_bytes: u64,
    pub admission_skips: u64,
    pub repairs: u64,
    pub limits: DiskLimits,
}
#[derive(Default)]
struct CachedStats([AtomicU64; 8]);
pub struct DiskStore {
    pub(super) root: PathBuf,
    connection: Mutex<Connection>,
    cached: CachedStats,
}
pub(super) struct Guard<'a> {
    conn: MutexGuard<'a, Connection>,
    _lock: fs::File,
    owner: &'a DiskStore,
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let _ = self.owner.refresh_stats(&self.conn);
    }
}
impl Deref for Guard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}
impl DerefMut for Guard<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}
pub(super) fn owned_temp(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|tail| tail.len() == 6 && tail.bytes().all(|b| b.is_ascii_alphanumeric()))
}
fn directory(path: &Path) -> Result<(), Status> {
    if !path.symlink_metadata().map_err(io)?.file_type().is_dir() {
        return Err(Status::data_loss("disk cache directory is not a directory"));
    }
    Ok(())
}
fn sync(path: &Path) -> Result<(), Status> {
    fs::File::open(path).and_then(|f| f.sync_all()).map_err(io)
}
fn regular(path: &Path) -> Result<Option<fs::Metadata>, Status> {
    match path.symlink_metadata() {
        Ok(m) if m.file_type().is_file() => Ok(Some(m)),
        Ok(_) => Err(Status::data_loss("disk cache contains a nonregular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(e)),
    }
}
fn lock(root: &Path, exclusive: bool) -> Result<fs::File, Status> {
    directory(root)?;
    let file: fs::File = rustix::fs::open(
        root.join(".cache-lru.lock"),
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map_err(io)?
    .into();
    if !file.metadata().map_err(io)?.is_file() {
        return Err(Status::data_loss("invalid disk cache lock"));
    }
    let start = Instant::now();
    loop {
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match result {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(Status::unavailable("disk cache lock deadline exceeded"));
            }
            Err(e) => return Err(io(e)),
        }
    }
}
impl DiskStore {
    /// Open a disk cache shared by every repository. A cache created for a
    /// single repository keeps serving its frames to that repository.
    pub fn open(root: &Path) -> Result<Self, Status> {
        Self::open_inner(root, None)
    }
    pub fn open_with_limits(root: &Path, limits: DiskLimits) -> Result<Self, Status> {
        limits.validate()?;
        Self::open_inner(root, Some(limits))
    }
    fn open_inner(root: &Path, limits: Option<DiskLimits>) -> Result<Self, Status> {
        fs::create_dir_all(root).map_err(io)?;
        directory(root)?;
        // SQLite NOFOLLOW also checks parent components; resolve system aliases such as /var.
        let resolved = fs::canonicalize(root).map_err(io)?;
        let root = resolved.as_path();
        let _lock = lock(root, true)?;
        if root.join("meta.db").symlink_metadata().is_ok() {
            return Err(Status::failed_precondition(
                "repository storage cannot be used as a disk cache",
            ));
        }
        let scope = Scope {
            version: 2,
            repository: Vec::new(),
        };
        let path = root.join("scope.json");
        if regular(&path)?.is_none() {
            for entry in fs::read_dir(root).map_err(io)? {
                let entry = entry.map_err(io)?;
                let name = entry.file_name();
                let safe = name.to_str().is_some_and(|n| {
                    n == ".cache-lru.lock" || n == "daemon.lock" || owned_temp(n, ".kasu-scope-")
                });
                if !safe {
                    return Err(Status::failed_precondition(
                        "new disk cache requires an empty directory",
                    ));
                }
                regular(&entry.path())?;
            }
            let mut file = tempfile::Builder::new()
                .prefix(".kasu-scope-")
                .tempfile_in(root)
                .map_err(io)?;
            file.write_all(&serde_json::to_vec(&scope).map_err(io)?)
                .map_err(io)?;
            file.as_file().sync_all().map_err(io)?;
            file.persist_noclobber(&path).map_err(io)?;
            sync(root)?;
        }
        if regular(&path)?.is_none_or(|m| m.len() > 64 * 1024) {
            return Err(Status::data_loss("invalid cache scope file"));
        }
        let stored: Scope = serde_json::from_slice(&fs::read(path).map_err(io)?)
            .map_err(|_| Status::data_loss("invalid cache scope file"))?;
        // Version 1 caches were bound to one repository, recorded in the catalog.
        if !matches!(
            (stored.version, stored.repository.is_empty()),
            (1, false) | (2, true)
        ) {
            return Err(Status::data_loss("unsupported cache scope file"));
        }
        for path in [
            root.join("index"),
            root.join("index/trees"),
            root.join("index/blobs"),
            root.join("objects"),
        ] {
            fs::create_dir_all(&path).map_err(io)?;
            directory(&path)?;
        }
        for name in [
            "cache-lru-v1.sqlite3",
            "cache-lru-v1.sqlite3-wal",
            "cache-lru-v1.sqlite3-shm",
        ] {
            regular(&root.join(name))?;
        }
        let conn = Connection::open_with_flags(
            root.join("cache-lru-v1.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(io)?;
        conn.busy_timeout(Duration::from_secs(2)).map_err(io)?;
        let version: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(io)?;
        if version > 1 {
            return Err(Status::data_loss("unsupported disk cache catalog version"));
        }
        conn.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA foreign_keys=ON;
          CREATE TABLE IF NOT EXISTS config(singleton INTEGER PRIMARY KEY CHECK(singleton=1),repository BLOB NOT NULL,max_bytes INTEGER NOT NULL,max_entries INTEGER NOT NULL,tick INTEGER NOT NULL DEFAULT 0,bytes INTEGER NOT NULL DEFAULT 0,entries INTEGER NOT NULL DEFAULT 0,dirty INTEGER NOT NULL DEFAULT 0,evictions INTEGER NOT NULL DEFAULT 0,evicted_bytes INTEGER NOT NULL DEFAULT 0,skips INTEGER NOT NULL DEFAULT 0,repairs INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS frames(id TEXT PRIMARY KEY,size INTEGER NOT NULL,accessed INTEGER NOT NULL);
          CREATE INDEX IF NOT EXISTS frame_lru ON frames(accessed,id);
          CREATE TABLE IF NOT EXISTS kinds(kind TEXT NOT NULL,id TEXT NOT NULL REFERENCES frames(id) ON DELETE CASCADE,PRIMARY KEY(kind,id));
          CREATE INDEX IF NOT EXISTS kind_frame ON kinds(id,kind);
          CREATE TRIGGER IF NOT EXISTS frame_insert AFTER INSERT ON frames BEGIN UPDATE config SET bytes=bytes+NEW.size;END;
          CREATE TRIGGER IF NOT EXISTS frame_delete AFTER DELETE ON frames BEGIN UPDATE config SET bytes=bytes-OLD.size;END;
          CREATE TRIGGER IF NOT EXISTS frame_resize AFTER UPDATE OF size ON frames BEGIN UPDATE config SET bytes=bytes+NEW.size-OLD.size;END;
          CREATE TRIGGER IF NOT EXISTS kind_insert AFTER INSERT ON kinds BEGIN UPDATE config SET entries=entries+1;END;
          CREATE TRIGGER IF NOT EXISTS kind_delete AFTER DELETE ON kinds BEGIN UPDATE config SET entries=entries-1;END;
          CREATE TEMP TABLE pinned(kind TEXT,id TEXT,PRIMARY KEY(kind,id));
          CREATE TEMP TABLE candidates(kind TEXT,id TEXT,accessed INTEGER,PRIMARY KEY(kind,id));
          CREATE INDEX candidate_lru ON candidates(accessed,id,kind);
          CREATE INDEX candidate_frame ON candidates(id);
          CREATE TABLE IF NOT EXISTS members(repo BLOB NOT NULL,kind TEXT NOT NULL,id TEXT NOT NULL,PRIMARY KEY(repo,kind,id)) WITHOUT ROWID;
          CREATE TABLE IF NOT EXISTS identities(repo BLOB PRIMARY KEY,identity TEXT NOT NULL) WITHOUT ROWID;
          PRAGMA user_version=1;").map_err(io)?;
        let defaults = DiskLimits::default();
        conn.execute("INSERT OR IGNORE INTO config(singleton,repository,max_bytes,max_entries) VALUES(1,X'',?1,?2)",params![defaults.bytes,defaults.entries]).map_err(io)?;
        let saved: Vec<u8> = conn
            .query_row("SELECT repository FROM config", [], |r| r.get(0))
            .map_err(io)?;
        if !saved.is_empty() {
            // Frames of a single-repository cache were confirmed for that repository.
            if let Some(repo) = crate::normalize(&saved) {
                conn.execute(
                    "INSERT OR IGNORE INTO members(repo,kind,id) SELECT ?1,kind,id FROM kinds",
                    [repo],
                )
                .map_err(io)?;
            }
            conn.execute("UPDATE config SET repository=X''", [])
                .map_err(io)?;
        }
        if let Some(limits) = limits {
            conn.execute(
                "UPDATE config SET max_bytes=?1,max_entries=?2,dirty=1",
                params![limits.bytes, limits.entries],
            )
            .map_err(io)?;
        }
        let store = Self {
            root: root.to_owned(),
            connection: Mutex::new(conn),
            cached: CachedStats::default(),
        };
        {
            let mut conn = store.connection.lock().map_err(io)?;
            store.recover(&mut conn)?;
            store.refresh_stats(&conn)?;
        }
        sync(root)?;
        Ok(store)
    }
    pub(super) fn guard(&self, exclusive: bool) -> Result<Guard<'_>, Status> {
        let mut conn = self.connection.lock().map_err(io)?;
        let mut file = lock(&self.root, exclusive)?;
        let dirty: bool = conn
            .query_row("SELECT dirty FROM config", [], |r| r.get(0))
            .map_err(io)?;
        if dirty {
            if !exclusive {
                drop(file);
                file = lock(&self.root, true)?;
            }
            self.recover(&mut conn)?;
        }
        Ok(Guard {
            conn,
            _lock: file,
            owner: self,
        })
    }
    fn marker(&self, key: Key) -> PathBuf {
        self.root
            .join("index")
            .join(key.kind.name())
            .join(key.id.to_string())
    }
    fn frame(&self, id: ObjectId) -> PathBuf {
        let hex = id.to_string();
        self.root.join("objects").join(&hex[..2]).join(&hex[2..])
    }
    fn frame_directory(&self, id: ObjectId) -> Result<PathBuf, Status> {
        directory(&self.root.join("objects"))?;
        let path = self.frame(id);
        let parent = path.parent().unwrap();
        let fresh = !parent.exists();
        fs::create_dir_all(parent).map_err(io)?;
        directory(parent)?;
        if fresh {
            sync(&self.root.join("objects"))?;
        }
        Ok(parent.to_owned())
    }
    fn marker_size(&self, key: Key) -> Result<Option<u64>, Status> {
        directory(&self.root.join("index"))?;
        directory(&self.root.join("index").join(key.kind.name()))?;
        let path = self.marker(key);
        let Some(meta) = regular(&path)? else {
            return Ok(None);
        };
        if meta.len() != 8 {
            return Err(Status::data_loss("invalid disk cache index"));
        }
        let bytes = fs::read(path).map_err(io)?;
        let size = u64::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| Status::data_loss("invalid disk cache index"))?,
        );
        if size == 0 || size > kasu_object::MAX_FRAME_BYTES as u64 {
            return Err(Status::data_loss("invalid disk cache frame size"));
        }
        Ok(Some(size))
    }
    pub fn prune_snapshots(&self, keep: usize) -> Result<usize, Status> {
        if !(1..=1000).contains(&keep) {
            return Err(Status::invalid_argument(
                "snapshot retention must be 1..1000",
            ));
        }
        let _guard = self.guard(true)?;
        let folder = self.root.join("snapshots");
        fs::create_dir_all(&folder).map_err(io)?;
        directory(&folder)?;
        let mut files = Vec::new();
        let mut removed = 0;
        for entry in fs::read_dir(&folder).map_err(io)? {
            let entry = entry.map_err(io)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if owned_temp(name, ".kasu-snapshot-write-") {
                regular(&entry.path())?;
                fs::remove_file(entry.path()).map_err(io)?;
                removed += 1;
                continue;
            }
            let Some(stamp) = name
                .strip_prefix("kasu-snapshot-v1-")
                .and_then(|s| s.strip_suffix(".tar"))
            else {
                continue;
            };
            let Ok(number) = stamp.parse::<u128>() else {
                continue;
            };
            if number.to_string() != stamp {
                continue;
            }
            regular(&entry.path())?;
            files.push((number, entry.path()));
        }
        files.sort_by_key(|(stamp, _)| *stamp);
        let excess = files.len().saturating_sub(keep);
        for (_, path) in files.into_iter().take(excess) {
            fs::remove_file(path).map_err(io)?;
            removed += 1;
        }
        sync(&folder)?;
        Ok(removed)
    }
    pub(super) fn get_locked(&self, key: Key) -> Result<Option<Frame>, Status> {
        directory(&self.root.join("objects"))?;
        let Some(size) = self.marker_size(key)? else {
            return Ok(None);
        };
        let path = self.frame(key.id);
        directory(path.parent().unwrap())?;
        let meta = regular(&path)?.ok_or_else(|| Status::data_loss("disk cached frame missing"))?;
        if meta.len() != size {
            return Err(Status::data_loss("disk cache frame size mismatch"));
        }
        let mut bytes = Vec::new();
        fs::File::open(path)
            .map_err(io)?
            .take(size + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() as u64 != size {
            return Err(Status::data_loss("disk cache frame size mismatch"));
        }
        kasu_object::validate(key.kind, key.id, &bytes)?;
        Ok(Some(Arc::new(bytes)))
    }
    pub(super) fn limits_locked(&self, conn: &Connection) -> Result<DiskLimits, Status> {
        let limits = conn
            .query_row("SELECT max_bytes,max_entries FROM config", [], |r| {
                Ok(DiskLimits {
                    bytes: r.get(0)?,
                    entries: r.get(1)?,
                })
            })
            .map_err(io)?;
        limits.validate()?;
        Ok(limits)
    }
    pub fn limits(&self) -> Result<DiskLimits, Status> {
        let guard = self.guard(false)?;
        self.limits_locked(&guard)
    }
    pub fn stats(&self) -> Result<DiskStats, Status> {
        let conn = self.guard(false)?;
        let limits = self.limits_locked(&conn)?;
        conn.query_row(
            "SELECT entries,bytes,evictions,evicted_bytes,skips,repairs FROM config",
            [],
            |r| {
                Ok(DiskStats {
                    entries: r.get(0)?,
                    bytes: r.get(1)?,
                    evictions: r.get(2)?,
                    evicted_bytes: r.get(3)?,
                    admission_skips: r.get(4)?,
                    repairs: r.get(5)?,
                    limits,
                })
            },
        )
        .map_err(io)
    }
    pub(super) fn catalog_locked(&self, conn: &Connection) -> Result<Vec<(Key, u64)>, Status> {
        let mut statement = conn
            .prepare("SELECT kind,kinds.id,size FROM kinds JOIN frames USING(id) ORDER BY CASE kind WHEN 'trees' THEN 0 ELSE 1 END,id")
            .map_err(io)?;
        let raw = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u64>(2)?,
                ))
            })
            .map_err(io)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(io)?;
        raw.into_iter()
            .map(|(kind, id, size)| {
                Ok((
                    Key {
                        kind: match kind.as_str() {
                            "trees" => Kind::Tree,
                            "blobs" => Kind::Blob,
                            _ => return Err(Status::data_loss("invalid disk catalog kind")),
                        },
                        id: id.parse().map_err(io)?,
                    },
                    size,
                ))
            })
            .collect()
    }
    pub fn catalog(&self) -> Result<Vec<(Key, u64)>, Status> {
        let guard = self.guard(false)?;
        self.catalog_locked(&guard)
    }
    fn tick(conn: &Connection) -> Result<i64, Status> {
        let tick: i64 = conn
            .query_row("SELECT tick FROM config", [], |r| r.get(0))
            .map_err(io)?;
        if tick == i64::MAX {
            conn.execute_batch("CREATE TEMP TABLE reorder AS SELECT id,ROW_NUMBER() OVER(ORDER BY accessed,id) AS rank FROM frames;UPDATE frames SET accessed=(SELECT rank FROM reorder WHERE reorder.id=frames.id);UPDATE config SET tick=(SELECT COUNT(*) FROM frames);UPDATE candidates SET accessed=(SELECT accessed FROM frames WHERE frames.id=candidates.id);DROP TABLE reorder;").map_err(io)?;
        }
        conn.query_row("UPDATE config SET tick=tick+1 RETURNING tick", [], |r| {
            r.get(0)
        })
        .map_err(io)
    }
    fn evict_key(
        &self,
        conn: &Connection,
        key: Key,
        size: u64,
        protect: Option<ObjectId>,
    ) -> Result<(), Status> {
        directory(&self.root.join("index"))?;
        directory(&self.root.join("objects"))?;
        let path = self.marker(key);
        directory(path.parent().unwrap())?;
        if regular(&path)?.is_some() {
            fs::remove_file(&path).map_err(io)?;
            sync(path.parent().unwrap())?;
        }
        conn.execute(
            "DELETE FROM candidates WHERE kind=?1 AND id=?2",
            params![key.kind.name(), key.id.to_string()],
        )
        .map_err(io)?;
        conn.execute(
            "DELETE FROM kinds WHERE kind=?1 AND id=?2",
            params![key.kind.name(), key.id.to_string()],
        )
        .map_err(io)?;
        let remaining: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM kinds WHERE id=?1)",
                [key.id.to_string()],
                |r| r.get(0),
            )
            .map_err(io)?;
        let removed = if !remaining && Some(key.id) != protect {
            let path = self.frame(key.id);
            directory(path.parent().unwrap())?;
            if regular(&path)?.is_some() {
                fs::remove_file(&path).map_err(io)?;
                sync(path.parent().unwrap())?;
            }
            conn.execute("DELETE FROM frames WHERE id=?1", [key.id.to_string()])
                .map_err(io)?;
            size
        } else {
            0
        };
        conn.execute("UPDATE config SET evictions=CASE WHEN evictions<9223372036854775807 THEN evictions+1 ELSE evictions END,evicted_bytes=CASE WHEN evicted_bytes>9223372036854775807-?1 THEN 9223372036854775807 ELSE evicted_bytes+?1 END",[removed]).map_err(io)?;
        Ok(())
    }
    fn make_room(
        &self,
        conn: &Connection,
        bytes: u64,
        entries: u64,
        protect: Option<ObjectId>,
    ) -> Result<bool, Status> {
        let limits = self.limits_locked(conn)?;
        loop {
            let (used, count): (u64, u64) = conn
                .query_row("SELECT bytes,entries FROM config", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .map_err(io)?;
            if used.saturating_add(bytes) <= limits.bytes
                && count.saturating_add(entries) <= limits.entries
            {
                return Ok(true);
            }
            let pinned: bool = conn
                .query_row("SELECT EXISTS(SELECT 1 FROM pinned)", [], |r| r.get(0))
                .map_err(io)?;
            let query = if pinned {
                "SELECT candidates.kind,candidates.id,frames.size FROM candidates INDEXED BY candidate_lru JOIN frames ON frames.id=candidates.id ORDER BY candidates.accessed,candidates.id,candidates.kind LIMIT 1"
            } else {
                "SELECT kinds.kind,frames.id,frames.size FROM frames INDEXED BY frame_lru JOIN kinds ON kinds.id=frames.id ORDER BY frames.accessed,frames.id,kinds.kind LIMIT 1"
            };
            let victim: Option<(String, String, u64)> = conn
                .query_row(query, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()
                .map_err(io)?;
            let Some((kind, id, size)) = victim else {
                return Ok(false);
            };
            let kind = match kind.as_str() {
                "trees" => Kind::Tree,
                "blobs" => Kind::Blob,
                _ => return Err(Status::data_loss("invalid disk catalog kind")),
            };
            self.evict_key(
                conn,
                Key {
                    kind,
                    id: id.parse().map_err(io)?,
                },
                size,
                protect,
            )?;
        }
    }
    pub(super) fn existing_size(
        &self,
        conn: &Connection,
        id: ObjectId,
    ) -> Result<Option<u64>, Status> {
        conn.query_row(
            "SELECT size FROM frames WHERE id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()
        .map_err(io)
    }
    fn refresh_stats(&self, conn: &Connection) -> Result<(), Status> {
        let values:[u64;8]=conn.query_row("SELECT entries,bytes,evictions,evicted_bytes,skips,repairs,max_bytes,max_entries FROM config",[],|r|Ok([r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?])).map_err(io)?;
        for (counter, value) in self.cached.0.iter().zip(values) {
            counter.store(value, Ordering::Relaxed);
        }
        Ok(())
    }
    pub fn cached_stats(&self) -> DiskStats {
        let v = self.cached.0.each_ref().map(|v| v.load(Ordering::Relaxed));
        DiskStats {
            entries: v[0],
            bytes: v[1],
            evictions: v[2],
            evicted_bytes: v[3],
            admission_skips: v[4],
            repairs: v[5],
            limits: DiskLimits {
                bytes: v[6],
                entries: v[7],
            },
        }
    }
    pub(super) fn put_locked(
        &self,
        conn: &mut Connection,
        key: Key,
        bytes: Frame,
    ) -> Result<(), Status> {
        kasu_object::validate(key.kind, key.id, &bytes)?;
        let existing: Option<u64> = conn
            .query_row(
                "SELECT size FROM frames WHERE id=?1",
                [key.id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(io)?;
        let present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM kinds WHERE kind=?1 AND id=?2)",
                params![key.kind.name(), key.id.to_string()],
                |r| r.get(0),
            )
            .map_err(io)?;
        let limits = self.limits_locked(conn)?;
        if existing.is_none() && bytes.len() as u64 > limits.bytes {
            conn.execute(
                "UPDATE config SET skips=CASE WHEN skips<9223372036854775807 THEN skips+1 ELSE skips END",
                [],
            )
            .map_err(io)?;
            return Ok(());
        }
        if let Some(size) = existing {
            let stored = self.get_frame(key.id, size)?;
            kasu_object::validate(key.kind, key.id, &stored)?;
            if stored.as_slice() != bytes.as_slice()
                && kasu_store::decompress(&stored).map_err(io)?
                    != kasu_store::decompress(&bytes).map_err(io)?
            {
                return Err(Status::data_loss("disk cached object collision"));
            }
        }
        if present {
            if self.marker_size(key)? != existing {
                return Err(Status::data_loss(
                    "disk cache marker disappeared or size mismatch",
                ));
            }
            let tx = conn.transaction().map_err(io)?;
            let tick = Self::tick(&tx)?;
            tx.execute(
                "UPDATE frames SET accessed=?1 WHERE id=?2",
                params![tick, key.id.to_string()],
            )
            .map_err(io)?;
            tx.execute(
                "UPDATE candidates SET accessed=?1 WHERE id=?2",
                params![tick, key.id.to_string()],
            )
            .map_err(io)?;
            tx.commit().map_err(io)?;
            return Ok(());
        }
        let parent = self.frame_directory(key.id)?;
        directory(&self.root.join("index"))?;
        directory(&self.root.join("index").join(key.kind.name()))?;
        conn.execute("UPDATE config SET dirty=1", []).map_err(io)?;
        let tx = conn.transaction().map_err(io)?;
        if !self.make_room(
            &tx,
            if existing.is_some() {
                0
            } else {
                bytes.len() as u64
            },
            1,
            existing.map(|_| key.id),
        )? {
            tx.execute(
                "UPDATE config SET skips=CASE WHEN skips<9223372036854775807 THEN skips+1 ELSE skips END,dirty=0",
                [],
            )
            .map_err(io)?;
            tx.commit().map_err(io)?;
            return Ok(());
        }
        let frame_path = self.frame(key.id);
        let size = if let Some(size) = existing {
            size
        } else {
            if regular(&frame_path)?.is_some() {
                return Err(Status::data_loss("unexpected unindexed disk frame"));
            }
            let mut file = tempfile::Builder::new()
                .prefix(".kasu-frame-")
                .tempfile_in(&parent)
                .map_err(io)?;
            file.write_all(&bytes).map_err(io)?;
            file.as_file().sync_all().map_err(io)?;
            file.persist_noclobber(&frame_path).map_err(io)?;
            sync(&parent)?;
            bytes.len() as u64
        };
        let path = self.marker(key);
        directory(path.parent().unwrap())?;
        let mut marker = tempfile::Builder::new()
            .prefix(".kasu-index-")
            .tempfile_in(path.parent().unwrap())
            .map_err(io)?;
        marker.write_all(&size.to_le_bytes()).map_err(io)?;
        marker.as_file().sync_all().map_err(io)?;
        marker.persist_noclobber(&path).map_err(io)?;
        sync(path.parent().unwrap())?;
        let tick = Self::tick(&tx)?;
        tx.execute("INSERT INTO frames(id,size,accessed) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET accessed=excluded.accessed",params![key.id.to_string(),size,tick]).map_err(io)?;
        tx.execute(
            "UPDATE candidates SET accessed=?1 WHERE id=?2",
            params![tick, key.id.to_string()],
        )
        .map_err(io)?;
        tx.execute(
            "INSERT INTO kinds(kind,id) VALUES(?1,?2)",
            params![key.kind.name(), key.id.to_string()],
        )
        .map_err(io)?;
        tx.execute("UPDATE config SET dirty=0", []).map_err(io)?;
        tx.commit().map_err(io)?;
        Ok(())
    }
    fn get_frame(&self, id: ObjectId, size: u64) -> Result<Vec<u8>, Status> {
        directory(&self.root.join("objects"))?;
        let path = self.frame(id);
        directory(path.parent().unwrap())?;
        if regular(&path)?.is_none_or(|m| m.len() != size) {
            return Err(Status::data_loss(
                "disk cache frame missing or size mismatch",
            ));
        }
        let mut bytes = Vec::new();
        fs::File::open(path)
            .map_err(io)?
            .take(size + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() as u64 != size {
            return Err(Status::data_loss("disk cache frame size mismatch"));
        }
        Ok(bytes)
    }
    fn recover(&self, conn: &mut Connection) -> Result<(), Status> {
        conn.execute("UPDATE config SET dirty=1", []).map_err(io)?;
        let tx = conn.transaction().map_err(io)?;
        tx.execute_batch("DELETE FROM pinned;DELETE FROM candidates;CREATE TEMP TABLE live(kind TEXT,id TEXT,PRIMARY KEY(kind,id));")
            .map_err(io)?;
        let mut repairs = 0u64;
        for entry in fs::read_dir(&self.root).map_err(io)? {
            let entry = entry.map_err(io)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if owned_temp(name, ".snapshot-import-") {
                directory(&entry.path())?;
                fs::remove_dir_all(entry.path()).map_err(io)?;
                repairs += 1;
            } else if owned_temp(name, ".kasu-scope-") {
                regular(&entry.path())?;
                fs::remove_file(entry.path()).map_err(io)?;
                repairs += 1;
            }
        }
        sync(&self.root)?;
        for kind in [Kind::Tree, Kind::Blob] {
            let folder = self.root.join("index").join(kind.name());
            directory(&folder)?;
            for entry in fs::read_dir(&folder).map_err(io)? {
                let entry = entry.map_err(io)?;
                let name = entry.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| Status::data_loss("invalid cache index filename"))?;
                if owned_temp(name, ".kasu-index-") {
                    regular(&entry.path())?;
                    fs::remove_file(entry.path()).map_err(io)?;
                    repairs += 1;
                    continue;
                }
                if name.starts_with('.') {
                    continue;
                }
                let id: ObjectId = name
                    .parse()
                    .map_err(|_| Status::data_loss("invalid cache index filename"))?;
                if id.to_string() != name {
                    return Err(Status::data_loss("noncanonical cache index filename"));
                }
                let size = self
                    .marker_size(Key { kind, id })?
                    .ok_or_else(|| Status::data_loss("cache marker disappeared"))?;
                let path = self.frame(id);
                let parent = path.parent().unwrap();
                if parent
                    .symlink_metadata()
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                {
                    fs::remove_file(entry.path()).map_err(io)?;
                    repairs += 1;
                    continue;
                }
                directory(parent)?;
                let Some(meta) = regular(&path)? else {
                    fs::remove_file(entry.path()).map_err(io)?;
                    repairs += 1;
                    continue;
                };
                if meta.len() != size {
                    return Err(Status::data_loss("disk cache frame size mismatch"));
                }
                tx.execute("INSERT INTO frames(id,size,accessed) VALUES(?1,?2,0) ON CONFLICT(id) DO UPDATE SET size=excluded.size",params![name,size]).map_err(io)?;
                tx.execute(
                    "INSERT INTO live(kind,id) VALUES(?1,?2)",
                    params![kind.name(), name],
                )
                .map_err(io)?;
            }
            sync(&folder)?;
        }
        tx.execute_batch("DELETE FROM kinds;INSERT INTO kinds SELECT kind,id FROM live;DELETE FROM frames WHERE id NOT IN(SELECT id FROM kinds);DROP TABLE live;").map_err(io)?;
        // Rebuild aggregate values as well as keys after interrupted transactions.
        tx.execute_batch("UPDATE config SET bytes=(SELECT COALESCE(SUM(size),0) FROM frames),entries=(SELECT COUNT(*) FROM kinds);").map_err(io)?;
        let objects = self.root.join("objects");
        directory(&objects)?;
        for prefix in fs::read_dir(&objects).map_err(io)? {
            let prefix = prefix.map_err(io)?;
            let name = prefix.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| Status::data_loss("invalid object directory"))?;
            if name.starts_with('.') {
                continue;
            }
            if name.len() != 2
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err(Status::data_loss("invalid object directory"));
            }
            directory(&prefix.path())?;
            for file in fs::read_dir(prefix.path()).map_err(io)? {
                let file = file.map_err(io)?;
                let leaf = file.file_name();
                let leaf = leaf
                    .to_str()
                    .ok_or_else(|| Status::data_loss("invalid disk frame name"))?;
                if owned_temp(leaf, ".kasu-frame-") {
                    regular(&file.path())?;
                    fs::remove_file(file.path()).map_err(io)?;
                    repairs += 1;
                    continue;
                }
                if leaf.starts_with('.') {
                    continue;
                }
                let combined = format!("{name}{leaf}");
                let id: ObjectId = combined
                    .parse()
                    .map_err(|_| Status::data_loss("invalid disk frame name"))?;
                if id.to_string() != combined {
                    return Err(Status::data_loss("noncanonical disk frame name"));
                }
                regular(&file.path())?;
                let used: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM frames WHERE id=?1)",
                        [combined],
                        |r| r.get(0),
                    )
                    .map_err(io)?;
                if !used {
                    fs::remove_file(file.path()).map_err(io)?;
                    repairs += 1;
                }
            }
            sync(&prefix.path())?;
        }
        if !self.make_room(&tx, 0, 0, None)? {
            return Err(Status::data_loss("cannot enforce disk cache limits"));
        }
        tx.execute(
            "UPDATE config SET dirty=0,repairs=CASE WHEN repairs>9223372036854775807-?1 THEN 9223372036854775807 ELSE repairs+?1 END",
            [repairs],
        )
        .map_err(io)?;
        tx.commit().map_err(io)?;
        Ok(())
    }
}
/// (normalized repository, its upstream identity, confirmed key)
pub(super) type Membership = (Vec<u8>, Option<String>, Key);
impl DiskStore {
    pub(super) fn add_member_locked(
        &self,
        conn: &Connection,
        repo: &[u8],
        key: Key,
    ) -> Result<(), Status> {
        conn.execute(
            "INSERT OR IGNORE INTO members(repo,kind,id) VALUES(?1,?2,?3)",
            params![repo, key.kind.name(), key.id.to_string()],
        )
        .map_err(io)?;
        // Memberships outlive evicted frames; keep them bounded by the catalog.
        let (members, limit): (u64, u64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM members),max_entries FROM config",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(io)?;
        if members > limit.saturating_mul(4) {
            conn.execute(
                "DELETE FROM members WHERE NOT EXISTS(SELECT 1 FROM kinds WHERE kinds.kind=members.kind AND kinds.id=members.id)",
                [],
            )
            .map_err(io)?;
        }
        Ok(())
    }
    pub(super) fn identify_locked(
        &self,
        conn: &mut Connection,
        repo: &[u8],
        identity: &str,
    ) -> Result<(), Status> {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(io)?;
        let known: Option<String> = tx
            .query_row(
                "SELECT identity FROM identities WHERE repo=?1",
                [repo],
                |r| r.get(0),
            )
            .optional()
            .map_err(io)?;
        if known.as_deref() != Some(identity) {
            if known.is_some() {
                tx.execute("DELETE FROM members WHERE repo=?1", [repo])
                    .map_err(io)?;
            }
            tx.execute(
                "INSERT INTO identities(repo,identity) VALUES(?1,?2) ON CONFLICT(repo) DO UPDATE SET identity=excluded.identity",
                params![repo, identity],
            )
            .map_err(io)?;
        }
        tx.commit().map_err(io)
    }
    /// Memberships by normalized repository name.
    pub(super) fn members_locked(&self, conn: &Connection) -> Result<Vec<Membership>, Status> {
        let mut statement = conn
            .prepare("SELECT members.repo,identities.identity,kind,id FROM members LEFT JOIN identities USING(repo) ORDER BY members.repo,kind,id")
            .map_err(io)?;
        let rows = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(io)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(io)?;
        rows.into_iter()
            .map(|(repo, identity, kind, id)| {
                Ok((
                    repo,
                    identity,
                    Key {
                        kind: match kind.as_str() {
                            "trees" => Kind::Tree,
                            "blobs" => Kind::Blob,
                            _ => return Err(Status::data_loss("invalid disk catalog kind")),
                        },
                        id: id.parse().map_err(io)?,
                    },
                ))
            })
            .collect()
    }
}
impl Store for DiskStore {
    fn member(&self, repo: &[u8], key: Key) -> Result<bool, Status> {
        let conn = self.guard(false)?;
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM members WHERE repo=?1 AND kind=?2 AND id=?3)",
            params![repo, key.kind.name(), key.id.to_string()],
            |r| r.get(0),
        )
        .map_err(io)
    }
    fn add_member(&self, repo: &[u8], key: Key) -> Result<(), Status> {
        let conn = self.guard(false)?;
        self.add_member_locked(&conn, repo, key)
    }
    fn identify(&self, repo: &[u8], identity: &str) -> Result<(), Status> {
        let mut conn = self.guard(false)?;
        self.identify_locked(&mut conn, repo, identity)
    }
    fn identity(&self, repo: &[u8]) -> Result<Option<String>, Status> {
        let conn = self.guard(false)?;
        conn.query_row(
            "SELECT identity FROM identities WHERE repo=?1",
            [repo],
            |r| r.get(0),
        )
        .optional()
        .map_err(io)
    }
    fn get(&self, key: Key) -> Result<Option<Frame>, Status> {
        let mut conn = self.guard(false)?;
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM kinds WHERE kind=?1 AND id=?2)",
                params![key.kind.name(), key.id.to_string()],
                |r| r.get(0),
            )
            .map_err(io)?;
        if !exists {
            return Ok(None);
        }
        let bytes = self.get_locked(key)?;
        if bytes.is_some() {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(io)?;
            let tick = Self::tick(&tx)?;
            tx.execute(
                "UPDATE frames SET accessed=?1 WHERE id=?2",
                params![tick, key.id.to_string()],
            )
            .map_err(io)?;
            tx.commit().map_err(io)?;
        }
        Ok(bytes)
    }
    fn put(&self, key: Key, bytes: Frame) -> Result<(), Status> {
        kasu_object::validate(key.kind, key.id, &bytes)?;
        let mut guard = self.guard(true)?;
        self.put_locked(&mut guard, key, bytes)
    }
    fn disk_stats(&self) -> Option<DiskStats> {
        Some(self.cached_stats())
    }
    fn usage(&self) -> (usize, usize) {
        let stats = self.cached_stats();
        (stats.entries as usize, stats.bytes as usize)
    }
}
