use super::{SqliteStore, registry, resolve};
use crate::*;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS review_counters (repo BLOB PRIMARY KEY, next INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS reviews (repo BLOB NOT NULL, number INTEGER NOT NULL, version INTEGER NOT NULL, state INTEGER NOT NULL, target BLOB NOT NULL, head BLOB NOT NULL, author TEXT NOT NULL, data BLOB NOT NULL, PRIMARY KEY(repo,number)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS reviews_state ON reviews(repo,state,number);
CREATE INDEX IF NOT EXISTS reviews_target ON reviews(repo,target,state);
CREATE INDEX IF NOT EXISTS reviews_author ON reviews(repo,author,state);
CREATE TABLE IF NOT EXISTS review_revisions (repo BLOB NOT NULL, number INTEGER NOT NULL, revision INTEGER NOT NULL, data BLOB NOT NULL, PRIMARY KEY(repo,number,revision)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS review_votes (repo BLOB NOT NULL, number INTEGER NOT NULL, account TEXT NOT NULL, data BLOB NOT NULL, PRIMARY KEY(repo,number,account)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS review_comments (repo BLOB NOT NULL, number INTEGER NOT NULL, id INTEGER NOT NULL, author TEXT NOT NULL, draft INTEGER NOT NULL, data BLOB NOT NULL, PRIMARY KEY(repo,number,id)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS review_submissions (repo BLOB NOT NULL, account TEXT NOT NULL, key BLOB NOT NULL, fingerprint BLOB NOT NULL, response BLOB NOT NULL, PRIMARY KEY(repo,account,key)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS review_log (seq INTEGER PRIMARY KEY AUTOINCREMENT, repo BLOB NOT NULL, number INTEGER NOT NULL, time INTEGER NOT NULL, data BLOB NOT NULL);
CREATE INDEX IF NOT EXISTS review_log_repo ON review_log(repo,seq);
";
const TABLES: [&str; 7] = [
    "review_log",
    "review_submissions",
    "review_comments",
    "review_votes",
    "review_revisions",
    "reviews",
    "review_counters",
];

fn number(value: u64) -> Result<i64> {
    value
        .try_into()
        .map_err(|_| invalid("review value exceeds storage range"))
}
fn unsigned(value: i64) -> Result<u64> {
    value
        .try_into()
        .map_err(|_| invalid("negative stored review value"))
}
type Row = (i64, i64, i64, Vec<u8>, Vec<u8>, String, Vec<u8>);
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}
fn review_row((n, version, state, target, head, author, data): Row) -> Result<ReviewRow> {
    Ok(ReviewRow {
        number: unsigned(n)?,
        version: unsigned(version)?,
        state: u8::try_from(state).map_err(|_| invalid("invalid review state"))?,
        target,
        head,
        author,
        data,
    })
}
const COLUMNS: &str = "number,version,state,target,head,author,data";
fn read_review(db: &Connection, repo: &[u8], n: u64) -> Result<Option<ReviewRow>> {
    db.query_row(
        &format!("SELECT {COLUMNS} FROM reviews WHERE repo=? AND number=?"),
        params![repo, number(n)?],
        row,
    )
    .optional()
    .map_err(storage_error)?
    .map(review_row)
    .transpose()
}
fn read_request(
    db: &Connection,
    repo: &[u8],
    account: &str,
    key: &[u8],
) -> Result<Option<ReviewRequestRecord>> {
    db.query_row(
        "SELECT fingerprint,response FROM review_submissions WHERE repo=? AND account=? AND key=?",
        params![repo, account, key],
        |r| {
            Ok(ReviewRequestRecord {
                account: account.into(),
                key: key.to_vec(),
                fingerprint: r.get(0)?,
                response: r.get(1)?,
            })
        },
    )
    .optional()
    .map_err(storage_error)
}
impl ReviewStore for SqliteStore {
    fn review(&self, repo: &[u8], n: u64) -> Result<Option<ReviewRow>> {
        read_review(&*self.lock()?, repo, n)
    }
    fn list_reviews(
        &self,
        repo: &[u8],
        filter: &ReviewFilter,
        before: u64,
        limit: usize,
    ) -> Result<Vec<ReviewRow>> {
        let db = self.lock()?;
        let mut stmt = db
            .prepare(&format!(
                "SELECT {COLUMNS} FROM reviews WHERE repo=?1 AND (?2 IS NULL OR state=?2) AND (?3 IS NULL OR target=?3) AND (?4 IS NULL OR author=?4) AND (?5=0 OR number<?5) ORDER BY number DESC LIMIT ?6"
            ))
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(
                params![
                    repo,
                    filter.state.map(i64::from),
                    filter.target,
                    filter.author,
                    number(before)?,
                    number(limit as u64)?
                ],
                row,
            )
            .map_err(storage_error)?;
        rows.map(|r| review_row(r.map_err(storage_error)?))
            .collect()
    }
    fn review_revisions(&self, repo: &[u8], n: u64) -> Result<Vec<ReviewRevisionRow>> {
        let db = self.lock()?;
        let mut stmt = db
            .prepare("SELECT revision,data FROM review_revisions WHERE repo=? AND number=? ORDER BY revision")
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![repo, number(n)?], |r| {
                Ok((r.get::<_, i64>(0)?, r.get(1)?))
            })
            .map_err(storage_error)?;
        rows.map(|r| {
            let (revision, data) = r.map_err(storage_error)?;
            Ok(ReviewRevisionRow {
                revision: u32::try_from(revision).map_err(|_| invalid("invalid revision"))?,
                data,
            })
        })
        .collect()
    }
    fn review_votes(&self, repo: &[u8], n: u64) -> Result<Vec<ReviewVoteRow>> {
        let db = self.lock()?;
        let mut stmt = db
            .prepare(
                "SELECT account,data FROM review_votes WHERE repo=? AND number=? ORDER BY account",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![repo, number(n)?], |r| {
                Ok(ReviewVoteRow {
                    account: r.get(0)?,
                    data: r.get(1)?,
                })
            })
            .map_err(storage_error)?;
        rows.map(|r| r.map_err(storage_error)).collect()
    }
    fn review_comments(&self, repo: &[u8], n: u64) -> Result<Vec<ReviewCommentRow>> {
        let db = self.lock()?;
        let mut stmt = db
            .prepare("SELECT id,author,draft,data FROM review_comments WHERE repo=? AND number=? ORDER BY id")
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![repo, number(n)?], |r| {
                Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map_err(storage_error)?;
        rows.map(|r| {
            let (id, author, draft, data) = r.map_err(storage_error)?;
            Ok(ReviewCommentRow {
                id: unsigned(id)?,
                author,
                draft,
                data,
            })
        })
        .collect()
    }
    fn review_request(
        &self,
        repo: &[u8],
        account: &str,
        key: &[u8],
    ) -> Result<Option<ReviewRequestRecord>> {
        read_request(&*self.lock()?, repo, account, key)
    }
    fn write_review(&self, repo: &[u8], write: &ReviewWrite) -> Result<ReviewWriteResult> {
        validate_write(write)?;
        let mut db = self.lock()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        registry::require_writable(&tx, repo)?;
        if let Some(request) = &write.request
            && let Some(existing) = read_request(&tx, repo, &request.account, &request.key)?
        {
            if existing.fingerprint != request.fingerprint {
                return Err(invalid("request key was used for a different request"));
            }
            return Ok(ReviewWriteResult::Replayed {
                response: existing.response,
            });
        }
        let review = &write.review;
        let n = if review.number == 0 {
            tx.execute(
                "INSERT INTO review_counters(repo,next) VALUES (?,1) ON CONFLICT(repo) DO NOTHING",
                [repo],
            )
            .map_err(storage_error)?;
            let next: i64 = tx
                .query_row(
                    "UPDATE review_counters SET next=next+1 WHERE repo=? RETURNING next-1",
                    [repo],
                    |r| r.get(0),
                )
                .map_err(storage_error)?;
            unsigned(next)?
        } else {
            let current: Option<i64> = tx
                .query_row(
                    "SELECT version FROM reviews WHERE repo=? AND number=?",
                    params![repo, number(review.number)?],
                    |r| r.get(0),
                )
                .optional()
                .map_err(storage_error)?;
            if current.map(unsigned).transpose()? != Some(write.expected_version) {
                return Ok(ReviewWriteResult::VersionMismatch);
            }
            review.number
        };
        if review.state == REVIEW_OPEN {
            let duplicate: Option<i64> = tx
                .query_row(
                    "SELECT number FROM reviews WHERE repo=? AND target=? AND head=? AND state=? AND number<>? LIMIT 1",
                    params![repo, review.target, review.head, i64::from(REVIEW_OPEN), number(n)?],
                    |r| r.get(0),
                )
                .optional()
                .map_err(storage_error)?;
            if let Some(other) = duplicate {
                return Ok(ReviewWriteResult::Duplicate(unsigned(other)?));
            }
        }
        if let Some(ReviewRefUpdate { update, actor }) = &write.ref_update {
            let actual = resolve(&tx, repo, &update.name)?;
            if actual != update.expected {
                return Ok(ReviewWriteResult::RefMismatch(actual));
            }
            let version = actual
                .version
                .checked_add(1)
                .filter(|v| *v <= i64::MAX as u64)
                .ok_or_else(|| invalid("ref version exhausted"))?;
            let new = update.new.map(|id| id.0.to_vec());
            tx.execute("INSERT INTO refs(repo,name,target,version) VALUES (?,?,?,?) ON CONFLICT(repo,name) DO UPDATE SET target=excluded.target,version=excluded.version",
                params![repo, update.name, new, version as i64]).map_err(storage_error)?;
            let (actor, via) = actor_parts(actor);
            tx.execute(
                "INSERT INTO ref_log(repo,name,old,new,actor,via,time) VALUES (?,?,?,?,?,?,?)",
                params![
                    repo,
                    update.name,
                    actual.target.map(|id| id.0.to_vec()),
                    new,
                    actor,
                    via,
                    write.time
                ],
            )
            .map_err(storage_error)?;
        }
        tx.execute(
            "INSERT INTO reviews(repo,number,version,state,target,head,author,data) VALUES (?,?,?,?,?,?,?,?) ON CONFLICT(repo,number) DO UPDATE SET version=excluded.version,state=excluded.state,target=excluded.target,head=excluded.head,author=excluded.author,data=excluded.data",
            params![repo, number(n)?, number(review.version)?, i64::from(review.state), review.target, review.head, review.author, review.data],
        )
        .map_err(storage_error)?;
        if let Some(revision) = &write.revision {
            tx.execute(
                "INSERT INTO review_revisions(repo,number,revision,data) VALUES (?,?,?,?)",
                params![
                    repo,
                    number(n)?,
                    i64::from(revision.revision),
                    revision.data
                ],
            )
            .map_err(storage_error)?;
        }
        for vote in &write.votes {
            tx.execute("INSERT INTO review_votes(repo,number,account,data) VALUES (?,?,?,?) ON CONFLICT(repo,number,account) DO UPDATE SET data=excluded.data",
                params![repo, number(n)?, vote.account, vote.data]).map_err(storage_error)?;
        }
        for comment in &write.comments {
            tx.execute("INSERT INTO review_comments(repo,number,id,author,draft,data) VALUES (?,?,?,?,?,?) ON CONFLICT(repo,number,id) DO UPDATE SET author=excluded.author,draft=excluded.draft,data=excluded.data",
                params![repo, number(n)?, number(comment.id)?, comment.author, comment.draft, comment.data]).map_err(storage_error)?;
        }
        if !write.event.is_empty() {
            tx.execute(
                "INSERT INTO review_log(repo,number,time,data) VALUES (?,?,?,?)",
                params![repo, number(n)?, write.time, write.event],
            )
            .map_err(storage_error)?;
        }
        if let Some(request) = &write.request {
            let response = if request.response.is_empty() {
                allocated(n)
            } else {
                request.response.clone()
            };
            tx.execute(
                "INSERT INTO review_submissions(repo,account,key,fingerprint,response) VALUES (?,?,?,?,?)",
                params![repo, request.account, request.key, request.fingerprint, response],
            )
            .map_err(storage_error)?;
        }
        tx.commit().map_err(storage_error)?;
        Ok(ReviewWriteResult::Applied { number: n })
    }
    fn review_log(&self, repo: &[u8], after: u64, limit: usize) -> Result<Vec<ReviewEvent>> {
        let db = self.lock()?;
        let mut stmt = db
            .prepare("SELECT seq,number,time,data FROM review_log WHERE repo=? AND seq>? ORDER BY seq LIMIT ?")
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![repo, number(after)?, number(limit as u64)?], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get(2)?,
                    r.get(3)?,
                ))
            })
            .map_err(storage_error)?;
        rows.map(|r| {
            let (seq, n, time, data) = r.map_err(storage_error)?;
            Ok(ReviewEvent {
                seq: unsigned(seq)?,
                number: unsigned(n)?,
                time,
                data,
            })
        })
        .collect()
    }
    fn purge_reviews(&self, repo: &[u8], _limit: usize) -> Result<usize> {
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(storage_error)?;
        let mut removed = 0;
        for table in TABLES {
            removed += tx
                .execute(&format!("DELETE FROM {table} WHERE repo=?"), [repo])
                .map_err(storage_error)?;
        }
        tx.commit().map_err(storage_error)?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn header(number: u64, version: u64, head: &[u8]) -> ReviewRow {
        ReviewRow {
            number,
            version,
            state: REVIEW_OPEN,
            target: b"refs/heads/main".to_vec(),
            head: head.to_vec(),
            author: "alice".into(),
            data: b"data".to_vec(),
        }
    }
    fn write(review: ReviewRow, expected_version: u64) -> ReviewWrite {
        ReviewWrite {
            review,
            expected_version,
            revision: None,
            votes: vec![],
            comments: vec![],
            event: b"event".to_vec(),
            request: None,
            ref_update: None,
            time: 1,
        }
    }
    #[test]
    fn reviews_allocate_numbers_compare_versions_and_reject_open_duplicates() {
        let store = SqliteStore::memory().unwrap();
        let first = store
            .write_review(b"r", &write(header(0, 1, b"a"), 0))
            .unwrap();
        assert_eq!(first, ReviewWriteResult::Applied { number: 1 });
        assert_eq!(
            store
                .write_review(b"r", &write(header(0, 1, b"a"), 0))
                .unwrap(),
            ReviewWriteResult::Duplicate(1)
        );
        let second = store
            .write_review(b"r", &write(header(0, 1, b"b"), 0))
            .unwrap();
        assert_eq!(second, ReviewWriteResult::Applied { number: 2 });
        assert_eq!(
            store
                .write_review(b"r", &write(header(1, 3, b"a"), 2))
                .unwrap(),
            ReviewWriteResult::VersionMismatch
        );
        let mut closed = header(2, 2, b"b");
        closed.state = REVIEW_CLOSED;
        store.write_review(b"r", &write(closed, 1)).unwrap();
        // Numbers are never reused, and other repositories count separately.
        assert_eq!(
            store
                .write_review(b"r", &write(header(0, 1, b"b"), 0))
                .unwrap(),
            ReviewWriteResult::Applied { number: 3 }
        );
        assert_eq!(
            store
                .write_review(b"o", &write(header(0, 1, b"a"), 0))
                .unwrap(),
            ReviewWriteResult::Applied { number: 1 }
        );
        let open = store
            .list_reviews(
                b"r",
                &ReviewFilter {
                    state: Some(REVIEW_OPEN),
                    ..Default::default()
                },
                0,
                10,
            )
            .unwrap();
        assert_eq!(open.iter().map(|r| r.number).collect::<Vec<_>>(), [3, 1]);
        assert_eq!(store.review_log(b"r", 0, 10).unwrap().len(), 4);
        assert!(store.purge_reviews(b"r", 1000).unwrap() > 0);
        assert!(store.review(b"r", 1).unwrap().is_none());
        assert!(store.review(b"o", 1).unwrap().is_some());
    }
    #[test]
    fn request_keys_replay_and_ref_updates_share_the_transaction() {
        let store = SqliteStore::memory().unwrap();
        let mut create = write(header(0, 1, b"a"), 0);
        create.request = Some(ReviewRequestRecord {
            account: "alice".into(),
            key: b"k".to_vec(),
            fingerprint: b"f".to_vec(),
            response: vec![],
        });
        store.write_review(b"r", &create).unwrap();
        assert_eq!(
            store.write_review(b"r", &create).unwrap(),
            ReviewWriteResult::Replayed {
                response: 1u64.to_be_bytes().to_vec()
            }
        );
        create.request.as_mut().unwrap().fingerprint = b"other".to_vec();
        assert!(store.write_review(b"r", &create).is_err());
        let target = kasu_core::ChangesetId([7; 32]);
        let mut submit = write(header(1, 2, b"a"), 1);
        submit.review.state = REVIEW_SUBMITTED;
        submit.ref_update = Some(ReviewRefUpdate {
            update: RefUpdate {
                name: b"refs/heads/main".to_vec(),
                expected: RefState {
                    target: None,
                    version: 5,
                },
                new: Some(target),
            },
            actor: delegated_actor(b"alice", b"kasu-web"),
        });
        assert_eq!(
            store.write_review(b"r", &submit).unwrap(),
            ReviewWriteResult::RefMismatch(RefState::default())
        );
        // Nothing of the rejected write was applied.
        assert_eq!(store.review(b"r", 1).unwrap().unwrap().version, 1);
        submit.ref_update.as_mut().unwrap().update.expected = RefState::default();
        store.write_review(b"r", &submit).unwrap();
        assert_eq!(
            store.resolve(b"r", b"refs/heads/main").unwrap().target,
            Some(target)
        );
        let log = store.tail(b"r", 0, 10).unwrap();
        assert_eq!(
            (log[0].actor.as_slice(), log[0].via.as_slice()),
            (b"alice".as_slice(), b"kasu-web".as_slice())
        );
    }
}
