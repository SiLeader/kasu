pub mod cache;
pub use cache::{CacheSimulation, simulate_cache};
pub mod faults;
use faults::{Fault, FaultStore};
use kasu_core::*;
use kasu_derived::{Ctx, Engine, flatten};
use kasu_ops::Operations;
use kasu_proto::v1 as p;
use kasu_server::{Backend, LandAttempt, LandOperation, LandPreparation, PreparedLand};
use kasu_store::*;
use prost::Message;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, atomic::Ordering},
};

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Counts {
    pub landed: usize,
    pub conflict: usize,
    pub retry: usize,
}
fn counts(results: &[p::LandResponse]) -> Counts {
    let mut counts = Counts::default();
    for result in results {
        match p::LandOutcome::try_from(result.outcome).unwrap() {
            p::LandOutcome::Landed => counts.landed += 1,
            p::LandOutcome::Conflict => counts.conflict += 1,
            p::LandOutcome::Retry => counts.retry += 1,
            other => panic!("unexpected outcome {other:?}"),
        }
    }
    counts
}
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct LandSimulation {
    pub seed: u64,
    pub clients: usize,
    pub initial: Counts,
    pub after_resubmission: Counts,
    pub virtual_time_ms: u64,
    pub scheduled_attempts: u64,
    pub max_attempts_per_request: u32,
    pub storage_failures: u64,
    pub lost_replies: u64,
    pub cancellations: usize,
    pub restarted_replays: usize,
    pub ref_log_entries: usize,
    pub final_files: usize,
    pub result_digest: String,
}
struct Client {
    request: p::LandRequest,
    operation: LandOperation,
    outcome: Option<p::LandResponse>,
    path: RepoPath,
    blob: BlobId,
}
enum Event {
    Prepare(usize, u32),
    Commit(usize, u32, Box<PreparedLand>),
}
struct Schedule {
    events: BTreeMap<(u64, u64), Event>,
    sequence: u64,
    random: u64,
}
impl Schedule {
    fn new(seed: u64) -> Self {
        Self {
            events: BTreeMap::new(),
            sequence: 0,
            random: seed,
        }
    }
    fn random(&mut self) -> u64 {
        self.random = self
            .random
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.random
    }
    fn push(&mut self, time: u64, event: Event) {
        self.events.insert((time, self.sequence), event);
        self.sequence += 1;
    }
}
fn signature() -> Signature {
    Signature {
        name: b"simulation".as_slice().into(),
        email: b"sim@kasu.invalid".as_slice().into(),
        timestamp: 1,
        tz: b"+0000".as_slice().into(),
    }
}

/// All clients begin against the same ref snapshot. Prepare and commit events
/// interleave under virtual time, using the production Land state machine and
/// real SQLite/FS stores. No OS scheduling or wall-clock sleeps affect outcomes.
pub fn simulate_land(seed: u64, count: usize) -> LandSimulation {
    assert!((2..=10_000).contains(&count));
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("meta.db");
    let store = Arc::new(SqliteStore::open(&db).unwrap());
    let blobs = Arc::new(FsBlobStore::open(temp.path().join("objects")).unwrap());
    let faults = Arc::new(FaultStore::new(store.clone()));
    let backend = Arc::new(Backend::new(
        b"sim".to_vec(),
        store.clone(),
        blobs.clone(),
        faults.clone(),
        None,
    ));
    let engine = Engine::standard();
    let ops = Operations {
        engine: &engine,
        ctx: Ctx {
            repo: Repository {
                meta: store.as_ref(),
                blobs: blobs.as_ref(),
            },
        },
    };
    let root = ops
        .create_changeset(&Changeset {
            parents: vec![],
            author: signature(),
            committer: signature(),
            message: b"root".as_slice().into(),
            changes: vec![],
            git_provenance: None,
        })
        .unwrap();
    ops.ctx.repo.associate_changeset(b"sim", root).unwrap();
    store
        .compare_and_swap(
            b"sim",
            &[RefUpdate {
                name: b"refs/heads/main".to_vec(),
                expected: RefState::default(),
                new: Some(root),
            }],
            b"setup",
            0,
        )
        .unwrap();
    let mut clients = Vec::new();
    let mut schedule = Schedule::new(seed);
    for n in 0..count {
        let path = RepoPath::new(if n % 100 == 0 {
            b"shared.txt".to_vec()
        } else {
            format!("work/{n:04}.txt").into_bytes()
        })
        .unwrap();
        let blob = ops
            .ctx
            .repo
            .put_blob(format!("client {n} seed {seed}\n").as_bytes())
            .unwrap();
        ops.ctx.repo.associate_blob(b"sim", blob).unwrap();
        let id = ops
            .create_changeset(&Changeset {
                parents: vec![root],
                author: signature(),
                committer: signature(),
                message: format!("client {n}").into_bytes().into(),
                changes: vec![FileChange::Set {
                    path: path.clone(),
                    entry: Entry::File {
                        blob,
                        executable: false,
                    },
                }],
                git_provenance: None,
            })
            .unwrap();
        ops.ctx.repo.associate_changeset(b"sim", id).unwrap();
        let key = format!("seed-{seed}/client-{n}").into_bytes();
        if n % 31 == 0 {
            faults.inject(key.clone(), Fault::BeforeCommit);
        } else if n % 43 == 0 {
            faults.inject(key.clone(), Fault::LostReply);
        }
        let request = p::LandRequest {
            required_ancestor: Vec::new(),
            repo: b"sim".to_vec(),
            id: id.0.to_vec(),
            name: b"refs/heads/main".to_vec(),
            idempotency_key: key,
            max_attempts: 3,
            timeout_ms: 1000,
            require_unborn: false,
        };
        let operation = backend.begin_land(request.clone()).unwrap();
        clients.push(Client {
            request,
            operation,
            outcome: None,
            path,
            blob,
        });
        schedule.push(0, Event::Prepare(n, 1));
    }
    let mut now = 0;
    let mut attempts = 0;
    let mut cancellations = 0;
    while let Some(((time, _), event)) = schedule.events.pop_first() {
        now = time;
        match event {
            Event::Prepare(n, attempt) => {
                attempts += 1;
                match backend
                    .prepare_land(&clients[n].operation, attempt)
                    .unwrap()
                {
                    LandPreparation::Complete(response) => clients[n].outcome = Some(response),
                    LandPreparation::Ready(prepared) => {
                        let delay = 1 + schedule.random() % 3;
                        schedule.push(now + delay, Event::Commit(n, attempt, Box::new(prepared)));
                    }
                }
            }
            Event::Commit(n, attempt, prepared) => {
                let cancel = n % 37 == 0 && attempt == 1;
                let retry = match backend.commit_land(*prepared, &|| cancel) {
                    Ok(LandAttempt::Complete(response)) => {
                        clients[n].outcome = Some(response);
                        None
                    }
                    Ok(LandAttempt::Contended(actual)) => Some(Some(actual)),
                    Ok(LandAttempt::Cancelled) => {
                        cancellations += 1;
                        clients[n].outcome = Some(clients[n].operation.retry(
                            attempt,
                            None,
                            "client cancelled",
                        ));
                        None
                    }
                    Err(error) => {
                        assert_eq!(error.code(), tonic::Code::Internal);
                        Some(None)
                    }
                };
                if let Some(actual) = retry {
                    let op = &clients[n].operation;
                    if attempt == op.max_attempts() {
                        clients[n].outcome =
                            Some(op.retry(attempt, actual, "attempt budget exhausted"));
                    } else {
                        schedule.push(
                            now + op.backoff(attempt).as_millis() as u64,
                            Event::Prepare(n, attempt + 1),
                        );
                    }
                }
            }
        }
    }
    let initial_results: Vec<_> = clients
        .iter()
        .map(|c| c.outcome.clone().expect("every request terminates"))
        .collect();
    assert!(initial_results.iter().all(|r| r.attempts <= 3));
    let initial = counts(&initial_results);
    // Explicit client resubmissions use the same semantic request/key and a new
    // bounded budget. Drain without competing events to verify eventual content
    // preservation, rather than accepting a simulation where everybody retries.
    for client in &mut clients {
        if client.outcome.as_ref().unwrap().outcome == p::LandOutcome::Retry as i32 {
            for _ in 0..3 {
                match backend.prepare_land(&client.operation, 1).unwrap() {
                    LandPreparation::Complete(response) => {
                        client.outcome = Some(response);
                        break;
                    }
                    LandPreparation::Ready(prepared) => {
                        match backend.commit_land(prepared, &|| false) {
                            Ok(LandAttempt::Complete(response)) => {
                                client.outcome = Some(response);
                                break;
                            }
                            Err(error) => assert_eq!(error.code(), tonic::Code::Internal),
                            _ => panic!("uncontended request must finish"),
                        }
                    }
                }
            }
        }
    }
    let results: Vec<_> = clients.iter().map(|c| c.outcome.clone().unwrap()).collect();
    let final_counts = counts(&results);
    assert_eq!(final_counts.retry, 0);
    assert!(final_counts.landed > 0);
    let head = store.resolve(b"sim", b"refs/heads/main").unwrap();
    let tree = ops.root_tree(head.target.unwrap()).unwrap();
    let files = flatten(&ops.ctx.repo, tree).unwrap();
    let mut landed_targets = BTreeSet::new();
    for (client, result) in clients.iter().zip(&results) {
        let record = store
            .land_record(b"sim", &client.request.idempotency_key)
            .unwrap()
            .expect("terminal result persisted");
        let stored = p::LandResponse::decode(record.response.as_slice()).unwrap();
        let mut expected = result.clone();
        expected.replayed = false;
        assert_eq!(stored, expected);
        if result.outcome == p::LandOutcome::Landed as i32 {
            assert_eq!(
                files.get(&client.path).and_then(Entry::blob),
                Some(client.blob),
                "lost accepted change"
            );
            assert!(
                landed_targets.insert(result.id.clone().unwrap()),
                "duplicate landing"
            );
        }
    }
    let log = store.tail(b"sim", 0, count + 10).unwrap();
    assert_eq!(log.len(), 1 + final_counts.landed);
    assert_eq!(files.len(), final_counts.landed);
    assert_eq!(head.version, log.len() as u64);
    // A fresh server and independent SQLite connection must replay all results
    // without recomputing a rewrite or changing refs/logs.
    let reopened = Arc::new(SqliteStore::open(&db).unwrap());
    let recovered = Backend::new(
        b"sim".to_vec(),
        reopened.clone(),
        blobs,
        reopened.clone(),
        None,
    );
    for (client, result) in clients.iter().zip(&results) {
        let operation = recovered.begin_land(client.request.clone()).unwrap();
        let LandPreparation::Complete(mut response) =
            recovered.prepare_land(&operation, 1).unwrap()
        else {
            panic!("missing durable replay")
        };
        assert!(response.replayed);
        response.replayed = false;
        let mut result = result.clone();
        result.replayed = false;
        assert_eq!(response, result);
    }
    assert_eq!(reopened.tail(b"sim", 0, count + 10).unwrap(), log);
    let mut digest = Vec::new();
    for result in &results {
        let mut result = result.clone();
        result.replayed = false;
        let bytes = result.encode_to_vec();
        digest.extend((bytes.len() as u64).to_be_bytes());
        digest.extend(bytes);
    }
    LandSimulation {
        seed,
        clients: count,
        initial,
        after_resubmission: final_counts,
        virtual_time_ms: now,
        scheduled_attempts: attempts,
        max_attempts_per_request: 3,
        storage_failures: faults.failures.load(Ordering::Relaxed),
        lost_replies: faults.lost_replies.load(Ordering::Relaxed),
        cancellations,
        restarted_replays: count,
        ref_log_entries: log.len(),
        final_files: files.len(),
        result_digest: ObjectId::hash(&digest).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seeded_schedule_is_repeatable_and_preserves_every_accepted_write() {
        assert_eq!(simulate_land(42, 32), simulate_land(42, 32));
    }
}
