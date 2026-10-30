use super::{
    PostgresStore, lock_refs, native_state, next_version, number, pg_error, registry, scope,
    unsigned, write_native,
};
use crate::*;
use ::postgres::{GenericClient, Row};

const COLUMNS: &str = "number,version,state,target,head,author,data";
fn review_row(row: Row) -> Result<ReviewRow> {
    Ok(ReviewRow {
        number: unsigned(row.get(0))?,
        version: unsigned(row.get(1))?,
        state: u8::try_from(row.get::<_, i16>(2)).map_err(|_| invalid("invalid review state"))?,
        target: row.get(3),
        head: row.get(4),
        author: row.get(5),
        data: row.get(6),
    })
}
fn read_request(
    client: &mut impl GenericClient,
    repo: &[u8],
    account: &str,
    key: &[u8],
) -> Result<Option<ReviewRequestRecord>> {
    Ok(client
        .query_opt(
            "SELECT fingerprint,response FROM kasu.review_submissions WHERE repo=$1 AND account=$2 AND key=$3",
            &[&repo, &account, &key],
        )
        .map_err(pg_error)?
        .map(|row| ReviewRequestRecord {
            account: account.into(),
            key: key.to_vec(),
            fingerprint: row.get(0),
            response: row.get(1),
        }))
}
impl ReviewStore for PostgresStore {
    fn review(&self, repo: &[u8], n: u64) -> Result<Option<ReviewRow>> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            client
                .query_opt(
                    &format!("SELECT {COLUMNS} FROM kasu.reviews WHERE repo=$1 AND number=$2"),
                    &[&repo, &number(n)?],
                )
                .map_err(pg_error)?
                .map(review_row)
                .transpose()
        })
    }
    fn list_reviews(
        &self,
        repo: &[u8],
        filter: &ReviewFilter,
        before: u64,
        limit: usize,
    ) -> Result<Vec<ReviewRow>> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            client
                .query(
                    &format!("SELECT {COLUMNS} FROM kasu.reviews WHERE repo=$1 AND ($2::SMALLINT IS NULL OR state=$2) AND ($3::BYTEA IS NULL OR target=$3) AND ($4::TEXT IS NULL OR author=$4) AND ($5::BIGINT=0 OR number<$5) ORDER BY number DESC LIMIT $6"),
                    &[&repo, &filter.state.map(i16::from), &filter.target, &filter.author, &number(before)?, &number(limit as u64)?],
                )
                .map_err(pg_error)?
                .into_iter()
                .map(review_row)
                .collect()
        })
    }
    fn review_revisions(&self, repo: &[u8], n: u64) -> Result<Vec<ReviewRevisionRow>> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            client
                .query("SELECT revision,data FROM kasu.review_revisions WHERE repo=$1 AND number=$2 ORDER BY revision", &[&repo, &number(n)?])
                .map_err(pg_error)?
                .into_iter()
                .map(|row| {
                    Ok(ReviewRevisionRow {
                        revision: u32::try_from(row.get::<_, i64>(0)).map_err(|_| invalid("invalid revision"))?,
                        data: row.get(1),
                    })
                })
                .collect()
        })
    }
    fn review_votes(&self, repo: &[u8], n: u64) -> Result<Vec<ReviewVoteRow>> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            Ok(client
                .query("SELECT account,data FROM kasu.review_votes WHERE repo=$1 AND number=$2 ORDER BY account", &[&repo, &number(n)?])
                .map_err(pg_error)?
                .into_iter()
                .map(|row| ReviewVoteRow {
                    account: row.get(0),
                    data: row.get(1),
                })
                .collect())
        })
    }
    fn review_comments(&self, repo: &[u8], n: u64) -> Result<Vec<ReviewCommentRow>> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            client
                .query("SELECT id,author,draft,data FROM kasu.review_comments WHERE repo=$1 AND number=$2 ORDER BY id", &[&repo, &number(n)?])
                .map_err(pg_error)?
                .into_iter()
                .map(|row| {
                    Ok(ReviewCommentRow {
                        id: unsigned(row.get(0))?,
                        author: row.get(1),
                        draft: row.get(2),
                        data: row.get(3),
                    })
                })
                .collect()
        })
    }
    fn review_request(
        &self,
        repo: &[u8],
        account: &str,
        key: &[u8],
    ) -> Result<Option<ReviewRequestRecord>> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            read_request(client, &repo, account, key)
        })
    }
    fn write_review(&self, repo: &[u8], write: &ReviewWrite) -> Result<ReviewWriteResult> {
        validate_write(write)?;
        self.client(|client| {
            let mut tx = client.transaction().map_err(pg_error)?;
            registry::require_writable(&mut tx, repo)?;
            let repo = scope(&mut tx, repo)?;
            // Serialize the writers of one repository's reviews.
            tx.execute(
                "INSERT INTO kasu.review_counters(repo,next) VALUES ($1,1) ON CONFLICT DO NOTHING",
                &[&repo],
            )
            .map_err(pg_error)?;
            tx.query_one(
                "SELECT next FROM kasu.review_counters WHERE repo=$1 FOR UPDATE",
                &[&repo],
            )
            .map_err(pg_error)?;
            if let Some(request) = &write.request
                && let Some(existing) = read_request(&mut tx, &repo, &request.account, &request.key)?
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
                let next: i64 = tx
                    .query_one(
                        "UPDATE kasu.review_counters SET next=next+1 WHERE repo=$1 RETURNING next-1",
                        &[&repo],
                    )
                    .map_err(pg_error)?
                    .get(0);
                unsigned(next)?
            } else {
                let current = tx
                    .query_opt(
                        "SELECT version FROM kasu.reviews WHERE repo=$1 AND number=$2",
                        &[&repo, &number(review.number)?],
                    )
                    .map_err(pg_error)?
                    .map(|row| unsigned(row.get(0)))
                    .transpose()?;
                if current != Some(write.expected_version) {
                    return Ok(ReviewWriteResult::VersionMismatch);
                }
                review.number
            };
            if review.state == REVIEW_OPEN
                && let Some(row) = tx
                    .query_opt(
                        "SELECT number FROM kasu.reviews WHERE repo=$1 AND target=$2 AND head=$3 AND state=0 AND number<>$4 LIMIT 1",
                        &[&repo, &review.target, &review.head, &number(n)?],
                    )
                    .map_err(pg_error)?
            {
                return Ok(ReviewWriteResult::Duplicate(unsigned(row.get(0))?));
            }
            if let Some(ReviewRefUpdate { update, actor }) = &write.ref_update {
                lock_refs(&mut tx, &repo, [(0, update.name.clone())])?;
                let actual = native_state(&mut tx, &repo, &update.name)?;
                if actual != update.expected {
                    return Ok(ReviewWriteResult::RefMismatch(actual));
                }
                write_native(
                    &mut tx,
                    &repo,
                    update,
                    next_version(actual.version)?,
                    actor,
                    write.time,
                )?;
            }
            tx.execute(
                "INSERT INTO kasu.reviews(repo,number,version,state,target,head,author,data) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(repo,number) DO UPDATE SET version=excluded.version,state=excluded.state,target=excluded.target,head=excluded.head,author=excluded.author,data=excluded.data",
                &[&repo, &number(n)?, &number(review.version)?, &i16::from(review.state), &review.target, &review.head, &review.author, &review.data],
            )
            .map_err(pg_error)?;
            if let Some(revision) = &write.revision {
                tx.execute(
                    "INSERT INTO kasu.review_revisions(repo,number,revision,data) VALUES ($1,$2,$3,$4)",
                    &[&repo, &number(n)?, &i64::from(revision.revision), &revision.data],
                )
                .map_err(pg_error)?;
            }
            for vote in &write.votes {
                tx.execute("INSERT INTO kasu.review_votes(repo,number,account,data) VALUES ($1,$2,$3,$4) ON CONFLICT(repo,number,account) DO UPDATE SET data=excluded.data",
                    &[&repo, &number(n)?, &vote.account, &vote.data]).map_err(pg_error)?;
            }
            for comment in &write.comments {
                tx.execute("INSERT INTO kasu.review_comments(repo,number,id,author,draft,data) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT(repo,number,id) DO UPDATE SET author=excluded.author,draft=excluded.draft,data=excluded.data",
                    &[&repo, &number(n)?, &number(comment.id)?, &comment.author, &comment.draft, &comment.data]).map_err(pg_error)?;
            }
            if !write.event.is_empty() {
                tx.execute(
                    "INSERT INTO kasu.review_log(repo,number,time,data) VALUES ($1,$2,$3,$4)",
                    &[&repo, &number(n)?, &write.time, &write.event],
                )
                .map_err(pg_error)?;
            }
            if let Some(request) = &write.request {
                let response = if request.response.is_empty() {
                    allocated(n)
                } else {
                    request.response.clone()
                };
                tx.execute(
                    "INSERT INTO kasu.review_submissions(repo,account,key,fingerprint,response) VALUES ($1,$2,$3,$4,$5)",
                    &[&repo, &request.account, &request.key, &request.fingerprint, &response],
                )
                .map_err(pg_error)?;
            }
            tx.commit().map_err(pg_error)?;
            Ok(ReviewWriteResult::Applied { number: n })
        })
    }
    fn review_log(&self, repo: &[u8], after: u64, limit: usize) -> Result<Vec<ReviewEvent>> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            client
                .query("SELECT seq,number,time,data FROM kasu.review_log WHERE repo=$1 AND seq>$2 ORDER BY seq LIMIT $3", &[&repo, &number(after)?, &number(limit as u64)?])
                .map_err(pg_error)?
                .into_iter()
                .map(|row| {
                    Ok(ReviewEvent {
                        seq: unsigned(row.get(0))?,
                        number: unsigned(row.get(1))?,
                        time: row.get(2),
                        data: row.get(3),
                    })
                })
                .collect()
        })
    }
    fn purge_reviews(&self, repo: &[u8], _limit: usize) -> Result<usize> {
        self.client(|client| {
            let mut tx = client.transaction().map_err(pg_error)?;
            let repo = scope(&mut tx, repo)?;
            let mut removed = 0;
            for table in [
                "review_log",
                "review_submissions",
                "review_comments",
                "review_votes",
                "review_revisions",
                "reviews",
                "review_counters",
            ] {
                removed += tx
                    .execute(&format!("DELETE FROM kasu.{table} WHERE repo=$1"), &[&repo])
                    .map_err(pg_error)?;
            }
            tx.commit().map_err(pg_error)?;
            usize::try_from(removed).map_err(|_| invalid("deleted row count exceeds range"))
        })
    }
}
