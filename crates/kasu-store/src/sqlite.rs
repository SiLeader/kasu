use crate::*;
mod webhook;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

pub struct SqliteStore {
    connection: Mutex<Connection>,
}
impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_connection(Connection::open(path).map_err(storage_error)?)
    }
    pub fn memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory().map_err(storage_error)?)
    }
    fn from_connection(connection: Connection) -> Result<Self> {
        connection
            .busy_timeout(Duration::from_secs(30))
            .map_err(storage_error)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS immutable (key BLOB PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;
          CREATE TABLE IF NOT EXISTS refs (repo BLOB NOT NULL, name BLOB NOT NULL, target BLOB, version INTEGER NOT NULL, PRIMARY KEY(repo,name)) WITHOUT ROWID;
          CREATE TABLE IF NOT EXISTS ref_log (seq INTEGER PRIMARY KEY AUTOINCREMENT, repo BLOB NOT NULL, name BLOB NOT NULL, old BLOB, new BLOB, actor BLOB NOT NULL, time INTEGER NOT NULL);
          CREATE INDEX IF NOT EXISTS ref_log_repo ON ref_log(repo,seq);
          CREATE TABLE IF NOT EXISTS git_refs (repo BLOB NOT NULL, name BLOB NOT NULL, value BLOB, version INTEGER NOT NULL, PRIMARY KEY(repo,name)) WITHOUT ROWID;
          CREATE TABLE IF NOT EXISTS git_ref_log (seq INTEGER PRIMARY KEY AUTOINCREMENT, repo BLOB NOT NULL, name BLOB NOT NULL, old BLOB, new BLOB, actor BLOB NOT NULL, time INTEGER NOT NULL);
          CREATE INDEX IF NOT EXISTS git_ref_log_repo ON git_ref_log(repo,seq);
          CREATE TABLE IF NOT EXISTS land_results (repo BLOB NOT NULL, key BLOB NOT NULL, fingerprint BLOB NOT NULL, response BLOB NOT NULL, PRIMARY KEY(repo,key)) WITHOUT ROWID;
          CREATE TABLE IF NOT EXISTS webhook_state (repo BLOB NOT NULL, consumer TEXT NOT NULL, stream INTEGER NOT NULL, cursor INTEGER NOT NULL, sequence INTEGER, body BLOB, attempts INTEGER NOT NULL DEFAULT 0, retry_at_ms INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(repo,consumer,stream)) WITHOUT ROWID;").map_err(storage_error)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| storage_error("sqlite mutex poisoned"))
    }
}
fn target(raw: Option<Vec<u8>>) -> Result<Option<ChangesetId>> {
    raw.map(|raw| {
        raw.try_into()
            .map(ChangesetId)
            .map_err(|_| invalid("corrupt ref id"))
    })
    .transpose()
}
fn resolve(connection: &Connection, repo: &[u8], name: &[u8]) -> Result<RefState> {
    let row: Option<(Option<Vec<u8>>, i64)> = connection
        .query_row(
            "SELECT target,version FROM refs WHERE repo=? AND name=?",
            params![repo, name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    match row {
        Some((raw, version)) => Ok(RefState {
            target: target(raw)?,
            version: u64::try_from(version).map_err(|_| invalid("negative ref version"))?,
        }),
        None => Ok(RefState::default()),
    }
}
impl MetaStore for SqliteStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.lock()?
            .query_row("SELECT value FROM immutable WHERE key=?", [key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(storage_error)
    }
    fn put_if_absent(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR IGNORE INTO immutable(key,value) VALUES (?,?)",
            params![key, value],
        )
        .map_err(storage_error)?;
        let stored: Vec<u8> = conn
            .query_row("SELECT value FROM immutable WHERE key=?", [key], |r| {
                r.get(0)
            })
            .map_err(storage_error)?;
        if stored != value {
            return Err(Error::Collision(String::from_utf8_lossy(key).into()));
        }
        Ok(())
    }
}
impl RefStore for SqliteStore {
    fn land_record(&self, repo: &[u8], key: &[u8]) -> Result<Option<LandRecord>> {
        read_land(&*self.lock()?, repo, key)
    }
    fn commit_land(
        &self,
        repo: &[u8],
        key: &[u8],
        commit: &LandCommit,
        actor: &[u8],
        time: i64,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<LandCommitResult> {
        if key.is_empty() || key.len() > 256 || commit.name.is_empty() || commit.name.contains(&0) {
            return Err(invalid("invalid Land key or ref name"));
        }
        let mut connection = self.lock()?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        if let Some(record) = read_land(&tx, repo, key)? {
            if record.fingerprint != commit.record.fingerprint {
                return Err(invalid("Land key was used for a different request"));
            }
            return Ok(LandCommitResult::Recorded {
                record,
                replayed: true,
            });
        }
        if cancelled() {
            return Ok(LandCommitResult::Cancelled);
        }
        let actual = resolve(&tx, repo, &commit.name)?;
        if actual != commit.expected {
            return Ok(LandCommitResult::Mismatch(actual));
        }
        if let Some(new) = commit.new {
            let version = actual
                .version
                .checked_add(1)
                .filter(|v| *v <= i64::MAX as u64)
                .ok_or_else(|| invalid("ref version exhausted"))?;
            tx.execute("INSERT INTO refs(repo,name,target,version) VALUES (?,?,?,?) ON CONFLICT(repo,name) DO UPDATE SET target=excluded.target,version=excluded.version",
                params![repo, commit.name, new.0.as_slice(), version as i64]).map_err(storage_error)?;
            tx.execute(
                "INSERT INTO ref_log(repo,name,old,new,actor,time) VALUES (?,?,?,?,?,?)",
                params![
                    repo,
                    commit.name,
                    actual.target.map(|id| id.0.to_vec()),
                    new.0.as_slice(),
                    actor,
                    time
                ],
            )
            .map_err(storage_error)?;
        }
        tx.execute(
            "INSERT INTO land_results(repo,key,fingerprint,response) VALUES (?,?,?,?)",
            params![
                repo,
                key,
                commit.record.fingerprint.0.as_slice(),
                commit.record.response
            ],
        )
        .map_err(storage_error)?;
        if cancelled() {
            return Ok(LandCommitResult::Cancelled);
        }
        tx.commit().map_err(storage_error)?;
        Ok(LandCommitResult::Recorded {
            record: commit.record.clone(),
            replayed: false,
        })
    }
    fn capabilities(&self) -> RefCapabilities {
        RefCapabilities { atomic_batch: true }
    }
    fn resolve(&self, repo: &[u8], name: &[u8]) -> Result<RefState> {
        resolve(&*self.lock()?, repo, name)
    }
    fn list(&self, repo: &[u8], prefix: &[u8]) -> Result<Vec<(Vec<u8>, RefState)>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT name,target,version FROM refs WHERE repo=? AND target IS NOT NULL ORDER BY name").map_err(storage_error)?;
        let rows = stmt
            .query_map([repo], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Option<Vec<u8>>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(storage_error)?;
        let mut out = Vec::new();
        for row in rows {
            let (name, raw, version) = row.map_err(storage_error)?;
            if name.starts_with(prefix) {
                out.push((
                    name,
                    RefState {
                        target: target(raw)?,
                        version: u64::try_from(version)
                            .map_err(|_| invalid("negative ref version"))?,
                    },
                ));
            }
        }
        Ok(out)
    }
    fn compare_and_swap(
        &self,
        repo: &[u8],
        updates: &[RefUpdate],
        actor: &[u8],
        time: i64,
    ) -> Result<CasResult> {
        let mut names = BTreeSet::new();
        for update in updates {
            if update.name.is_empty() || update.name.contains(&0) || !names.insert(&update.name) {
                return Err(invalid("invalid or duplicate ref name"));
            }
        }
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let mut result = Vec::new();
        for update in updates {
            let actual = resolve(&tx, repo, &update.name)?;
            if actual != update.expected {
                return Ok(CasResult::Mismatch {
                    name: update.name.clone(),
                    actual,
                });
            }
            let version = actual
                .version
                .checked_add(1)
                .filter(|v| *v <= i64::MAX as u64)
                .ok_or_else(|| invalid("ref version exhausted"))?;
            result.push(RefState {
                target: update.new,
                version,
            });
        }
        for (update, state) in updates.iter().zip(&result) {
            let old = update.expected.target.map(|id| id.0.to_vec());
            let new = update.new.map(|id| id.0.to_vec());
            tx.execute("INSERT INTO refs(repo,name,target,version) VALUES (?,?,?,?) ON CONFLICT(repo,name) DO UPDATE SET target=excluded.target,version=excluded.version", params![repo, update.name, new, state.version as i64]).map_err(storage_error)?;
            tx.execute(
                "INSERT INTO ref_log(repo,name,old,new,actor,time) VALUES (?,?,?,?,?,?)",
                params![repo, update.name, old, new, actor, time],
            )
            .map_err(storage_error)?;
        }
        tx.commit().map_err(storage_error)?;
        tracing::debug!(count = updates.len(), "ref transaction committed");
        Ok(CasResult::Applied(result))
    }
    fn tail(&self, repo: &[u8], after: u64, limit: usize) -> Result<Vec<RefLog>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT seq,name,old,new,actor,time FROM ref_log WHERE repo=? AND seq>? ORDER BY seq LIMIT ?").map_err(storage_error)?;
        let rows = stmt
            .query_map(
                params![
                    repo,
                    after.min(i64::MAX as u64) as i64,
                    limit.min(i64::MAX as usize) as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get(1)?,
                        r.get::<_, Option<Vec<u8>>>(2)?,
                        r.get::<_, Option<Vec<u8>>>(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .map_err(storage_error)?;
        rows.map(|row| {
            let (seq, name, old, new, actor, time) = row.map_err(storage_error)?;
            Ok(RefLog {
                seq: u64::try_from(seq).map_err(|_| invalid("negative log sequence"))?,
                name,
                old: target(old)?,
                new: target(new)?,
                actor,
                time,
            })
        })
        .collect()
    }
}
fn read_land(connection: &Connection, repo: &[u8], key: &[u8]) -> Result<Option<LandRecord>> {
    let row: Option<(Vec<u8>, Vec<u8>)> = connection
        .query_row(
            "SELECT fingerprint,response FROM land_results WHERE repo=? AND key=?",
            params![repo, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    row.map(|(fingerprint, response)| {
        Ok(LandRecord {
            fingerprint: ObjectId(
                fingerprint
                    .try_into()
                    .map_err(|_| invalid("corrupt Land fingerprint"))?,
            ),
            response,
        })
    })
    .transpose()
}

fn decode_git(raw: Option<Vec<u8>>) -> Result<Option<kasu_core::GitRef>> {
    raw.map(|raw| kasu_core::GitRef::decode(&raw)).transpose()
}
fn resolve_git(connection: &Connection, repo: &[u8], name: &[u8]) -> Result<GitRefState> {
    let row: Option<(Option<Vec<u8>>, i64)> = connection
        .query_row(
            "SELECT value,version FROM git_refs WHERE repo=? AND name=?",
            params![repo, name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    match row {
        Some((raw, version)) => Ok(GitRefState {
            value: decode_git(raw)?,
            version: version
                .try_into()
                .map_err(|_| invalid("negative Git ref version"))?,
        }),
        None => Ok(GitRefState::default()),
    }
}
impl GitRefStore for SqliteStore {
    fn supports_atomic_publication(&self) -> bool {
        true
    }
    fn compare_native_and_git(
        &self,
        repo: &[u8],
        native: &[RefUpdate],
        git: &[GitRefUpdate],
        actor: &[u8],
        time: i64,
    ) -> Result<RefPublicationResult> {
        let mut names = BTreeSet::new();
        for name in native
            .iter()
            .map(|u| &u.name)
            .chain(git.iter().map(|u| &u.name))
        {
            if name.is_empty() || name.contains(&0) || !names.insert(name) {
                return Err(invalid("invalid or duplicate publication ref name"));
            }
        }
        if git.iter().any(|u| u.name.starts_with(b"refs/heads/")) {
            return Err(invalid("Git branches must use native ref CAS"));
        }
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let mut native_states = Vec::new();
        let mut git_states = Vec::new();
        for update in native {
            let actual = resolve(&tx, repo, &update.name)?;
            if actual != update.expected {
                return Ok(RefPublicationResult::NativeMismatch {
                    name: update.name.clone(),
                    actual,
                });
            }
            let version = actual
                .version
                .checked_add(1)
                .filter(|v| *v <= i64::MAX as u64)
                .ok_or_else(|| invalid("ref version exhausted"))?;
            native_states.push(RefState {
                target: update.new,
                version,
            });
        }
        for update in git {
            let actual = resolve_git(&tx, repo, &update.name)?;
            if actual != update.expected {
                return Ok(RefPublicationResult::GitMismatch {
                    name: update.name.clone(),
                    actual,
                });
            }
            let version = actual
                .version
                .checked_add(1)
                .filter(|v| *v <= i64::MAX as u64)
                .ok_or_else(|| invalid("Git ref version exhausted"))?;
            git_states.push(GitRefState {
                value: update.new.clone(),
                version,
            });
        }
        for (update, state) in native.iter().zip(&native_states) {
            let old = update.expected.target.map(|id| id.0.to_vec());
            let new = update.new.map(|id| id.0.to_vec());
            tx.execute("INSERT INTO refs(repo,name,target,version) VALUES (?,?,?,?) ON CONFLICT(repo,name) DO UPDATE SET target=excluded.target,version=excluded.version", params![repo,update.name,new,state.version as i64]).map_err(storage_error)?;
            tx.execute(
                "INSERT INTO ref_log(repo,name,old,new,actor,time) VALUES (?,?,?,?,?,?)",
                params![repo, update.name, old, new, actor, time],
            )
            .map_err(storage_error)?;
        }
        for (update, state) in git.iter().zip(&git_states) {
            let old = update.expected.value.as_ref().map(Canonical::encode);
            let new = update.new.as_ref().map(Canonical::encode);
            tx.execute("INSERT INTO git_refs(repo,name,value,version) VALUES (?,?,?,?) ON CONFLICT(repo,name) DO UPDATE SET value=excluded.value,version=excluded.version",params![repo,update.name,new,state.version as i64]).map_err(storage_error)?;
            tx.execute(
                "INSERT INTO git_ref_log(repo,name,old,new,actor,time) VALUES (?,?,?,?,?,?)",
                params![repo, update.name, old, new, actor, time],
            )
            .map_err(storage_error)?;
        }
        tx.commit().map_err(storage_error)?;
        Ok(RefPublicationResult::Applied {
            native: native_states,
            git: git_states,
        })
    }
    fn resolve_git(&self, repo: &[u8], name: &[u8]) -> Result<GitRefState> {
        resolve_git(&*self.lock()?, repo, name)
    }
    fn list_git(&self, repo: &[u8], prefix: &[u8]) -> Result<Vec<(Vec<u8>, GitRefState)>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT name,value,version FROM git_refs WHERE repo=? AND value IS NOT NULL ORDER BY name").map_err(storage_error)?;
        let rows = stmt
            .query_map([repo], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Option<Vec<u8>>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(storage_error)?;
        let mut out = Vec::new();
        for row in rows {
            let (name, raw, version) = row.map_err(storage_error)?;
            if name.starts_with(prefix) {
                out.push((
                    name,
                    GitRefState {
                        value: decode_git(raw)?,
                        version: version
                            .try_into()
                            .map_err(|_| invalid("negative Git ref version"))?,
                    },
                ));
            }
        }
        Ok(out)
    }
    fn compare_git(
        &self,
        repo: &[u8],
        updates: &[GitRefUpdate],
        actor: &[u8],
        time: i64,
    ) -> Result<GitCasResult> {
        let mut names = BTreeSet::new();
        for update in updates {
            if update.name.is_empty() || update.name.contains(&0) || !names.insert(&update.name) {
                return Err(invalid("invalid or duplicate Git ref name"));
            }
            // Branches are always projections of native refs, never separate mutable copies.
            if update.name.starts_with(b"refs/heads/") {
                return Err(invalid("Git branches must use native ref CAS"));
            }
        }
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let mut result = Vec::new();
        for update in updates {
            let actual = resolve_git(&tx, repo, &update.name)?;
            if actual != update.expected {
                return Ok(GitCasResult::Mismatch {
                    name: update.name.clone(),
                    actual,
                });
            }
            let version = actual
                .version
                .checked_add(1)
                .filter(|v| *v <= i64::MAX as u64)
                .ok_or_else(|| invalid("Git ref version exhausted"))?;
            result.push(GitRefState {
                value: update.new.clone(),
                version,
            });
        }
        for (update, state) in updates.iter().zip(&result) {
            let old = update.expected.value.as_ref().map(Canonical::encode);
            let new = update.new.as_ref().map(Canonical::encode);
            tx.execute("INSERT INTO git_refs(repo,name,value,version) VALUES (?,?,?,?) ON CONFLICT(repo,name) DO UPDATE SET value=excluded.value,version=excluded.version",params![repo,update.name,new,state.version as i64]).map_err(storage_error)?;
            tx.execute(
                "INSERT INTO git_ref_log(repo,name,old,new,actor,time) VALUES (?,?,?,?,?,?)",
                params![repo, update.name, old, new, actor, time],
            )
            .map_err(storage_error)?;
        }
        tx.commit().map_err(storage_error)?;
        Ok(GitCasResult::Applied(result))
    }
    fn tail_git(&self, repo: &[u8], after: u64, limit: usize) -> Result<Vec<GitRefLog>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT seq,name,old,new,actor,time FROM git_ref_log WHERE repo=? AND seq>? ORDER BY seq LIMIT ?").map_err(storage_error)?;
        let rows = stmt
            .query_map(
                params![
                    repo,
                    after.min(i64::MAX as u64) as i64,
                    limit.min(i64::MAX as usize) as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get(1)?,
                        r.get::<_, Option<Vec<u8>>>(2)?,
                        r.get::<_, Option<Vec<u8>>>(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .map_err(storage_error)?;
        rows.map(|row| {
            let (seq, name, old, new, actor, time) = row.map_err(storage_error)?;
            Ok(GitRefLog {
                seq: seq
                    .try_into()
                    .map_err(|_| invalid("negative Git ref seq"))?,
                name,
                old: decode_git(old)?,
                new: decode_git(new)?,
                actor,
                time,
            })
        })
        .collect()
    }
}
