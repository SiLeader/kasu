use crate::*;
use ::postgres::{Client, Config, GenericClient, Row, Transaction, config::SslMode};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};
mod registry;
mod review;
mod webhook;

/// Connection settings are intentionally not Debug: the DSN can contain a password.
#[derive(Clone)]
pub struct PostgresOptions {
    pub dsn: String,
    pub ca_file: Option<PathBuf>,
    pub allow_plaintext: bool,
    pub statement_timeout: Duration,
    pub lock_timeout: Duration,
}
impl PostgresOptions {
    pub fn new(dsn: String) -> Self {
        Self {
            dsn,
            ca_file: None,
            allow_plaintext: false,
            statement_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(5),
        }
    }
}

pub struct PostgresStore {
    connection: Mutex<Option<Client>>,
    options: PostgresOptions,
}
impl PostgresStore {
    pub fn connect(options: PostgresOptions) -> Result<Self> {
        let client = crate::blocking::run(|| connect_client(options.clone()))?;
        Ok(Self {
            connection: Mutex::new(Some(client)),
            options,
        })
    }
    fn client<T: Send>(&self, work: impl FnOnce(&mut Client) -> Result<T> + Send) -> Result<T> {
        crate::blocking::run(|| {
            let mut client = self
                .connection
                .lock()
                .map_err(|_| storage_error("PostgreSQL mutex poisoned"))?;
            if client.as_ref().is_none_or(Client::is_closed) {
                *client = Some(connect_client(self.options.clone())?);
            }
            work(
                client
                    .as_mut()
                    .ok_or_else(|| storage_error("PostgreSQL connection is closed"))?,
            )
        })
    }
}
impl Drop for PostgresStore {
    fn drop(&mut self) {
        if let Ok(connection) = self.connection.get_mut()
            && let Some(client) = connection.take()
        {
            let _ = crate::blocking::run(move || {
                drop(client);
                Ok(())
            });
        }
    }
}
fn connect_client(options: PostgresOptions) -> Result<Client> {
    if options.dsn.trim().is_empty() {
        return Err(invalid("empty PostgreSQL connection configuration"));
    }
    let mut config: Config = options
        .dsn
        .parse()
        .map_err(|_| invalid("invalid PostgreSQL connection configuration"))?;
    if config.get_ssl_mode() == SslMode::Disable {
        if !options.allow_plaintext {
            return Err(invalid("plaintext PostgreSQL requires explicit permission"));
        }
    } else {
        config.ssl_mode(SslMode::Require);
    }
    config.connect_timeout(Duration::from_secs(5));
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = options.ca_file {
        let file = std::fs::File::open(path).map_err(storage_error)?;
        let mut reader = std::io::BufReader::new(file);
        let mut count = 0;
        for cert in CertificateDer::pem_reader_iter(&mut reader) {
            roots
                .add(cert.map_err(storage_error)?)
                .map_err(storage_error)?;
            count += 1;
        }
        if count == 0 {
            return Err(invalid("PostgreSQL CA file contains no certificates"));
        }
    }
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(storage_error)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let mut client = config
        .connect(tokio_postgres_rustls::MakeRustlsConnect::new(tls))
        .map_err(pg_error)?;
    for (name, timeout) in [
        ("statement_timeout", options.statement_timeout),
        ("lock_timeout", options.lock_timeout),
    ] {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(invalid("PostgreSQL timeouts must be within 1 ms..300 s"));
        }
        let value = format!("{}ms", timeout.as_millis().max(1));
        client
            .query_one("SELECT set_config($1,$2,false)", &[&name, &value])
            .map_err(pg_error)?;
    }
    client
        .batch_execute("SET idle_in_transaction_session_timeout='30s'; SET synchronous_commit=on;")
        .map_err(pg_error)?;
    let mut tx = client.transaction().map_err(pg_error)?;
    tx.query_one("SELECT pg_advisory_xact_lock(1801548659)", &[])
        .map_err(pg_error)?;
    tx.batch_execute(include_str!("postgres/schema.sql"))
        .map_err(pg_error)?;
    tx.batch_execute(include_str!("postgres/review.sql"))
        .map_err(pg_error)?;
    if tx
        .query_one(
            "SELECT version FROM kasu.schema_version WHERE singleton",
            &[],
        )
        .map_err(pg_error)?
        .get::<_, i64>(0)
        != 1
    {
        return Err(invalid("unsupported PostgreSQL storage schema version"));
    }
    tx.commit().map_err(pg_error)?;
    Ok(client)
}
// Do not surface SQL parameter values, connection strings or server DETAIL fields.
fn pg_error(error: ::postgres::Error) -> Error {
    match error.code() {
        Some(code) => storage_error(format!("PostgreSQL error {}", code.code())),
        None => storage_error("PostgreSQL connection or protocol error"),
    }
}
fn hash(raw: &[u8]) -> Vec<u8> {
    ObjectId::hash(raw).0.to_vec()
}
fn number(value: u64) -> Result<i64> {
    value
        .try_into()
        .map_err(|_| invalid("value exceeds PostgreSQL bigint range"))
}
fn unsigned(value: i64) -> Result<u64> {
    value
        .try_into()
        .map_err(|_| invalid("negative stored counter"))
}
fn next_version(value: u64) -> Result<u64> {
    value
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or_else(|| invalid("ref version exhausted"))
}
fn native_value(value: Option<Vec<u8>>) -> Result<Option<ChangesetId>> {
    value
        .map(|raw| {
            raw.try_into()
                .map(ChangesetId)
                .map_err(|_| invalid("corrupt native ref target"))
        })
        .transpose()
}
fn git_value(value: Option<Vec<u8>>) -> Result<Option<kasu_core::GitRef>> {
    value.map(|raw| kasu_core::GitRef::decode(&raw)).transpose()
}
fn scope(client: &mut impl GenericClient, repo: &[u8]) -> Result<Vec<u8>> {
    let id = hash(repo);
    client
        .execute(
            "INSERT INTO kasu.repositories(id,name) VALUES ($1,$2) ON CONFLICT DO NOTHING",
            &[&id, &repo],
        )
        .map_err(pg_error)?;
    let row = client
        .query_one("SELECT name FROM kasu.repositories WHERE id=$1", &[&id])
        .map_err(pg_error)?;
    if row.get::<_, Vec<u8>>(0) != repo {
        return Err(Error::Collision("repository key".into()));
    }
    Ok(id)
}
fn ref_row(
    client: &mut impl GenericClient,
    repo: &[u8],
    name: &[u8],
    kind: i16,
) -> Result<Option<Row>> {
    let row = client
        .query_opt(
            "SELECT name,value,version FROM kasu.refs WHERE repo=$1 AND kind=$2 AND id=$3",
            &[&repo, &kind, &hash(name)],
        )
        .map_err(pg_error)?;
    if row
        .as_ref()
        .is_some_and(|row| row.get::<_, Vec<u8>>(0) != name)
    {
        return Err(Error::Collision("ref key".into()));
    }
    Ok(row)
}
fn native_state(client: &mut impl GenericClient, repo: &[u8], name: &[u8]) -> Result<RefState> {
    ref_row(client, repo, name, 0)?
        .map(|row| {
            Ok(RefState {
                target: native_value(row.get(1))?,
                version: unsigned(row.get(2))?,
            })
        })
        .unwrap_or(Ok(RefState::default()))
}
fn git_state(client: &mut impl GenericClient, repo: &[u8], name: &[u8]) -> Result<GitRefState> {
    ref_row(client, repo, name, 1)?
        .map(|row| {
            Ok(GitRefState {
                value: git_value(row.get(1))?,
                version: unsigned(row.get(2))?,
            })
        })
        .unwrap_or(Ok(GitRefState::default()))
}
fn check_ref_namespace(client: &mut impl GenericClient, repo: &[u8], name: &[u8]) -> Result<()> {
    for ancestor in ref_ancestors(name) {
        for kind in [0, 1] {
            if ref_row(client, repo, ancestor, kind)?
                .is_some_and(|row| row.get::<_, Option<Vec<u8>>>(1).is_some())
            {
                return Err(invalid("ref name conflicts with an existing ancestor"));
            }
        }
    }
    let prefix = [name, b"/"].concat();
    let indexed = &prefix[..prefix.len().min(registry::KEY_INDEX_BYTES)];
    let exists: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM kasu.refs WHERE repo=$1 AND value IS NOT NULL AND substring(name from 1 for 96)>=$2 AND ($3::BYTEA IS NULL OR substring(name from 1 for 96)<$3) AND name>=$4 AND name<$5)",
            &[&repo, &indexed, &prefix_end(indexed), &prefix, &prefix_end(&prefix)],
        )
        .map_err(pg_error)?
        .get(0);
    if exists {
        return Err(invalid("ref name conflicts with an existing descendant"));
    }
    Ok(())
}
fn lock_refs(
    tx: &mut Transaction<'_>,
    repo: &[u8],
    refs: impl IntoIterator<Item = (i16, Vec<u8>)>,
) -> Result<()> {
    // Shared ancestor locks allow sibling writes, but serialize a ref with
    // creation/deletion of its descendants, including across both ref models.
    let mut sorted = BTreeMap::new();
    for (kind, name) in refs {
        for ancestor in ref_ancestors(&name) {
            sorted.entry((0, ancestor.to_vec())).or_insert(false);
        }
        sorted.insert((0, name.clone()), true);
        sorted.insert((kind, name), true);
    }
    for ((kind, name), exclusive) in sorted {
        let id = hash(&name);
        tx.execute("INSERT INTO kasu.refs(repo,kind,id,name,value,version) VALUES ($1,$2,$3,$4,NULL,0) ON CONFLICT DO NOTHING", &[&repo, &kind, &id, &name]).map_err(pg_error)?;
        let row = tx
            .query_one(
                if exclusive {
                    "SELECT name FROM kasu.refs WHERE repo=$1 AND kind=$2 AND id=$3 FOR UPDATE"
                } else {
                    "SELECT name FROM kasu.refs WHERE repo=$1 AND kind=$2 AND id=$3 FOR SHARE"
                },
                &[&repo, &kind, &id],
            )
            .map_err(pg_error)?;
        if row.get::<_, Vec<u8>>(0) != name {
            return Err(Error::Collision("ref key".into()));
        }
    }
    Ok(())
}
fn append_log(
    tx: &mut Transaction<'_>,
    repo: &[u8],
    stream: i16,
    name: &[u8],
    old: Option<Vec<u8>>,
    new: Option<Vec<u8>>,
    audit: (&[u8], i64),
) -> Result<()> {
    let (actor, time) = audit;
    let (actor, via) = actor_parts(actor);
    tx.execute(
        "INSERT INTO kasu.log_counters(repo,stream,value) VALUES ($1,$2,0) ON CONFLICT DO NOTHING",
        &[&repo, &stream],
    )
    .map_err(pg_error)?;
    let sequence: i64 = tx.query_one("UPDATE kasu.log_counters SET value=value+1 WHERE repo=$1 AND stream=$2 RETURNING value", &[&repo,&stream]).map_err(pg_error)?.get(0);
    tx.execute("INSERT INTO kasu.ref_log(repo,stream,seq,name,old,new,actor,via,time) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)", &[&repo,&stream,&sequence,&name,&old,&new,&actor,&via,&time]).map_err(pg_error)?;
    Ok(())
}
fn write_native(
    tx: &mut Transaction<'_>,
    repo: &[u8],
    update: &RefUpdate,
    version: u64,
    actor: &[u8],
    time: i64,
) -> Result<()> {
    let new = update.new.map(|id| id.0.to_vec());
    tx.execute(
        "UPDATE kasu.refs SET value=$4,version=$5 WHERE repo=$1 AND kind=0 AND id=$2 AND name=$3",
        &[
            &repo,
            &hash(&update.name),
            &update.name,
            &new,
            &number(version)?,
        ],
    )
    .map_err(pg_error)?;
    append_log(
        tx,
        repo,
        0,
        &update.name,
        update.expected.target.map(|id| id.0.to_vec()),
        new,
        (actor, time),
    )
}
fn write_git(
    tx: &mut Transaction<'_>,
    repo: &[u8],
    update: &GitRefUpdate,
    version: u64,
    actor: &[u8],
    time: i64,
) -> Result<()> {
    let new = update.new.as_ref().map(Canonical::encode);
    tx.execute(
        "UPDATE kasu.refs SET value=$4,version=$5 WHERE repo=$1 AND kind=1 AND id=$2 AND name=$3",
        &[
            &repo,
            &hash(&update.name),
            &update.name,
            &new,
            &number(version)?,
        ],
    )
    .map_err(pg_error)?;
    append_log(
        tx,
        repo,
        1,
        &update.name,
        update.expected.value.as_ref().map(Canonical::encode),
        new,
        (actor, time),
    )
}
fn read_land(
    client: &mut impl GenericClient,
    repo: &[u8],
    key: &[u8],
) -> Result<Option<LandRecord>> {
    client
        .query_opt(
            "SELECT fingerprint,response FROM kasu.land_results WHERE repo=$1 AND key=$2",
            &[&repo, &key],
        )
        .map_err(pg_error)?
        .map(|row| {
            Ok(LandRecord {
                fingerprint: ObjectId(
                    row.get::<_, Vec<u8>>(0)
                        .try_into()
                        .map_err(|_| invalid("corrupt Land fingerprint"))?,
                ),
                response: row.get(1),
            })
        })
        .transpose()
}
impl MetaStore for PostgresStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.client(|client| {
            client
                .query_opt(
                    "SELECT key,value FROM kasu.immutable WHERE id=$1",
                    &[&hash(key)],
                )
                .map_err(pg_error)?
                .map(|row| {
                    if row.get::<_, Vec<u8>>(0) != key {
                        return Err(Error::Collision("immutable key".into()));
                    }
                    Ok(row.get(1))
                })
                .transpose()
        })
    }
    fn put_if_absent(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.client(|client| {
            let id = hash(key);
            client.execute("INSERT INTO kasu.immutable(id,key,value) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING", &[&id,&key,&value]).map_err(pg_error)?;
            let row = client.query_one("SELECT key,value FROM kasu.immutable WHERE id=$1", &[&id]).map_err(pg_error)?;
            if row.get::<_, Vec<u8>>(0) != key || row.get::<_,Vec<u8>>(1) != value { return Err(Error::Collision("immutable key/value".into())); }
            Ok(())
        })
    }
    fn scan_prefix(&self, prefix: &[u8], limit: usize) -> Result<Vec<Vec<u8>>> {
        let indexed = &prefix[..prefix.len().min(registry::KEY_INDEX_BYTES)];
        let end = prefix_end(indexed);
        self.client(|client| {
            Ok(client
                .query(
                    "SELECT key FROM kasu.immutable WHERE substring(key from 1 for 96)>=$1 AND ($2::BYTEA IS NULL OR substring(key from 1 for 96)<$2) AND substring(key from 1 for $3)=$4 ORDER BY key LIMIT $5",
                    &[&indexed, &end, &(prefix.len() as i32), &prefix, &(limit.min(i64::MAX as usize) as i64)],
                )
                .map_err(pg_error)?
                .into_iter()
                .map(|row| row.get(0))
                .collect())
        })
    }
    fn delete_prefix(&self, prefix: &[u8], limit: usize) -> Result<usize> {
        if prefix.is_empty() {
            return Err(invalid("refusing to delete every immutable key"));
        }
        let indexed = &prefix[..prefix.len().min(registry::KEY_INDEX_BYTES)];
        let end = prefix_end(indexed);
        self.client(|client| {
            let deleted = client
                .execute(
                    "DELETE FROM kasu.immutable WHERE id IN (SELECT id FROM kasu.immutable WHERE substring(key from 1 for 96)>=$1 AND ($2::BYTEA IS NULL OR substring(key from 1 for 96)<$2) AND substring(key from 1 for $3)=$4 LIMIT $5)",
                    &[&indexed, &end, &(prefix.len() as i32), &prefix, &(limit.min(i64::MAX as usize) as i64)],
                )
                .map_err(pg_error)?;
            usize::try_from(deleted).map_err(|_| invalid("deleted row count exceeds range"))
        })
    }
}
impl RefStore for PostgresStore {
    fn capabilities(&self) -> RefCapabilities {
        RefCapabilities { atomic_batch: true }
    }
    fn resolve(&self, repo: &[u8], name: &[u8]) -> Result<RefState> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            native_state(client, &repo, name)
        })
    }
    fn list(&self, repo: &[u8], prefix: &[u8]) -> Result<Vec<(Vec<u8>, RefState)>> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            client.query("SELECT name,value,version FROM kasu.refs WHERE repo=$1 AND kind=0 AND value IS NOT NULL ORDER BY name", &[&repo]).map_err(pg_error)?.into_iter().filter(|row| row.get::<_,Vec<u8>>(0).starts_with(prefix)).map(|row| Ok((row.get(0),RefState { target: native_value(row.get(1))?,version:unsigned(row.get(2))? }))).collect()
        })
    }
    fn compare_and_swap(
        &self,
        repo: &[u8],
        updates: &[RefUpdate],
        actor: &[u8],
        time: i64,
    ) -> Result<CasResult> {
        match self.compare_native_and_git(repo, updates, &[], actor, time)? {
            RefPublicationResult::Applied { native, .. } => Ok(CasResult::Applied(native)),
            RefPublicationResult::NativeMismatch { name, actual } => {
                Ok(CasResult::Mismatch { name, actual })
            }
            _ => Err(invalid("unexpected Git mismatch in native CAS")),
        }
    }
    fn tail(&self, repo: &[u8], after: u64, limit: usize) -> Result<Vec<RefLog>> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            client.query("SELECT seq,name,old,new,actor,time,COALESCE(via,''::bytea) FROM kasu.ref_log WHERE repo=$1 AND stream=0 AND seq>$2 ORDER BY seq LIMIT $3", &[&repo,&(after.min(i64::MAX as u64) as i64),&(limit.min(i64::MAX as usize) as i64)]).map_err(pg_error)?.into_iter().map(|row| Ok(RefLog { seq:unsigned(row.get(0))?, name:row.get(1),old:native_value(row.get(2))?,new:native_value(row.get(3))?,actor:row.get(4),via:row.get(6),time:row.get(5) })).collect()
        })
    }
    fn land_record(&self, repo: &[u8], key: &[u8]) -> Result<Option<LandRecord>> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            read_land(client, &repo, key)
        })
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
        self.client(|client| {
            let mut tx = client.transaction().map_err(pg_error)?;
            registry::require_writable(&mut tx, repo)?;
            let repo = scope(&mut tx, repo)?;
            tx.execute(
                "INSERT INTO kasu.land_claims(repo,key) VALUES ($1,$2) ON CONFLICT DO NOTHING",
                &[&repo, &key],
            )
            .map_err(pg_error)?;
            tx.query_one(
                "SELECT key FROM kasu.land_claims WHERE repo=$1 AND key=$2 FOR UPDATE",
                &[&repo, &key],
            )
            .map_err(pg_error)?;
            if let Some(record) = read_land(&mut tx, &repo, key)? {
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
            lock_refs(&mut tx, &repo, [(0, commit.name.clone())])?;
            let actual = native_state(&mut tx, &repo, &commit.name)?;
            if actual != commit.expected {
                return Ok(LandCommitResult::Mismatch(actual));
            }
            if let Some(new) = commit.new {
                write_native(
                    &mut tx,
                    &repo,
                    &RefUpdate {
                        name: commit.name.clone(),
                        expected: actual,
                        new: Some(new),
                    },
                    next_version(actual.version)?,
                    actor,
                    time,
                )?;
                check_ref_namespace(&mut tx, &repo, &commit.name)?;
            }
            tx.execute(
                "INSERT INTO kasu.land_results(repo,key,fingerprint,response) VALUES ($1,$2,$3,$4)",
                &[
                    &repo,
                    &key,
                    &commit.record.fingerprint.0.as_slice(),
                    &commit.record.response,
                ],
            )
            .map_err(pg_error)?;
            if cancelled() {
                return Ok(LandCommitResult::Cancelled);
            }
            tx.commit().map_err(pg_error)?;
            Ok(LandCommitResult::Recorded {
                record: commit.record.clone(),
                replayed: false,
            })
        })
    }
}
impl GitRefStore for PostgresStore {
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
        self.client(|client| {
            let mut tx = client.transaction().map_err(pg_error)?;
            registry::require_writable(&mut tx, repo)?;
            let repo = scope(&mut tx, repo)?;
            lock_refs(
                &mut tx,
                &repo,
                native
                    .iter()
                    .map(|u| (0, u.name.clone()))
                    .chain(git.iter().map(|u| (1, u.name.clone()))),
            )?;
            let mut native_states = Vec::new();
            let mut git_states = Vec::new();
            for update in native {
                let actual = native_state(&mut tx, &repo, &update.name)?;
                if actual != update.expected {
                    return Ok(RefPublicationResult::NativeMismatch {
                        name: update.name.clone(),
                        actual,
                    });
                }
                native_states.push(RefState {
                    target: update.new,
                    version: next_version(actual.version)?,
                });
            }
            for update in git {
                let actual = git_state(&mut tx, &repo, &update.name)?;
                if actual != update.expected {
                    return Ok(RefPublicationResult::GitMismatch {
                        name: update.name.clone(),
                        actual,
                    });
                }
                git_states.push(GitRefState {
                    value: update.new.clone(),
                    version: next_version(actual.version)?,
                });
            }
            for (update, state) in native.iter().zip(&native_states) {
                write_native(&mut tx, &repo, update, state.version, actor, time)?;
            }
            for (update, state) in git.iter().zip(&git_states) {
                write_git(&mut tx, &repo, update, state.version, actor, time)?;
            }
            for name in native
                .iter()
                .filter(|u| u.new.is_some())
                .map(|u| &u.name)
                .chain(git.iter().filter(|u| u.new.is_some()).map(|u| &u.name))
            {
                check_ref_namespace(&mut tx, &repo, name)?;
            }
            tx.commit().map_err(pg_error)?;
            Ok(RefPublicationResult::Applied {
                native: native_states,
                git: git_states,
            })
        })
    }
    fn resolve_git(&self, repo: &[u8], name: &[u8]) -> Result<GitRefState> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            git_state(client, &repo, name)
        })
    }
    fn list_git(&self, repo: &[u8], prefix: &[u8]) -> Result<Vec<(Vec<u8>, GitRefState)>> {
        self.client(|client| {
            let repo=scope(client,repo)?;
            client.query("SELECT name,value,version FROM kasu.refs WHERE repo=$1 AND kind=1 AND value IS NOT NULL ORDER BY name", &[&repo]).map_err(pg_error)?.into_iter().filter(|row| row.get::<_,Vec<u8>>(0).starts_with(prefix)).map(|row| Ok((row.get(0),GitRefState { value:git_value(row.get(1))?,version:unsigned(row.get(2))? }))).collect()
        })
    }
    fn compare_git(
        &self,
        repo: &[u8],
        updates: &[GitRefUpdate],
        actor: &[u8],
        time: i64,
    ) -> Result<GitCasResult> {
        match self.compare_native_and_git(repo, &[], updates, actor, time)? {
            RefPublicationResult::Applied { git, .. } => Ok(GitCasResult::Applied(git)),
            RefPublicationResult::GitMismatch { name, actual } => {
                Ok(GitCasResult::Mismatch { name, actual })
            }
            _ => Err(invalid("unexpected native mismatch in Git CAS")),
        }
    }
    fn tail_git(&self, repo: &[u8], after: u64, limit: usize) -> Result<Vec<GitRefLog>> {
        self.client(|client| {
            let repo=scope(client,repo)?;
            client.query("SELECT seq,name,old,new,actor,time,COALESCE(via,''::bytea) FROM kasu.ref_log WHERE repo=$1 AND stream=1 AND seq>$2 ORDER BY seq LIMIT $3", &[&repo,&(after.min(i64::MAX as u64) as i64),&(limit.min(i64::MAX as usize) as i64)]).map_err(pg_error)?.into_iter().map(|row| Ok(GitRefLog { seq:unsigned(row.get(0))?,name:row.get(1),old:git_value(row.get(2))?,new:git_value(row.get(3))?,actor:row.get(4),via:row.get(6),time:row.get(5) })).collect()
        })
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::*;
    fn options() -> PostgresOptions {
        let mut o = PostgresOptions::new(std::env::var("KASU_TEST_POSTGRES").unwrap());
        o.allow_plaintext = true;
        o
    }
    #[test]
    #[ignore = "requires KASU_TEST_POSTGRES"]
    fn committed_feed_cannot_overtake_an_uncommitted_lower_sequence() {
        let store = PostgresStore::connect(options()).unwrap();
        let reader = PostgresStore::connect(options()).unwrap();
        let writer = PostgresStore::connect(options()).unwrap();
        let repo = format!(
            "feed-order/{}/{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
        .into_bytes();
        store
            .compare_and_swap(
                &repo,
                &[RefUpdate {
                    name: b"first".to_vec(),
                    expected: RefState::default(),
                    new: Some(ChangesetId([1; 32])),
                }],
                b"test",
                1,
            )
            .unwrap();
        let pid = writer
            .client(|c| {
                Ok(c.query_one("SELECT pg_backend_pid()", &[])
                    .map_err(pg_error)?
                    .get::<_, i32>(0))
            })
            .unwrap();
        let mut raw = Client::connect(&options().dsn, ::postgres::NoTls).unwrap();
        let mut tx = raw.transaction().unwrap();
        let id = hash(&repo);
        let seq:i64=tx.query_one("UPDATE kasu.log_counters SET value=value+1 WHERE repo=$1 AND stream=0 RETURNING value", &[&id]).unwrap().get(0);
        assert_eq!(seq, 2);
        tx.execute(
            "INSERT INTO kasu.ref_log(repo,stream,seq,name,actor,time) VALUES ($1,0,$2,$3,$4,2)",
            &[&id, &seq, &b"held".as_slice(), &b"test".as_slice()],
        )
        .unwrap();
        let worker_repo = repo.clone();
        let worker = std::thread::spawn(move || {
            writer.compare_and_swap(
                &worker_repo,
                &[RefUpdate {
                    name: b"later".to_vec(),
                    expected: RefState::default(),
                    new: Some(ChangesetId([3; 32])),
                }],
                b"test",
                3,
            )
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let waiting=reader.client(|c| Ok(c.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock')", &[&pid]).map_err(pg_error)?.get::<_,bool>(0))).unwrap();
            if waiting {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "writer never waited for the earlier log transaction"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(reader.tail(&repo, 1, 100).unwrap().is_empty());
        tx.commit().unwrap();
        assert!(matches!(
            worker.join().unwrap().unwrap(),
            CasResult::Applied(_)
        ));
        let tail = reader.tail(&repo, 1, 100).unwrap();
        assert_eq!(tail.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(tail[0].name, b"held");
        assert_eq!(tail[1].name, b"later");
    }
}

#[cfg(test)]
mod reconnect_tests {
    use super::*;
    #[test]
    #[ignore = "requires KASU_TEST_POSTGRES"]
    fn a_closed_connection_recovers_on_a_subsequent_call_without_replaying_a_query() {
        let mut options = PostgresOptions::new(std::env::var("KASU_TEST_POSTGRES").unwrap());
        options.allow_plaintext = true;
        let store = PostgresStore::connect(options.clone()).unwrap();
        let key = format!(
            "reconnect/{}/{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
        .into_bytes();
        store.put_if_absent(&key, b"durable").unwrap();
        let pid = store
            .client(|c| {
                Ok(c.query_one("SELECT pg_backend_pid()", &[])
                    .map_err(pg_error)?
                    .get::<_, i32>(0))
            })
            .unwrap();
        let mut other = Client::connect(&options.dsn, ::postgres::NoTls).unwrap();
        assert!(
            other
                .query_one("SELECT pg_terminate_backend($1)", &[&pid])
                .unwrap()
                .get::<_, bool>(0)
        );
        let _ = store.get(&key);
        assert_eq!(store.get(&key).unwrap(), Some(b"durable".to_vec()));
    }
}
