use super::*;
fn state(
    client: &mut impl GenericClient,
    repo: &[u8],
    consumer: &str,
    stream: i16,
) -> Result<WebhookState> {
    let row=client.query_opt("SELECT cursor,sequence,body,attempts,retry_at_ms FROM kasu.webhook_state WHERE repo=$1 AND consumer=$2 AND stream=$3", &[&repo,&consumer,&stream]).map_err(pg_error)?;
    let Some(row) = row else {
        return Ok(WebhookState::default());
    };
    Ok(WebhookState {
        cursor: unsigned(row.get(0))?,
        pending: match (
            row.get::<_, Option<i64>>(1),
            row.get::<_, Option<Vec<u8>>>(2),
        ) {
            (None, None) => None,
            (Some(sequence), Some(body)) => Some(WebhookDelivery {
                sequence: unsigned(sequence)?,
                body,
                attempts: unsigned(row.get(3))?,
                retry_at_ms: row.get(4),
            }),
            _ => return Err(invalid("incomplete pending webhook state")),
        },
    })
}
impl WebhookStore for PostgresStore {
    fn webhook_state(
        &self,
        repo: &[u8],
        consumer: &str,
        stream: WebhookStream,
    ) -> Result<WebhookState> {
        self.client(|client| {
            let repo = scope(client, repo)?;
            state(client, &repo, consumer, stream.code() as i16)
        })
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
        self.client(|client| {
            let mut tx=client.transaction().map_err(pg_error)?;
            let repo=scope(&mut tx,repo)?; let stream=stream.code() as i16;
            tx.execute("INSERT INTO kasu.webhook_state(repo,consumer,stream) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING", &[&repo,&consumer,&stream]).map_err(pg_error)?;
            tx.query_one("SELECT cursor FROM kasu.webhook_state WHERE repo=$1 AND consumer=$2 AND stream=$3 FOR UPDATE", &[&repo,&consumer,&stream]).map_err(pg_error)?;
            let current=state(&mut tx,&repo,consumer,stream)?;
            if current.cursor!=cursor || current.pending.is_some() { return Ok(current); }
            tx.execute("UPDATE kasu.webhook_state SET sequence=$4,body=$5,attempts=0,retry_at_ms=0 WHERE repo=$1 AND consumer=$2 AND stream=$3", &[&repo,&consumer,&stream,&number(sequence)?,&body]).map_err(pg_error)?;
            tx.commit().map_err(pg_error)?;
            Ok(WebhookState { cursor,pending:Some(WebhookDelivery { sequence,body:body.to_vec(),attempts:0,retry_at_ms:0 }) })
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
        self.client(|client| {
            let repo=scope(client,repo)?; let stream=stream.code() as i16; let sequence=number(sequence)?;
            let affected=if let Some(retry)=retry_at_ms {
                client.execute("UPDATE kasu.webhook_state SET attempts=attempts+1,retry_at_ms=GREATEST(retry_at_ms,$5) WHERE repo=$1 AND consumer=$2 AND stream=$3 AND sequence=$4 AND attempts<9223372036854775807", &[&repo,&consumer,&stream,&sequence,&retry])
            } else {
                client.execute("UPDATE kasu.webhook_state SET cursor=sequence,sequence=NULL,body=NULL,attempts=0,retry_at_ms=0 WHERE repo=$1 AND consumer=$2 AND stream=$3 AND sequence=$4", &[&repo,&consumer,&stream,&sequence])
            }.map_err(pg_error)?;
            Ok(affected==1)
        })
    }
}
