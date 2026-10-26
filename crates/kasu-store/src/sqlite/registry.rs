use super::SqliteStore;
use crate::registry::{check_update, require_deleting};
use crate::*;
use kasu_core::{RepoKey, RepoName, RepositoryState};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

pub(super) const SCHEMA: &str = "
  CREATE TABLE IF NOT EXISTS repository_records (key BLOB PRIMARY KEY, record BLOB NOT NULL, state INTEGER NOT NULL, version INTEGER NOT NULL) WITHOUT ROWID;
  CREATE TABLE IF NOT EXISTS repository_names (name TEXT PRIMARY KEY, key BLOB NOT NULL, alias INTEGER NOT NULL, expires_at INTEGER) WITHOUT ROWID;
  CREATE INDEX IF NOT EXISTS repository_names_key ON repository_names(key);
  CREATE TABLE IF NOT EXISTS repository_log (seq INTEGER PRIMARY KEY AUTOINCREMENT, key BLOB NOT NULL, event TEXT NOT NULL, actor BLOB NOT NULL, time INTEGER NOT NULL);";

fn record(connection: &Connection, key: &[u8]) -> Result<Option<RepositoryRecord>> {
    let row: Option<(Vec<u8>, i64)> = connection
        .query_row(
            "SELECT record,version FROM repository_records WHERE key=?",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    row.map(|(raw, version)| decode(&raw, version)).transpose()
}
fn decode(raw: &[u8], version: i64) -> Result<RepositoryRecord> {
    let mut record = RepositoryRecord::decode(raw)?;
    record.version = version
        .try_into()
        .map_err(|_| invalid("negative repository version"))?;
    Ok(record)
}
fn name(connection: &Connection, normalized: &str) -> Result<Option<RepositoryName>> {
    let row: Option<(Vec<u8>, bool, Option<i64>)> = connection
        .query_row(
            "SELECT key,alias,expires_at FROM repository_names WHERE name=?",
            [normalized],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(storage_error)?;
    row.map(|(key, alias, expires_at)| {
        Ok(RepositoryName {
            name: normalized.to_owned(),
            key: RepoKey::from_bytes(key)?,
            alias,
            expires_at,
        })
    })
    .transpose()
}
fn store(connection: &Connection, record: &RepositoryRecord, version: u64) -> Result<()> {
    connection
        .execute(
            "INSERT INTO repository_records(key,record,state,version) VALUES (?,?,?,?) ON CONFLICT(key) DO UPDATE SET record=excluded.record,state=excluded.state,version=excluded.version",
            params![
                record.key.as_bytes(),
                record.encode(),
                record.state.code(),
                i64::try_from(version).map_err(|_| invalid("repository version exhausted"))?
            ],
        )
        .map_err(storage_error)?;
    Ok(())
}
fn append(
    connection: &Connection,
    key: &RepoKey,
    event: &str,
    actor: &[u8],
    time: i64,
) -> Result<()> {
    connection
        .execute(
            "INSERT INTO repository_log(key,event,actor,time) VALUES (?,?,?,?)",
            params![key.as_bytes(), event, actor, time],
        )
        .map_err(storage_error)?;
    Ok(())
}
fn set_name(connection: &Connection, value: &RepositoryName) -> Result<()> {
    connection
        .execute(
            "INSERT INTO repository_names(name,key,alias,expires_at) VALUES (?,?,?,?) ON CONFLICT(name) DO UPDATE SET key=excluded.key,alias=excluded.alias,expires_at=excluded.expires_at",
            params![value.name, value.key.as_bytes(), value.alias, value.expires_at],
        )
        .map_err(storage_error)?;
    Ok(())
}
/// Registered repositories accept ref transactions only while Active. Keys that
/// were never registered keep the pre-registry single-repository behavior.
pub(super) fn require_writable(connection: &Connection, repo: &[u8]) -> Result<()> {
    let state: Option<u8> = connection
        .query_row(
            "SELECT state FROM repository_records WHERE key=?",
            [repo],
            |r| r.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    match state {
        Some(code) => RepositoryState::from_code(code)?.require_writable(),
        None => Ok(()),
    }
}

impl RepositoryStore for SqliteStore {
    fn create_repository(&self, value: &RepositoryRecord) -> Result<RepositoryWrite> {
        value.validate()?;
        let mut connection = self.lock()?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let normalized = value.name.normalized();
        if record(&tx, value.key.as_bytes())?.is_some()
            || name(&tx, &normalized)?.is_some_and(|n| n.live(value.created_at))
        {
            return Ok(RepositoryWrite::NameTaken);
        }
        let mut stored = value.clone();
        stored.version = 1;
        store(&tx, &stored, 1)?;
        set_name(
            &tx,
            &RepositoryName {
                name: normalized,
                key: value.key.clone(),
                alias: false,
                expires_at: None,
            },
        )?;
        append(
            &tx,
            &value.key,
            "created",
            &value.created_by,
            value.created_at,
        )?;
        tx.commit().map_err(storage_error)?;
        Ok(RepositoryWrite::Applied(stored))
    }
    fn repository(&self, key: &RepoKey) -> Result<Option<RepositoryRecord>> {
        record(&*self.lock()?, key.as_bytes())
    }
    fn resolve_repository_name(
        &self,
        value: &RepoName,
        now: i64,
    ) -> Result<Option<RepositoryName>> {
        Ok(name(&*self.lock()?, &value.normalized())?.filter(|n| n.live(now)))
    }
    fn list_repository_names(
        &self,
        prefix: &str,
        after: &str,
        limit: usize,
    ) -> Result<Vec<RepositoryName>> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare("SELECT name,key FROM repository_names WHERE alias=0 AND name>? AND substr(name,1,?)=? ORDER BY name LIMIT ?")
            .map_err(storage_error)?;
        let rows = statement
            .query_map(
                params![
                    after,
                    prefix.len() as i64,
                    prefix,
                    limit.min(i64::MAX as usize) as i64
                ],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .map_err(storage_error)?;
        rows.map(|row| {
            let (name, key) = row.map_err(storage_error)?;
            Ok(RepositoryName {
                name,
                key: RepoKey::from_bytes(key)?,
                alias: false,
                expires_at: None,
            })
        })
        .collect()
    }
    fn list_repositories(
        &self,
        after: Option<&RepoKey>,
        limit: usize,
    ) -> Result<Vec<RepositoryRecord>> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT record,version FROM repository_records WHERE key>? ORDER BY key LIMIT ?",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map(
                params![
                    after.map(RepoKey::as_bytes).unwrap_or_default(),
                    limit.min(i64::MAX as usize) as i64
                ],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
            )
            .map_err(storage_error)?;
        rows.map(|row| {
            let (raw, version) = row.map_err(storage_error)?;
            decode(&raw, version)
        })
        .collect()
    }
    fn update_repository(
        &self,
        value: &RepositoryRecord,
        event: &str,
        actor: &[u8],
        time: i64,
    ) -> Result<RepositoryWrite> {
        let mut connection = self.lock()?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let Some(current) = record(&tx, value.key.as_bytes())? else {
            return Ok(RepositoryWrite::Mismatch(None));
        };
        if current.version != value.version {
            return Ok(RepositoryWrite::Mismatch(Some(current)));
        }
        check_update(&current, value)?;
        let mut stored = value.clone();
        stored.version = current
            .version
            .checked_add(1)
            .ok_or_else(|| invalid("repository version exhausted"))?;
        store(&tx, &stored, stored.version)?;
        if stored.state == RepositoryState::Deleting {
            tx.execute(
                "DELETE FROM repository_names WHERE key=?",
                [value.key.as_bytes()],
            )
            .map_err(storage_error)?;
        }
        append(&tx, &value.key, event, actor, time)?;
        tx.commit().map_err(storage_error)?;
        Ok(RepositoryWrite::Applied(stored))
    }
    fn rename_repository(
        &self,
        key: &RepoKey,
        expected_version: u64,
        value: &RepoName,
        alias_expires_at: Option<i64>,
        actor: &[u8],
        time: i64,
    ) -> Result<RepositoryWrite> {
        let mut connection = self.lock()?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let Some(current) = record(&tx, key.as_bytes())? else {
            return Ok(RepositoryWrite::Mismatch(None));
        };
        if current.version != expected_version || current.state == RepositoryState::Deleting {
            return Ok(RepositoryWrite::Mismatch(Some(current)));
        }
        let normalized = value.normalized();
        let old = current.name.normalized();
        if normalized != old
            && name(&tx, &normalized)?.is_some_and(|n| n.live(time) && n.key != *key)
        {
            return Ok(RepositoryWrite::NameTaken);
        }
        tx.execute("DELETE FROM repository_names WHERE name=?", [&old])
            .map_err(storage_error)?;
        if normalized != old
            && let Some(expires_at) = alias_expires_at
        {
            set_name(
                &tx,
                &RepositoryName {
                    name: old,
                    key: key.clone(),
                    alias: true,
                    expires_at: (expires_at != i64::MAX).then_some(expires_at),
                },
            )?;
        }
        set_name(
            &tx,
            &RepositoryName {
                name: normalized,
                key: key.clone(),
                alias: false,
                expires_at: None,
            },
        )?;
        let mut stored = current.renamed(value);
        stored.version = stored
            .version
            .checked_add(1)
            .ok_or_else(|| invalid("repository version exhausted"))?;
        store(&tx, &stored, stored.version)?;
        append(&tx, key, "renamed", actor, time)?;
        tx.commit().map_err(storage_error)?;
        Ok(RepositoryWrite::Applied(stored))
    }
    fn tail_repositories(&self, after: u64, limit: usize) -> Result<Vec<RepositoryEvent>> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare("SELECT seq,key,event,actor,time FROM repository_log WHERE seq>? ORDER BY seq LIMIT ?")
            .map_err(storage_error)?;
        let rows = statement
            .query_map(
                params![
                    after.min(i64::MAX as u64) as i64,
                    limit.min(i64::MAX as usize) as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .map_err(storage_error)?;
        rows.map(|row| {
            let (seq, key, event, actor, time) = row.map_err(storage_error)?;
            Ok(RepositoryEvent {
                seq: seq
                    .try_into()
                    .map_err(|_| invalid("negative repository log sequence"))?,
                key: RepoKey::from_bytes(key)?,
                event,
                actor,
                time,
            })
        })
        .collect()
    }
    fn purge_repository_refs(&self, key: &RepoKey, ref_logs: bool) -> Result<()> {
        let mut connection = self.lock()?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        require_deleting(record(&tx, key.as_bytes())?.as_ref())?;
        let mut tables = vec!["refs", "git_refs", "land_results", "webhook_state"];
        if ref_logs {
            tables.extend(["ref_log", "git_ref_log"]);
        }
        for table in tables {
            tx.execute(
                &format!("DELETE FROM {table} WHERE repo=?"),
                [key.as_bytes()],
            )
            .map_err(storage_error)?;
        }
        tx.commit().map_err(storage_error)
    }
}
