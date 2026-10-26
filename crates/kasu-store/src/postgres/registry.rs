use super::*;
use crate::registry::{check_update, require_deleting};
use kasu_core::{RepoKey, RepoName, RepositoryState};

/// Immutable keys are range-scanned through this bounded expression index, so
/// long Derived Data keys never exceed the B-tree entry size.
pub(super) const KEY_INDEX_BYTES: usize = 96;

fn decode(raw: Vec<u8>, version: i64) -> Result<RepositoryRecord> {
    let mut record = RepositoryRecord::decode(&raw)?;
    record.version = unsigned(version)?;
    Ok(record)
}
fn record(
    client: &mut impl GenericClient,
    key: &[u8],
    lock: &str,
) -> Result<Option<RepositoryRecord>> {
    client
        .query_opt(
            &format!("SELECT record,version FROM kasu.repository_records WHERE key=$1 {lock}"),
            &[&key],
        )
        .map_err(pg_error)?
        .map(|row| decode(row.get(0), row.get(1)))
        .transpose()
}
fn name(client: &mut impl GenericClient, normalized: &str) -> Result<Option<RepositoryName>> {
    client
        .query_opt(
            "SELECT key,alias,expires_at FROM kasu.repository_names WHERE name=$1",
            &[&normalized],
        )
        .map_err(pg_error)?
        .map(|row| {
            Ok(RepositoryName {
                name: normalized.to_owned(),
                key: RepoKey::from_bytes(row.get(0))?,
                alias: row.get(1),
                expires_at: row.get(2),
            })
        })
        .transpose()
}
fn store(tx: &mut Transaction<'_>, record: &RepositoryRecord) -> Result<()> {
    tx.execute(
        "INSERT INTO kasu.repository_records(key,record,state,version) VALUES ($1,$2,$3,$4) ON CONFLICT(key) DO UPDATE SET record=excluded.record,state=excluded.state,version=excluded.version",
        &[
            &record.key.as_bytes(),
            &record.encode(),
            &i16::from(record.state.code()),
            &number(record.version)?,
        ],
    )
    .map_err(pg_error)?;
    Ok(())
}
fn set_name(tx: &mut Transaction<'_>, value: &RepositoryName) -> Result<()> {
    tx.execute(
        "INSERT INTO kasu.repository_names(name,key,alias,expires_at) VALUES ($1,$2,$3,$4) ON CONFLICT(name) DO UPDATE SET key=excluded.key,alias=excluded.alias,expires_at=excluded.expires_at",
        &[&value.name, &value.key.as_bytes(), &value.alias, &value.expires_at],
    )
    .map_err(pg_error)?;
    Ok(())
}
// The counter row orders the feed by commit, like the ref log counters.
fn append(
    tx: &mut Transaction<'_>,
    key: &RepoKey,
    event: &str,
    actor: &[u8],
    time: i64,
) -> Result<()> {
    let seq: i64 = tx
        .query_one(
            "UPDATE kasu.repository_log_counter SET value=value+1 WHERE singleton RETURNING value",
            &[],
        )
        .map_err(pg_error)?
        .get(0);
    tx.execute(
        "INSERT INTO kasu.repository_log(seq,key,event,actor,time) VALUES ($1,$2,$3,$4,$5)",
        &[&seq, &key.as_bytes(), &event, &actor, &time],
    )
    .map_err(pg_error)?;
    Ok(())
}
/// Shared row lock: a concurrent state change waits for in-flight ref
/// transactions, and later ref transactions observe the new state.
pub(super) fn require_writable(tx: &mut Transaction<'_>, repo: &[u8]) -> Result<()> {
    let state = tx
        .query_opt(
            "SELECT state FROM kasu.repository_records WHERE key=$1 FOR SHARE",
            &[&repo],
        )
        .map_err(pg_error)?
        .map(|row| row.get::<_, i16>(0));
    match state {
        Some(code) => RepositoryState::from_code(
            u8::try_from(code).map_err(|_| invalid("invalid repository state"))?,
        )?
        .require_writable(),
        None => Ok(()),
    }
}

impl RepositoryStore for PostgresStore {
    fn create_repository(&self, value: &RepositoryRecord) -> Result<RepositoryWrite> {
        value.validate()?;
        self.client(|client| {
            let mut tx = client.transaction().map_err(pg_error)?;
            let normalized = value.name.normalized();
            tx.query_one("SELECT pg_advisory_xact_lock(1801548660)", &[])
                .map_err(pg_error)?;
            if record(&mut tx, value.key.as_bytes(), "")?.is_some()
                || name(&mut tx, &normalized)?.is_some_and(|n| n.live(value.created_at))
            {
                return Ok(RepositoryWrite::NameTaken);
            }
            let mut stored = value.clone();
            stored.version = 1;
            store(&mut tx, &stored)?;
            set_name(
                &mut tx,
                &RepositoryName {
                    name: normalized,
                    key: value.key.clone(),
                    alias: false,
                    expires_at: None,
                },
            )?;
            append(
                &mut tx,
                &value.key,
                "created",
                &value.created_by,
                value.created_at,
            )?;
            tx.commit().map_err(pg_error)?;
            Ok(RepositoryWrite::Applied(stored))
        })
    }
    fn repository(&self, key: &RepoKey) -> Result<Option<RepositoryRecord>> {
        self.client(|client| record(client, key.as_bytes(), ""))
    }
    fn resolve_repository_name(
        &self,
        value: &RepoName,
        now: i64,
    ) -> Result<Option<RepositoryName>> {
        self.client(|client| Ok(name(client, &value.normalized())?.filter(|n| n.live(now))))
    }
    fn list_repository_names(
        &self,
        prefix: &str,
        after: &str,
        limit: usize,
    ) -> Result<Vec<RepositoryName>> {
        self.client(|client| {
            client
                .query(
                    "SELECT name,key FROM kasu.repository_names WHERE NOT alias AND name COLLATE \"C\">$1 AND left(name,$2)=$3 ORDER BY name COLLATE \"C\" LIMIT $4",
                    &[
                        &after,
                        &(prefix.len() as i32),
                        &prefix,
                        &(limit.min(i64::MAX as usize) as i64),
                    ],
                )
                .map_err(pg_error)?
                .into_iter()
                .map(|row| {
                    Ok(RepositoryName {
                        name: row.get(0),
                        key: RepoKey::from_bytes(row.get(1))?,
                        alias: false,
                        expires_at: None,
                    })
                })
                .collect()
        })
    }
    fn list_repositories(
        &self,
        after: Option<&RepoKey>,
        limit: usize,
    ) -> Result<Vec<RepositoryRecord>> {
        self.client(|client| {
            client
                .query(
                    "SELECT record,version FROM kasu.repository_records WHERE key>$1 ORDER BY key LIMIT $2",
                    &[
                        &after.map(RepoKey::as_bytes).unwrap_or_default(),
                        &(limit.min(i64::MAX as usize) as i64),
                    ],
                )
                .map_err(pg_error)?
                .into_iter()
                .map(|row| decode(row.get(0), row.get(1)))
                .collect()
        })
    }
    fn update_repository(
        &self,
        value: &RepositoryRecord,
        event: &str,
        actor: &[u8],
        time: i64,
    ) -> Result<RepositoryWrite> {
        self.client(|client| {
            let mut tx = client.transaction().map_err(pg_error)?;
            let Some(current) = record(&mut tx, value.key.as_bytes(), "FOR UPDATE")? else {
                return Ok(RepositoryWrite::Mismatch(None));
            };
            if current.version != value.version {
                return Ok(RepositoryWrite::Mismatch(Some(current)));
            }
            check_update(&current, value)?;
            let mut stored = value.clone();
            stored.version = next_version(current.version)?;
            store(&mut tx, &stored)?;
            if stored.state == RepositoryState::Deleting {
                tx.execute(
                    "DELETE FROM kasu.repository_names WHERE key=$1",
                    &[&value.key.as_bytes()],
                )
                .map_err(pg_error)?;
            }
            append(&mut tx, &value.key, event, actor, time)?;
            tx.commit().map_err(pg_error)?;
            Ok(RepositoryWrite::Applied(stored))
        })
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
        self.client(|client| {
            let mut tx = client.transaction().map_err(pg_error)?;
            tx.query_one("SELECT pg_advisory_xact_lock(1801548660)", &[])
                .map_err(pg_error)?;
            let Some(current) = record(&mut tx, key.as_bytes(), "FOR UPDATE")? else {
                return Ok(RepositoryWrite::Mismatch(None));
            };
            if current.version != expected_version || current.state == RepositoryState::Deleting {
                return Ok(RepositoryWrite::Mismatch(Some(current)));
            }
            let normalized = value.normalized();
            let old = current.name.normalized();
            if normalized != old
                && name(&mut tx, &normalized)?.is_some_and(|n| n.live(time) && n.key != *key)
            {
                return Ok(RepositoryWrite::NameTaken);
            }
            tx.execute("DELETE FROM kasu.repository_names WHERE name=$1", &[&old])
                .map_err(pg_error)?;
            if normalized != old
                && let Some(expires_at) = alias_expires_at
            {
                set_name(
                    &mut tx,
                    &RepositoryName {
                        name: old,
                        key: key.clone(),
                        alias: true,
                        expires_at: (expires_at != i64::MAX).then_some(expires_at),
                    },
                )?;
            }
            set_name(
                &mut tx,
                &RepositoryName {
                    name: normalized,
                    key: key.clone(),
                    alias: false,
                    expires_at: None,
                },
            )?;
            let mut stored = current;
            stored.name = value.clone();
            stored.version = next_version(stored.version)?;
            store(&mut tx, &stored)?;
            append(&mut tx, key, "renamed", actor, time)?;
            tx.commit().map_err(pg_error)?;
            Ok(RepositoryWrite::Applied(stored))
        })
    }
    fn tail_repositories(&self, after: u64, limit: usize) -> Result<Vec<RepositoryEvent>> {
        self.client(|client| {
            client
                .query(
                    "SELECT seq,key,event,actor,time FROM kasu.repository_log WHERE seq>$1 ORDER BY seq LIMIT $2",
                    &[
                        &(after.min(i64::MAX as u64) as i64),
                        &(limit.min(i64::MAX as usize) as i64),
                    ],
                )
                .map_err(pg_error)?
                .into_iter()
                .map(|row| {
                    Ok(RepositoryEvent {
                        seq: unsigned(row.get(0))?,
                        key: RepoKey::from_bytes(row.get(1))?,
                        event: row.get(2),
                        actor: row.get(3),
                        time: row.get(4),
                    })
                })
                .collect()
        })
    }
    fn purge_repository_refs(&self, key: &RepoKey, ref_logs: bool) -> Result<()> {
        self.client(|client| {
            let mut tx = client.transaction().map_err(pg_error)?;
            require_deleting(record(&mut tx, key.as_bytes(), "FOR UPDATE")?.as_ref())?;
            let repo = hash(key.as_bytes());
            let mut tables = vec!["refs", "land_results", "land_claims", "webhook_state"];
            if ref_logs {
                tables.extend(["ref_log", "log_counters"]);
            }
            for table in tables {
                tx.execute(&format!("DELETE FROM kasu.{table} WHERE repo=$1"), &[&repo])
                    .map_err(pg_error)?;
            }
            tx.commit().map_err(pg_error)
        })
    }
}
