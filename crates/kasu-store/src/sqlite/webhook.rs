use super::SqliteStore;
use crate::*;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
type StateRow = (i64, Option<i64>, Option<Vec<u8>>, i64, i64);

fn number(value: u64) -> Result<i64> {
    value
        .try_into()
        .map_err(|_| invalid("webhook sequence exceeds storage range"))
}
fn state(
    db: &Connection,
    repo: &[u8],
    consumer: &str,
    stream: WebhookStream,
) -> Result<WebhookState> {
    let row:Option<StateRow>=db.query_row("SELECT cursor,sequence,body,attempts,retry_at_ms FROM webhook_state WHERE repo=? AND consumer=? AND stream=?",params![repo,consumer,stream.code()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(storage_error)?;
    let Some((cursor, seq, body, attempts, retry_at_ms)) = row else {
        return Ok(WebhookState::default());
    };
    Ok(WebhookState {
        cursor: cursor
            .try_into()
            .map_err(|_| invalid("negative webhook cursor"))?,
        pending: match (seq, body) {
            (Some(sequence), Some(body)) => Some(WebhookDelivery {
                sequence: sequence
                    .try_into()
                    .map_err(|_| invalid("negative delivery sequence"))?,
                body,
                attempts: attempts
                    .try_into()
                    .map_err(|_| invalid("negative delivery attempt count"))?,
                retry_at_ms,
            }),
            (None, None) => None,
            _ => return Err(invalid("incomplete pending webhook state")),
        },
    })
}
impl WebhookStore for SqliteStore {
    fn webhook_state(
        &self,
        repo: &[u8],
        consumer: &str,
        stream: WebhookStream,
    ) -> Result<WebhookState> {
        state(&*self.lock()?, repo, consumer, stream)
    }
    fn prepare_webhook(
        &self,
        repo: &[u8],
        consumer: &str,
        stream: WebhookStream,
        cursor: u64,
        sequence: u64,
        body: &[u8],
    ) -> Result<WebhookState> {
        if consumer.is_empty() || consumer.len() > 128 || sequence <= cursor {
            return Err(invalid("invalid webhook consumer or sequence"));
        }
        let mut db = self.lock()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let current = state(&tx, repo, consumer, stream)?;
        if current.cursor != cursor || current.pending.is_some() {
            return Ok(current);
        }
        tx.execute("INSERT INTO webhook_state(repo,consumer,stream,cursor,sequence,body) VALUES (?,?,?,?,?,?) ON CONFLICT(repo,consumer,stream) DO UPDATE SET sequence=excluded.sequence,body=excluded.body,attempts=0,retry_at_ms=0",params![repo,consumer,stream.code(),number(cursor)?,number(sequence)?,body]).map_err(storage_error)?;
        tx.commit().map_err(storage_error)?;
        Ok(WebhookState {
            cursor,
            pending: Some(WebhookDelivery {
                sequence,
                body: body.to_vec(),
                attempts: 0,
                retry_at_ms: 0,
            }),
        })
    }
    fn finish_webhook(
        &self,
        repo: &[u8],
        consumer: &str,
        stream: WebhookStream,
        sequence: u64,
        retry_at_ms: Option<i64>,
    ) -> Result<bool> {
        let db = self.lock()?;
        let affected=if let Some(retry)=retry_at_ms {
            db.execute("UPDATE webhook_state SET attempts=attempts+1,retry_at_ms=MAX(retry_at_ms,?) WHERE repo=? AND consumer=? AND stream=? AND sequence=? AND attempts<9223372036854775807",params![retry,repo,consumer,stream.code(),number(sequence)?])
        } else {
            db.execute("UPDATE webhook_state SET cursor=sequence,sequence=NULL,body=NULL,attempts=0,retry_at_ms=0 WHERE repo=? AND consumer=? AND stream=? AND sequence=?",params![repo,consumer,stream.code(),number(sequence)?])
        }.map_err(storage_error)?;
        Ok(affected == 1)
    }
}
