//! Daemon-owned trajectory SQLite writer and query barrier.
//!
//! One bounded FIFO thread owns the write connection. Harness and UI threads
//! only enqueue or wait for a committed revision.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use parking_lot::{Condvar, Mutex};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use crate::persistence::{apply_migrations, configure_sqlite, open_sqlite_read_only};
use crate::trajectory::{
    TRAJECTORY_SCHEMA_VERSION, TrajectoryAvailability, TrajectoryBatch, TrajectoryInitSource,
    TrajectoryKind, TrajectoryLane, TrajectoryLiveOp, TrajectoryLiveUpdate, TrajectoryOp,
    TrajectoryPage, TrajectoryPrompt, TrajectoryRecord, TrajectorySessionMeta, TrajectoryStatus,
    clamp_page_limit, default_flush_timeout, project_legacy_session, project_snapshot,
};

const WRITER_BOUND: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum TrajectoryError {
    #[error("trajectory writer is unavailable")]
    Disconnected,
    #[error("trajectory store: {0}")]
    Store(String),
    #[error("trajectory timed out")]
    Timeout,
    #[error("trajectory is marked {status}")]
    Unavailable { status: String },
}

impl From<std::io::Error> for TrajectoryError {
    fn from(error: std::io::Error) -> Self {
        Self::Store(error.to_string())
    }
}

impl From<rusqlite::Error> for TrajectoryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.to_string())
    }
}

type LiveSink = Arc<dyn Fn(TrajectoryLiveUpdate) + Send + Sync>;
type Ack = Sender<Result<i64, TrajectoryError>>;

/// Outcome of a nonblocking shadow-event enqueue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionEventEnqueue {
    Queued,
    Full,
    Disconnected,
}

enum WriterCommand {
    Ensure {
        session_id: Uuid,
        source: Box<TrajectoryInitSource>,
        ack: Ack,
    },
    Submit {
        batch: TrajectoryBatch,
        promote_exact: bool,
        ack: Option<Ack>,
    },
    Flush {
        session_id: Uuid,
        ack: Ack,
    },
    Fork {
        source: Uuid,
        dest: Uuid,
        ack: Ack,
    },
    Rewind {
        session_id: Uuid,
        retained_turn: i64,
        ack: Ack,
    },
    MarkError {
        session_id: Uuid,
        message: String,
        ack: Option<Ack>,
    },
    AppendSessionEvents {
        session_id: Uuid,
        events: Vec<crate::session_events::NewSessionEvent>,
        ack: Option<Ack>,
    },
    FlushSessionEvents {
        session_id: Uuid,
        ack: Ack,
    },
    Shutdown {
        ack: Ack,
    },
}

struct RevisionGate {
    state: Mutex<RevisionMap>,
    ready: Condvar,
}

#[derive(Default)]
struct RevisionMap {
    committed: std::collections::HashMap<Uuid, i64>,
    failed: std::collections::HashMap<Uuid, String>,
}

impl RevisionGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(RevisionMap::default()),
            ready: Condvar::new(),
        })
    }

    fn commit(&self, session_id: Uuid, revision: i64) {
        let mut state = self.state.lock();
        state.committed.insert(session_id, revision);
        state.failed.remove(&session_id);
        self.ready.notify_all();
    }

    fn fail(&self, session_id: Uuid, message: String) {
        let mut state = self.state.lock();
        state.failed.insert(session_id, message);
        self.ready.notify_all();
    }

    fn committed(&self, session_id: Uuid) -> i64 {
        self.state
            .lock()
            .committed
            .get(&session_id)
            .copied()
            .unwrap_or(0)
    }

    fn wait_at_least(
        &self,
        session_id: Uuid,
        at_least: i64,
        timeout: Duration,
    ) -> Result<i64, TrajectoryError> {
        let mut state = self.state.lock();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(message) = state.failed.get(&session_id) {
                return Err(TrajectoryError::Unavailable {
                    status: message.clone(),
                });
            }
            let current = state.committed.get(&session_id).copied().unwrap_or(0);
            if current >= at_least {
                return Ok(current);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(TrajectoryError::Timeout);
            }
            if self.ready.wait_for(&mut state, remaining).timed_out() {
                return Err(TrajectoryError::Timeout);
            }
        }
    }

    fn fail_all(&self, message: &str) {
        let mut state = self.state.lock();
        let ids = state
            .committed
            .keys()
            .copied()
            .chain(state.failed.keys().copied())
            .collect::<Vec<_>>();
        for id in ids {
            state.failed.insert(id, message.to_owned());
        }
        self.ready.notify_all();
    }
}

pub struct TrajectoryWriter {
    path: PathBuf,
    tx: Sender<WriterCommand>,
    thread: Mutex<Option<JoinHandle<()>>>,
    revisions: Arc<RevisionGate>,
}

impl TrajectoryWriter {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, TrajectoryError> {
        Self::open_with_live(path, |_| {})
    }

    pub fn open_with_live(
        path: impl Into<PathBuf>,
        live: impl Fn(TrajectoryLiveUpdate) + Send + Sync + 'static,
    ) -> Result<Self, TrajectoryError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&path)?;
        configure_sqlite(&connection).map_err(to_store_error)?;
        apply_migrations(&connection).map_err(to_store_error)?;
        let (tx, rx) = bounded(WRITER_BOUND);
        let revisions = RevisionGate::new();
        let live: LiveSink = Arc::new(live);
        let thread_revisions = Arc::clone(&revisions);
        let handle = thread::Builder::new()
            .name("waku-trajectory-writer".into())
            .spawn(move || writer_loop(connection, rx, thread_revisions, live))
            .map_err(|error| TrajectoryError::Store(error.to_string()))?;
        Ok(Self {
            path,
            tx,
            thread: Mutex::new(Some(handle)),
            revisions,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn ensure_initialized(
        &self,
        session_id: Uuid,
        source: TrajectoryInitSource,
    ) -> Result<i64, TrajectoryError> {
        self.call(|ack| WriterCommand::Ensure {
            session_id,
            source: Box::new(source),
            ack,
        })
    }

    pub fn submit(&self, batch: TrajectoryBatch) -> Result<(), TrajectoryError> {
        self.tx
            .send(WriterCommand::Submit {
                batch,
                promote_exact: true,
                ack: None,
            })
            .map_err(|_| TrajectoryError::Disconnected)
    }

    pub fn apply(&self, batch: TrajectoryBatch) -> Result<i64, TrajectoryError> {
        self.call(|ack| WriterCommand::Submit {
            batch,
            promote_exact: true,
            ack: Some(ack),
        })
    }

    pub fn flush(&self, session_id: Uuid) -> Result<i64, TrajectoryError> {
        self.call(|ack| WriterCommand::Flush { session_id, ack })
    }

    pub fn fork(&self, source: Uuid, dest: Uuid) -> Result<i64, TrajectoryError> {
        self.call(|ack| WriterCommand::Fork { source, dest, ack })
    }

    pub fn rewind(&self, session_id: Uuid, retained_turn: i64) -> Result<i64, TrajectoryError> {
        self.call(|ack| WriterCommand::Rewind {
            session_id,
            retained_turn,
            ack,
        })
    }

    pub fn mark_error(
        &self,
        session_id: Uuid,
        message: impl Into<String>,
    ) -> Result<i64, TrajectoryError> {
        self.call(|ack| WriterCommand::MarkError {
            session_id,
            message: message.into(),
            ack: Some(ack),
        })
    }

    /// Blocking append for callers that need the resulting head. The
    /// single writer thread serializes all event access.
    pub fn append_session_events(
        &self,
        session_id: Uuid,
        events: Vec<crate::session_events::NewSessionEvent>,
    ) -> Result<i64, TrajectoryError> {
        let (ack, rx) = bounded(1);
        self.tx
            .send(WriterCommand::AppendSessionEvents {
                session_id,
                events,
                ack: Some(ack),
            })
            .map_err(|_| TrajectoryError::Disconnected)?;
        rx.recv_timeout(default_flush_timeout())
            .map_err(|_| TrajectoryError::Timeout)?
    }

    /// Nonblocking append for shadow capture: never waits on a full queue and
    /// distinguishes a full bounded channel from a stopped writer.
    pub fn try_append_session_events(
        &self,
        session_id: Uuid,
        events: Vec<crate::session_events::NewSessionEvent>,
    ) -> SessionEventEnqueue {
        match self.tx.try_send(WriterCommand::AppendSessionEvents {
            session_id,
            events,
            ack: None,
        }) {
            Ok(()) => SessionEventEnqueue::Queued,
            Err(crossbeam_channel::TrySendError::Full(_)) => SessionEventEnqueue::Full,
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                SessionEventEnqueue::Disconnected
            }
        }
    }

    /// FIFO barrier that returns the session's current event head, or 0 when
    /// no stream exists yet. Does not touch trajectory revisions.
    pub fn flush_session_events(&self, session_id: Uuid) -> Result<i64, TrajectoryError> {
        self.call(|ack| WriterCommand::FlushSessionEvents { session_id, ack })
    }

    pub fn committed_revision(&self, session_id: Uuid) -> i64 {
        self.revisions.committed(session_id)
    }

    pub fn page(
        &self,
        session_id: Uuid,
        before: Option<(i64, Uuid)>,
        limit: Option<u32>,
        at_least_revision: Option<i64>,
    ) -> Result<TrajectoryPage, TrajectoryError> {
        if let Some(at_least) = at_least_revision {
            self.revisions
                .wait_at_least(session_id, at_least, default_flush_timeout())?;
        }
        let connection = open_sqlite_read_only(&self.path).map_err(to_store_error)?;
        load_page(&connection, session_id, before, clamp_page_limit(limit))
    }

    pub fn detail_context(
        &self,
        session_id: Uuid,
        record_id: Uuid,
        at_least_revision: Option<i64>,
    ) -> Result<crate::trajectory_detail::TrajectoryDetailContext, TrajectoryError> {
        if let Some(at_least) = at_least_revision {
            self.revisions
                .wait_at_least(session_id, at_least, default_flush_timeout())?;
        }
        let connection = open_sqlite_read_only(&self.path).map_err(to_store_error)?;
        let meta = load_meta(&connection, session_id)?.ok_or_else(|| {
            TrajectoryError::Store(format!("trajectory session {session_id} is missing"))
        })?;
        let record = load_record(&connection, session_id, record_id)?.ok_or_else(|| {
            TrajectoryError::Store(format!("trajectory record {record_id} is missing"))
        })?;
        let prompt = record
            .prompt_id
            .map(|prompt_id| load_prompt(&connection, session_id, prompt_id))
            .transpose()?
            .flatten();
        let previous_system_prompt = prompt
            .as_ref()
            .map(|prompt| load_previous_system_prompt(&connection, session_id, prompt.sequence))
            .transpose()?
            .flatten();
        Ok(crate::trajectory_detail::TrajectoryDetailContext {
            meta,
            record,
            prompt,
            previous_system_prompt,
        })
    }

    fn call(&self, build: impl FnOnce(Ack) -> WriterCommand) -> Result<i64, TrajectoryError> {
        let (ack, rx) = bounded(1);
        self.tx
            .send(build(ack))
            .map_err(|_| TrajectoryError::Disconnected)?;
        rx.recv_timeout(default_flush_timeout())
            .map_err(|_| TrajectoryError::Timeout)?
    }
}

impl crate::trajectory::TrajectorySubmit for TrajectoryWriter {
    fn submit_batch(&self, batch: crate::trajectory::TrajectoryBatch) -> Result<(), String> {
        self.submit(batch).map_err(|error| error.to_string())
    }

    fn flush_session(&self, session_id: Uuid) -> Result<i64, String> {
        self.flush(session_id).map_err(|error| error.to_string())
    }

    fn mark_session_error(&self, session_id: Uuid, message: &str) {
        let _ = self.mark_error(session_id, message);
    }
}
impl Drop for TrajectoryWriter {
    fn drop(&mut self) {
        let (ack, rx) = bounded(1);
        let _ = self.tx.send(WriterCommand::Shutdown { ack });
        let _ = rx.recv_timeout(Duration::from_secs(5));
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
    }
}

fn writer_loop(
    mut connection: Connection,
    rx: Receiver<WriterCommand>,
    revisions: Arc<RevisionGate>,
    live: LiveSink,
) {
    while let Ok(command) = rx.recv() {
        match command {
            WriterCommand::Shutdown { ack } => {
                let _ = ack.send(Ok(0));
                break;
            }
            WriterCommand::Ensure {
                session_id,
                source,
                ack,
            } => {
                let result =
                    ensure_initialized(&mut connection, session_id, *source, &revisions, &live);
                let _ = ack.send(result);
            }
            WriterCommand::Submit {
                batch,
                promote_exact,
                ack,
            } => {
                let result = apply_batch(&mut connection, batch, promote_exact, &revisions, &live);
                if let Some(ack) = ack {
                    let _ = ack.send(result);
                }
            }
            WriterCommand::Flush { session_id, ack } => {
                let _ = ack.send(Ok(revisions.committed(session_id)));
            }
            WriterCommand::Fork { source, dest, ack } => {
                let result = fork_session(&mut connection, source, dest, &revisions, &live);
                let _ = ack.send(result);
            }
            WriterCommand::Rewind {
                session_id,
                retained_turn,
                ack,
            } => {
                let result = rewind_session(
                    &mut connection,
                    session_id,
                    retained_turn,
                    &revisions,
                    &live,
                );
                let _ = ack.send(result);
            }
            WriterCommand::MarkError {
                session_id,
                message,
                ack,
            } => {
                let result = mark_error(&mut connection, session_id, &message, &revisions, &live);
                if let Some(ack) = ack {
                    let _ = ack.send(result);
                }
            }
            WriterCommand::AppendSessionEvents {
                session_id,
                events,
                ack,
            } => {
                let result = append_session_events_on(&mut connection, session_id, events);
                if let Some(ack) = ack {
                    let _ = ack.send(result);
                }
            }
            WriterCommand::FlushSessionEvents { session_id, ack } => {
                let _ = ack.send(session_event_head_on(&connection, session_id));
            }
        }
    }
    revisions.fail_all("writer stopped");
}

fn ensure_initialized(
    connection: &mut Connection,
    session_id: Uuid,
    source: TrajectoryInitSource,
    revisions: &RevisionGate,
    live: &LiveSink,
) -> Result<i64, TrajectoryError> {
    if let Some(meta) = load_meta(connection, session_id)? {
        revisions.commit(session_id, meta.revision);
        return Ok(meta.revision);
    }
    let (availability, batch) = match source {
        TrajectoryInitSource::Snapshot(snapshot) => (
            TrajectoryAvailability::Legacy,
            project_snapshot(session_id, &snapshot),
        ),
        TrajectoryInitSource::LegacyPartial(session) => (
            TrajectoryAvailability::LegacyPartialMissingSnapshot,
            project_legacy_session(session_id, &session),
        ),
        TrajectoryInitSource::Empty => (
            TrajectoryAvailability::Exact,
            TrajectoryBatch {
                session_id,
                ops: Vec::new(),
            },
        ),
    };
    match persist_session_batch(connection, session_id, availability, &batch, false) {
        Ok(meta) => {
            revisions.commit(session_id, meta.revision);
            live(TrajectoryLiveUpdate {
                session_id,
                generation: meta.generation,
                revision: meta.revision,
                op: TrajectoryLiveOp::Reset,
            });
            Ok(meta.revision)
        }
        Err(error) => {
            revisions.fail(session_id, error.to_string());
            Err(error)
        }
    }
}

fn apply_batch(
    connection: &mut Connection,
    batch: TrajectoryBatch,
    promote_exact: bool,
    revisions: &RevisionGate,
    live: &LiveSink,
) -> Result<i64, TrajectoryError> {
    let session_id = batch.session_id;
    if load_meta(connection, session_id)?.is_none()
        && let Err(error) = insert_skeleton(connection, session_id, TrajectoryAvailability::Exact)
    {
        revisions.fail(session_id, error.to_string());
        return Err(error);
    }
    match persist_ops(connection, &batch, promote_exact) {
        Ok(meta) => {
            revisions.commit(session_id, meta.revision);
            let ids = batch
                .ops
                .iter()
                .filter_map(|op| match op {
                    TrajectoryOp::UpsertRecord(record) => Some(record.record_id),
                    TrajectoryOp::UpsertPrompt(_) => None,
                })
                .collect::<Vec<_>>();
            if !ids.is_empty() {
                let records = load_records_by_ids(connection, session_id, &ids)?;
                live(TrajectoryLiveUpdate {
                    session_id,
                    generation: meta.generation,
                    revision: meta.revision,
                    op: TrajectoryLiveOp::Upsert(records),
                });
            }
            Ok(meta.revision)
        }
        Err(error) => {
            let _ = set_availability(connection, session_id, TrajectoryAvailability::Error);
            revisions.fail(session_id, error.to_string());
            Err(error)
        }
    }
}

fn persist_session_batch(
    connection: &mut Connection,
    session_id: Uuid,
    availability: TrajectoryAvailability,
    batch: &TrajectoryBatch,
    promote_exact: bool,
) -> Result<TrajectorySessionMeta, TrajectoryError> {
    let txn = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    insert_skeleton_on(&txn, session_id, availability)?;
    apply_ops_on(&txn, batch, promote_exact)?;
    let meta = load_meta_on(&txn, session_id)?
        .ok_or_else(|| TrajectoryError::Store("trajectory session missing after init".into()))?;
    txn.commit()?;
    Ok(meta)
}

fn persist_ops(
    connection: &mut Connection,
    batch: &TrajectoryBatch,
    promote_exact: bool,
) -> Result<TrajectorySessionMeta, TrajectoryError> {
    let txn = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    apply_ops_on(&txn, batch, promote_exact)?;
    let meta = load_meta_on(&txn, batch.session_id)?
        .ok_or_else(|| TrajectoryError::Store("trajectory session missing".into()))?;
    txn.commit()?;
    Ok(meta)
}

fn apply_ops_on(
    connection: &Connection,
    batch: &TrajectoryBatch,
    promote_exact: bool,
) -> Result<(), TrajectoryError> {
    let session_id = batch.session_id;
    let mut meta = load_meta_on(connection, session_id)?
        .ok_or_else(|| TrajectoryError::Store("trajectory session is not initialized".into()))?;
    for op in &batch.ops {
        match op {
            TrajectoryOp::UpsertPrompt(prompt) => {
                upsert_prompt(connection, session_id, prompt, &mut meta)?;
            }
            TrajectoryOp::UpsertRecord(record) => {
                upsert_record(connection, session_id, record, &mut meta)?;
            }
        }
    }
    meta.revision += 1;
    if promote_exact {
        meta.availability = TrajectoryAvailability::Exact;
    }
    update_meta(connection, &meta)?;
    Ok(())
}

fn insert_skeleton(
    connection: &Connection,
    session_id: Uuid,
    availability: TrajectoryAvailability,
) -> Result<(), TrajectoryError> {
    insert_skeleton_on(connection, session_id, availability)
}

fn insert_skeleton_on(
    connection: &Connection,
    session_id: Uuid,
    availability: TrajectoryAvailability,
) -> Result<(), TrajectoryError> {
    connection.execute(
        "INSERT OR IGNORE INTO trajectory_sessions (
             session_id, schema_version, generation, revision, next_sequence, availability
         ) VALUES (?1, ?2, 1, 0, 1, ?3)",
        params![
            session_id.to_string(),
            TRAJECTORY_SCHEMA_VERSION,
            availability.as_str()
        ],
    )?;
    Ok(())
}

fn upsert_prompt(
    connection: &Connection,
    session_id: Uuid,
    prompt: &TrajectoryPrompt,
    meta: &mut TrajectorySessionMeta,
) -> Result<(), TrajectoryError> {
    let existing: Option<i64> = connection
        .query_row(
            "SELECT sequence FROM trajectory_prompt_snapshots
             WHERE session_id = ?1 AND prompt_id = ?2",
            params![session_id.to_string(), prompt.prompt_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let sequence = match existing {
        Some(sequence) => sequence,
        None => {
            let sequence = meta.next_sequence;
            meta.next_sequence += 1;
            sequence
        }
    };
    connection.execute(
        "INSERT INTO trajectory_prompt_snapshots (
             session_id, prompt_id, sequence, fingerprint, system_prompt,
             tools_json, options_json, model_hint, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(session_id, prompt_id) DO UPDATE SET
             fingerprint = excluded.fingerprint,
             system_prompt = excluded.system_prompt,
             tools_json = excluded.tools_json,
             options_json = excluded.options_json,
             model_hint = excluded.model_hint",
        params![
            session_id.to_string(),
            prompt.prompt_id.to_string(),
            sequence,
            prompt.fingerprint,
            prompt.system_prompt,
            prompt.tools_json,
            prompt.options_json,
            prompt.model_hint,
            prompt.created_at_ms,
        ],
    )?;
    Ok(())
}

fn upsert_record(
    connection: &Connection,
    session_id: Uuid,
    record: &TrajectoryRecord,
    meta: &mut TrajectorySessionMeta,
) -> Result<(), TrajectoryError> {
    let existing: Option<i64> = connection
        .query_row(
            "SELECT sequence FROM trajectory_records
             WHERE session_id = ?1 AND record_id = ?2",
            params![session_id.to_string(), record.record_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let sequence = match existing {
        Some(sequence) => sequence,
        None => {
            let sequence = meta.next_sequence;
            meta.next_sequence += 1;
            sequence
        }
    };
    connection.execute(
        "INSERT INTO trajectory_records (
             session_id, record_id, sequence, revision, request_id, parent_record_id,
             prompt_id, turn_count, step, kind, lane, status, title, preview, search_text,
             started_at_ms, first_token_at_ms, completed_at_ms, duration_ms, ttft_ms, detail_json
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
             ?16, ?17, ?18, ?19, ?20, ?21
         )
         ON CONFLICT(session_id, record_id) DO UPDATE SET
             revision = excluded.revision,
             request_id = excluded.request_id,
             parent_record_id = excluded.parent_record_id,
             prompt_id = excluded.prompt_id,
             turn_count = excluded.turn_count,
             step = excluded.step,
             kind = excluded.kind,
             lane = excluded.lane,
             status = excluded.status,
             title = excluded.title,
             preview = excluded.preview,
             search_text = excluded.search_text,
             started_at_ms = excluded.started_at_ms,
             first_token_at_ms = excluded.first_token_at_ms,
             completed_at_ms = excluded.completed_at_ms,
             duration_ms = excluded.duration_ms,
             ttft_ms = excluded.ttft_ms,
             detail_json = excluded.detail_json",
        params![
            session_id.to_string(),
            record.record_id.to_string(),
            sequence,
            meta.revision + 1,
            record.request_id.map(|id| id.to_string()),
            record.parent_record_id.map(|id| id.to_string()),
            record.prompt_id.map(|id| id.to_string()),
            record.turn_count,
            record.step,
            record.kind.as_str(),
            record.lane.as_str(),
            record.status.as_str(),
            record.title,
            record.preview,
            record.search_text,
            record.started_at_ms,
            record.first_token_at_ms,
            record.completed_at_ms,
            record.duration_ms,
            record.ttft_ms,
            record.detail_json,
        ],
    )?;
    Ok(())
}

fn update_meta(
    connection: &Connection,
    meta: &TrajectorySessionMeta,
) -> Result<(), TrajectoryError> {
    connection.execute(
        "UPDATE trajectory_sessions
         SET schema_version = ?2, generation = ?3, revision = ?4,
             next_sequence = ?5, availability = ?6
         WHERE session_id = ?1",
        params![
            meta.session_id.to_string(),
            meta.schema_version,
            meta.generation,
            meta.revision,
            meta.next_sequence,
            meta.availability.as_str(),
        ],
    )?;
    Ok(())
}

fn set_availability(
    connection: &Connection,
    session_id: Uuid,
    availability: TrajectoryAvailability,
) -> Result<(), TrajectoryError> {
    connection.execute(
        "UPDATE trajectory_sessions SET availability = ?2 WHERE session_id = ?1",
        params![session_id.to_string(), availability.as_str()],
    )?;
    Ok(())
}

fn fork_session(
    connection: &mut Connection,
    source: Uuid,
    dest: Uuid,
    revisions: &RevisionGate,
    live: &LiveSink,
) -> Result<i64, TrajectoryError> {
    let Some(source_meta) = load_meta(connection, source)? else {
        return Ok(0);
    };
    let txn = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    txn.execute(
        "INSERT INTO trajectory_sessions (
             session_id, schema_version, generation, revision, next_sequence, availability
         )
         SELECT ?2, schema_version, generation, revision, next_sequence, availability
         FROM trajectory_sessions WHERE session_id = ?1",
        params![source.to_string(), dest.to_string()],
    )?;
    txn.execute(
        "INSERT INTO trajectory_prompt_snapshots (
             session_id, prompt_id, sequence, fingerprint, system_prompt,
             tools_json, options_json, model_hint, created_at_ms
         )
         SELECT ?2, prompt_id, sequence, fingerprint, system_prompt,
                tools_json, options_json, model_hint, created_at_ms
         FROM trajectory_prompt_snapshots WHERE session_id = ?1",
        params![source.to_string(), dest.to_string()],
    )?;
    txn.execute(
        "INSERT INTO trajectory_records (
             session_id, record_id, sequence, revision, request_id, parent_record_id,
             prompt_id, turn_count, step, kind, lane, status, title, preview, search_text,
             started_at_ms, first_token_at_ms, completed_at_ms, duration_ms, ttft_ms, detail_json
         )
         SELECT ?2, record_id, sequence, revision, request_id, parent_record_id,
                prompt_id, turn_count, step, kind, lane, status, title, preview, search_text,
                started_at_ms, first_token_at_ms, completed_at_ms, duration_ms, ttft_ms, detail_json
         FROM trajectory_records WHERE session_id = ?1",
        params![source.to_string(), dest.to_string()],
    )?;
    let provenance = TrajectoryRecord {
        record_id: Uuid::new_v4(),
        sequence: 0,
        revision: 0,
        request_id: None,
        parent_record_id: None,
        prompt_id: None,
        turn_count: 0,
        step: 0,
        kind: TrajectoryKind::Context,
        lane: TrajectoryLane::Input,
        status: TrajectoryStatus::Completed,
        title: "Forked".into(),
        preview: source.to_string(),
        search_text: source.to_string(),
        started_at_ms: None,
        first_token_at_ms: None,
        completed_at_ms: None,
        duration_ms: None,
        ttft_ms: None,
        detail_json: serde_json::json!({
            "v": 1,
            "kind": "context",
            "forked_from": source,
        })
        .to_string(),
    };
    let mut dest_meta = source_meta.clone();
    dest_meta.session_id = dest;
    upsert_record(&txn, dest, &provenance, &mut dest_meta)?;
    dest_meta.revision += 1;
    update_meta(&txn, &dest_meta)?;
    txn.commit()?;
    revisions.commit(dest, dest_meta.revision);
    live(TrajectoryLiveUpdate {
        session_id: dest,
        generation: dest_meta.generation,
        revision: dest_meta.revision,
        op: TrajectoryLiveOp::Reset,
    });
    Ok(dest_meta.revision)
}

fn rewind_session(
    connection: &mut Connection,
    session_id: Uuid,
    retained_turn: i64,
    revisions: &RevisionGate,
    live: &LiveSink,
) -> Result<i64, TrajectoryError> {
    let Some(mut meta) = load_meta(connection, session_id)? else {
        return Ok(0);
    };
    let txn = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    txn.execute(
        "DELETE FROM trajectory_records
         WHERE session_id = ?1 AND turn_count > ?2",
        params![session_id.to_string(), retained_turn],
    )?;
    txn.execute(
        "DELETE FROM trajectory_prompt_snapshots
         WHERE session_id = ?1
           AND prompt_id NOT IN (
               SELECT prompt_id FROM trajectory_records
               WHERE session_id = ?1 AND prompt_id IS NOT NULL
           )",
        params![session_id.to_string()],
    )?;
    let next: i64 = txn.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM trajectory_records WHERE session_id = ?1",
        params![session_id.to_string()],
        |row| row.get(0),
    )?;
    meta.next_sequence = next;
    meta.generation += 1;
    meta.revision += 1;
    update_meta(&txn, &meta)?;
    txn.commit()?;
    revisions.commit(session_id, meta.revision);
    live(TrajectoryLiveUpdate {
        session_id,
        generation: meta.generation,
        revision: meta.revision,
        op: TrajectoryLiveOp::Reset,
    });
    Ok(meta.revision)
}

fn mark_error(
    connection: &mut Connection,
    session_id: Uuid,
    message: &str,
    revisions: &RevisionGate,
    live: &LiveSink,
) -> Result<i64, TrajectoryError> {
    let _ = insert_skeleton(connection, session_id, TrajectoryAvailability::Error);
    let Some(mut meta) = load_meta(connection, session_id)? else {
        revisions.fail(session_id, message.to_owned());
        return Err(TrajectoryError::Store(message.to_owned()));
    };
    meta.availability = TrajectoryAvailability::Error;
    meta.revision += 1;
    update_meta(connection, &meta)?;
    revisions.fail(session_id, message.to_owned());
    live(TrajectoryLiveUpdate {
        session_id,
        generation: meta.generation,
        revision: meta.revision,
        op: TrajectoryLiveOp::Reset,
    });
    Ok(meta.revision)
}

fn append_session_events_on(
    connection: &mut Connection,
    session_id: Uuid,
    events: Vec<crate::session_events::NewSessionEvent>,
) -> Result<i64, TrajectoryError> {
    use crate::session_events::SESSION_EVENT_SCHEMA_VERSION;

    if events.is_empty() {
        return Err(TrajectoryError::Store(
            "session event append received an empty batch".into(),
        ));
    }

    struct PreparedEvent {
        event_id: String,
        command_id: Option<String>,
        payload_json: String,
        kind: &'static str,
        created_at_ms: i64,
        runtime_id: Option<String>,
        turn_id: Option<String>,
    }
    let prepared = events
        .into_iter()
        .map(|event| {
            let kind = event.payload.kind();
            let payload_json = serde_json::to_string(&event.payload)
                .map_err(|error| TrajectoryError::Store(error.to_string()))?;
            Ok(PreparedEvent {
                event_id: event.event_id.to_string(),
                command_id: event.command_id,
                payload_json,
                kind,
                created_at_ms: event.created_at_ms,
                runtime_id: event.runtime_id.map(|id| id.to_string()),
                turn_id: event.turn_id.map(|id| id.to_string()),
            })
        })
        .collect::<Result<Vec<_>, TrajectoryError>>()?;

    let command_id = prepared
        .iter()
        .filter_map(|event| event.command_id.as_ref())
        .try_fold(None::<&String>, |acc, next| match acc {
            None => Ok(Some(next)),
            Some(current) if current == next => Ok(Some(current)),
            Some(_) => Err(TrajectoryError::Store(
                "session event batch mixes multiple command ids".into(),
            )),
        })?
        .cloned();

    let txn_started_ms = crate::model::unix_time_millis() as i64;
    let txn = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let stream_id = session_id.to_string();
    let created_at_ms: Option<i64> = txn
        .query_row(
            "SELECT created_at FROM sessions WHERE id = ?1",
            params![session_id.to_string()],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    let Some(created_at_ms) = created_at_ms else {
        return Err(TrajectoryError::Store(format!(
            "session event append for unknown session {session_id}"
        )));
    };

    txn.execute(
        "INSERT OR IGNORE INTO session_streams (stream_id, session_id, created_at_ms)
         VALUES (?1, ?2, ?3)",
        params![
            stream_id,
            session_id.to_string(),
            created_at_ms.saturating_mul(1000)
        ],
    )?;
    txn.execute(
        "INSERT OR IGNORE INTO session_heads (stream_id, head_seq, revision, schema_version, updated_at_ms)
         VALUES (?1, 0, 0, ?2, ?3)",
        params![
            stream_id,
            SESSION_EVENT_SCHEMA_VERSION,
            txn_started_ms
        ],
    )?;

    let (mut head_seq, revision, mut last_event_id, schema_version): (
        i64,
        i64,
        Option<String>,
        i64,
    ) = txn.query_row(
        "SELECT head_seq, revision, last_event_id, schema_version
         FROM session_heads WHERE stream_id = ?1",
        params![stream_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    if schema_version != SESSION_EVENT_SCHEMA_VERSION {
        return Err(TrajectoryError::Store(format!(
            "session event stream {stream_id} uses schema {schema_version}; expected {SESSION_EVENT_SCHEMA_VERSION}"
        )));
    }

    if let Some(command_id) = &command_id {
        let existing = {
            let mut statement = txn.prepare(
                "SELECT event_id, kind, payload_json, runtime_id, turn_id
                 FROM session_events WHERE stream_id = ?1 AND command_id = ?2
                 ORDER BY seq",
            )?;
            let rows = statement.query_map(params![stream_id, command_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if !existing.is_empty() {
            let matches = existing.len() == prepared.len()
                && existing.iter().zip(&prepared).all(|(row, event)| {
                    row.0 == event.event_id
                        && row.1 == event.kind
                        && row.2 == event.payload_json
                        && row.3 == event.runtime_id
                        && row.4 == event.turn_id
                });
            if matches {
                txn.commit()?;
                return Ok(head_seq);
            }
            return Err(TrajectoryError::Store(format!(
                "session event command {command_id} already recorded with different events"
            )));
        }
    }

    for event in &prepared {
        head_seq += 1;
        txn.execute(
            "INSERT INTO session_events (
                 stream_id, seq, event_id, command_id, schema_version, kind,
                 payload_json, created_at_ms, runtime_id, turn_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                stream_id,
                head_seq,
                event.event_id,
                command_id,
                SESSION_EVENT_SCHEMA_VERSION,
                event.kind,
                event.payload_json,
                event.created_at_ms,
                event.runtime_id,
                event.turn_id,
            ],
        )?;
        last_event_id = Some(event.event_id.clone());
    }
    txn.execute(
        "UPDATE session_heads
         SET head_seq = ?2, revision = ?3, last_event_id = ?4, updated_at_ms = ?5
         WHERE stream_id = ?1",
        params![
            stream_id,
            head_seq,
            revision + 1,
            last_event_id,
            txn_started_ms
        ],
    )?;
    txn.commit()?;
    Ok(head_seq)
}

fn session_event_head_on(
    connection: &Connection,
    session_id: Uuid,
) -> Result<i64, TrajectoryError> {
    match connection.query_row(
        "SELECT head_seq FROM session_heads WHERE stream_id = ?1",
        params![session_id.to_string()],
        |row| row.get(0),
    ) {
        Ok(head_seq) => Ok(head_seq),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
        Err(error) => Err(TrajectoryError::Store(error.to_string())),
    }
}

fn load_meta(
    connection: &Connection,
    session_id: Uuid,
) -> Result<Option<TrajectorySessionMeta>, TrajectoryError> {
    load_meta_on(connection, session_id)
}

fn load_meta_on(
    connection: &Connection,
    session_id: Uuid,
) -> Result<Option<TrajectorySessionMeta>, TrajectoryError> {
    connection
        .query_row(
            "SELECT schema_version, generation, revision, next_sequence, availability
             FROM trajectory_sessions WHERE session_id = ?1",
            params![session_id.to_string()],
            |row| {
                Ok(TrajectorySessionMeta {
                    session_id,
                    schema_version: row.get(0)?,
                    generation: row.get(1)?,
                    revision: row.get(2)?,
                    next_sequence: row.get(3)?,
                    availability: TrajectoryAvailability::parse(&row.get::<_, String>(4)?)
                        .unwrap_or(TrajectoryAvailability::Unavailable),
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn load_page(
    connection: &Connection,
    session_id: Uuid,
    before: Option<(i64, Uuid)>,
    limit: u32,
) -> Result<TrajectoryPage, TrajectoryError> {
    let Some(meta) = load_meta(connection, session_id)? else {
        return Ok(TrajectoryPage::empty(session_id));
    };
    let take = i64::from(limit) + 1;
    let mut records = if let Some((sequence, record_id)) = before {
        let mut statement = connection.prepare(
            "SELECT record_id, sequence, revision, request_id, parent_record_id, prompt_id,
                    turn_count, step, kind, lane, status, title, preview, search_text,
                    started_at_ms, first_token_at_ms, completed_at_ms, duration_ms, ttft_ms,
                    detail_json
             FROM trajectory_records
             WHERE session_id = ?1
               AND (sequence < ?2 OR (sequence = ?2 AND record_id < ?3))
             ORDER BY sequence DESC, record_id DESC
             LIMIT ?4",
        )?;
        map_records(statement.query_map(
            params![
                session_id.to_string(),
                sequence,
                record_id.to_string(),
                take
            ],
            record_from_row,
        )?)?
    } else {
        let mut statement = connection.prepare(
            "SELECT record_id, sequence, revision, request_id, parent_record_id, prompt_id,
                    turn_count, step, kind, lane, status, title, preview, search_text,
                    started_at_ms, first_token_at_ms, completed_at_ms, duration_ms, ttft_ms,
                    detail_json
             FROM trajectory_records
             WHERE session_id = ?1
             ORDER BY sequence DESC, record_id DESC
             LIMIT ?2",
        )?;
        map_records(statement.query_map(params![session_id.to_string(), take], record_from_row)?)?
    };
    let has_older = records.len() as u32 > limit;
    if has_older {
        records.truncate(limit as usize);
    }
    records.reverse();
    let older_cursor = records
        .first()
        .map(|record| (record.sequence, record.record_id));
    let newer_cursor = records
        .last()
        .map(|record| (record.sequence, record.record_id));
    let has_newer = if let Some((sequence, record_id)) = newer_cursor {
        connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM trajectory_records
                 WHERE session_id = ?1
                   AND (sequence > ?2 OR (sequence = ?2 AND record_id > ?3))
             )",
            params![session_id.to_string(), sequence, record_id.to_string()],
            |row| row.get(0),
        )?
    } else {
        false
    };
    Ok(TrajectoryPage {
        session_id,
        availability: meta.availability,
        generation: meta.generation,
        revision: meta.revision,
        records,
        has_older,
        has_newer,
        older_cursor,
        newer_cursor,
    })
}

fn map_records(
    rows: rusqlite::MappedRows<
        '_,
        impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<TrajectoryRecord>,
    >,
) -> Result<Vec<TrajectoryRecord>, TrajectoryError> {
    let mut records = Vec::new();
    for row in rows {
        records.push(row?);
    }
    Ok(records)
}

fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrajectoryRecord> {
    let kind = TrajectoryKind::parse(&row.get::<_, String>(8)?).unwrap_or(TrajectoryKind::Context);
    Ok(TrajectoryRecord {
        record_id: parse_uuid(row.get::<_, String>(0)?),
        sequence: row.get(1)?,
        revision: row.get(2)?,
        request_id: parse_uuid_opt(row.get(3)?),
        parent_record_id: parse_uuid_opt(row.get(4)?),
        prompt_id: parse_uuid_opt(row.get(5)?),
        turn_count: row.get(6)?,
        step: row.get(7)?,
        kind,
        lane: TrajectoryLane::parse(&row.get::<_, String>(9)?).unwrap_or(kind.lane()),
        status: TrajectoryStatus::parse(&row.get::<_, String>(10)?)
            .unwrap_or(TrajectoryStatus::Unavailable),
        title: row.get(11)?,
        preview: row.get(12)?,
        search_text: row.get(13)?,
        started_at_ms: row.get(14)?,
        first_token_at_ms: row.get(15)?,
        completed_at_ms: row.get(16)?,
        duration_ms: row.get(17)?,
        ttft_ms: row.get(18)?,
        detail_json: row.get(19)?,
    })
}

fn load_record(
    connection: &Connection,
    session_id: Uuid,
    record_id: Uuid,
) -> Result<Option<TrajectoryRecord>, TrajectoryError> {
    let mut statement = connection.prepare(
        "SELECT record_id, sequence, revision, request_id, parent_record_id, prompt_id,
                turn_count, step, kind, lane, status, title, preview, search_text,
                started_at_ms, first_token_at_ms, completed_at_ms, duration_ms, ttft_ms,
                detail_json
         FROM trajectory_records
         WHERE session_id = ?1 AND record_id = ?2",
    )?;
    statement
        .query_row(
            params![session_id.to_string(), record_id.to_string()],
            record_from_row,
        )
        .optional()
        .map_err(Into::into)
}

fn load_records_by_ids(
    connection: &Connection,
    session_id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<TrajectoryRecord>, TrajectoryError> {
    let mut records = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(record) = load_record(connection, session_id, *id)? {
            records.push(record);
        }
    }
    Ok(records)
}

fn load_prompt(
    connection: &Connection,
    session_id: Uuid,
    prompt_id: Uuid,
) -> Result<Option<TrajectoryPrompt>, TrajectoryError> {
    connection
        .query_row(
            "SELECT prompt_id, sequence, fingerprint, system_prompt, tools_json,
                    options_json, model_hint, created_at_ms
             FROM trajectory_prompt_snapshots
             WHERE session_id = ?1 AND prompt_id = ?2",
            params![session_id.to_string(), prompt_id.to_string()],
            |row| {
                Ok(TrajectoryPrompt {
                    prompt_id: parse_uuid(row.get(0)?),
                    sequence: row.get(1)?,
                    fingerprint: row.get(2)?,
                    system_prompt: row.get(3)?,
                    tools_json: row.get(4)?,
                    options_json: row.get(5)?,
                    model_hint: row.get(6)?,
                    created_at_ms: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn load_previous_system_prompt(
    connection: &Connection,
    session_id: Uuid,
    sequence: i64,
) -> Result<Option<String>, TrajectoryError> {
    connection
        .query_row(
            "SELECT system_prompt FROM trajectory_prompt_snapshots
             WHERE session_id = ?1 AND sequence < ?2
             ORDER BY sequence DESC LIMIT 1",
            params![session_id.to_string(), sequence],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

fn parse_uuid(value: String) -> Uuid {
    Uuid::parse_str(&value).unwrap_or(Uuid::nil())
}

fn parse_uuid_opt(value: Option<String>) -> Option<Uuid> {
    value.and_then(|value| Uuid::parse_str(&value).ok())
}

fn to_store_error(error: std::io::Error) -> TrajectoryError {
    TrajectoryError::Store(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MessageRole;
    use crate::persistence::{PersistedState, StateStore, configure_sqlite};
    use crate::trajectory::{TraceHandoff, TrajectoryKind, TrajectoryUserInput, record_drive};
    use rusqlite::{Connection, params};
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use wakuwaku_harness::{
        AssistantMessage, ContentBlock, Message, StopReason, TextBlock, TraceEvent, Usage,
    };

    fn fixture() -> (std::path::PathBuf, StateStore, TrajectoryWriter, Uuid) {
        let directory = std::env::temp_dir().join(format!("wakuwaku-traj-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.clone());
        let session_id = state.sessions[0].id;
        state.sessions[0].begin_turn("seed");
        state.mark_session_dirty(session_id);
        store.save(&mut state).unwrap();
        let writer = TrajectoryWriter::open(store.path()).unwrap();
        (directory, store, writer, session_id)
    }

    fn cleanup(directory: std::path::PathBuf) {
        std::fs::remove_dir_all(directory).ok();
    }

    fn user_batch(session_id: Uuid, text: &str) -> TrajectoryBatch {
        record_drive(
            session_id,
            Some(TrajectoryUserInput {
                text: text.into(),
                display_text: Some(text.into()),
                ..TrajectoryUserInput::default()
            }),
            Vec::new(),
            vec![TraceEvent::PromptPrepared {
                system_prompt: Some("sys".into()),
                tools_json: Arc::from("[]"),
                options_json: Arc::from("{}"),
                model_hint: "m".into(),
            }],
        )
    }

    #[test]
    fn immediate_transaction_waits_for_a_peer_write_lock_then_commits() {
        let (directory, store, _writer, session_id) = fixture();
        let seed_row = session_id.to_string();

        // Seed one trajectory session row so both contenders UPDATE a row
        // that already satisfies the FK to `sessions`.
        let seed = Connection::open(store.path()).unwrap();
        configure_sqlite(&seed).unwrap();
        seed.execute(
            "INSERT INTO trajectory_sessions (
                 session_id, schema_version, generation, revision, next_sequence, availability
             ) VALUES (?1, 1, 0, 0, 1, 'exact')",
            [&seed_row],
        )
        .unwrap();
        drop(seed);

        let mut a = Connection::open(store.path()).unwrap();
        configure_sqlite(&a).unwrap();
        let mut b = Connection::open(store.path()).unwrap();
        configure_sqlite(&b).unwrap();

        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel::<()>();
        let (committed_tx, committed_rx) = std::sync::mpsc::channel::<Instant>();
        let holder_row = seed_row.clone();
        let holder = std::thread::spawn(move || {
            let txn = a
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            // The immediate transaction holds the write lock from here.
            acquired_tx.send(()).unwrap();
            txn.execute(
                "UPDATE trajectory_sessions SET revision = revision + 1 WHERE session_id = ?1",
                [&holder_row],
            )
            .unwrap();
            std::thread::sleep(Duration::from_millis(200));
            let committed = Instant::now();
            txn.commit().unwrap();
            committed_tx.send(committed).unwrap();
        });

        // B only starts once A provably holds the write lock.
        acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let waiter_row = seed_row;
        let waiter = std::thread::spawn(move || {
            let started = Instant::now();
            let txn = b
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            let acquired = Instant::now();
            txn.execute(
                "UPDATE trajectory_sessions SET generation = generation + 1 WHERE session_id = ?1",
                [&waiter_row],
            )
            .unwrap();
            txn.commit().unwrap();
            (started, acquired)
        });

        holder.join().unwrap();
        let holder_committed = committed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (waiter_started, waiter_acquired) = waiter.join().unwrap();

        let connection = Connection::open(store.path()).unwrap();
        configure_sqlite(&connection).unwrap();
        let (revision, generation): (i64, i64) = connection
            .query_row(
                "SELECT revision, generation FROM trajectory_sessions WHERE session_id = ?1",
                [session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (revision, generation),
            (1, 1),
            "both writers must commit under the busy timeout"
        );
        assert!(
            waiter_started < holder_committed,
            "B must begin contending before A releases the lock"
        );
        assert!(
            waiter_acquired > holder_committed,
            "B's immediate transaction must have waited for A's commit"
        );
        drop(connection);
        cleanup(directory);
    }

    fn observed_event(
        payload: crate::session_events::SessionEventPayload,
    ) -> crate::session_events::NewSessionEvent {
        crate::session_events::NewSessionEvent::observed(None, None, payload)
    }

    #[test]
    fn session_events_append_assigns_contiguous_seq_and_advances_head() {
        let (directory, _store, writer, session_id) = fixture();
        let first_event =
            observed_event(crate::session_events::SessionEventPayload::TurnStarted {});
        let second_event =
            observed_event(crate::session_events::SessionEventPayload::TurnFinished {
                success: true,
                summary: Some("done".into()),
            });
        let head = writer
            .append_session_events(session_id, vec![first_event, second_event])
            .unwrap();
        assert_eq!(head, 2);
        assert_eq!(writer.flush_session_events(session_id).unwrap(), 2);

        let final_event =
            observed_event(crate::session_events::SessionEventPayload::ProcessExited {});
        let expected_final_id = final_event.event_id;
        let head = writer
            .append_session_events(session_id, vec![final_event])
            .unwrap();
        assert_eq!(head, 3);

        let connection = Connection::open(_store.path()).unwrap();
        configure_sqlite(&connection).unwrap();
        let seqs: Vec<i64> = connection
            .prepare("SELECT seq FROM session_events WHERE stream_id = ?1 ORDER BY seq")
            .unwrap()
            .query_map([session_id.to_string()], |row| row.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        let (head_seq, revision, last_event_id): (i64, i64, Option<String>) = connection
            .query_row(
                "SELECT head_seq, revision, last_event_id FROM session_heads WHERE stream_id = ?1",
                [session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(head_seq, 3);
        assert_eq!(revision, 2);
        assert_eq!(
            last_event_id.as_deref(),
            Some(expected_final_id.to_string().as_str()),
            "head must point at the final appended event"
        );
        let (stream_created_ms, session_created_s): (i64, i64) = connection
            .query_row(
                "SELECT (SELECT created_at_ms FROM session_streams WHERE stream_id = ?1),
                        (SELECT created_at FROM sessions WHERE id = ?1)",
                [session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            stream_created_ms,
            session_created_s.saturating_mul(1000),
            "stream creation derives from the session row's seconds timestamp"
        );
        drop(connection);
        cleanup(directory);
    }

    #[test]
    fn exact_command_retry_returns_existing_head_without_new_rows() {
        let (directory, _store, writer, session_id) = fixture();
        let command_id = "retry-1".to_string();
        let first_event =
            observed_event(crate::session_events::SessionEventPayload::TurnStarted {})
                .with_command_id(command_id.clone());
        let first = writer
            .append_session_events(session_id, vec![first_event.clone()])
            .unwrap();
        // A retry replays the same command with identical events; only the
        // created timestamp may differ.
        let mut retried = first_event.clone();
        retried.created_at_ms += 7;
        let second = writer
            .append_session_events(session_id, vec![retried])
            .unwrap();
        assert_eq!(first, second);
        let connection = Connection::open(_store.path()).unwrap();
        configure_sqlite(&connection).unwrap();
        let (rows, head_seq, revision, last_event_id): (i64, i64, i64, Option<String>) = connection
            .query_row(
                "SELECT
                        (SELECT COUNT(*) FROM session_events WHERE stream_id = ?1),
                        head_seq, revision, last_event_id
                     FROM session_heads WHERE stream_id = ?1",
                [session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(rows, 1, "an exact retry appends nothing");
        assert_eq!(head_seq, 1);
        assert_eq!(revision, 1, "an exact retry does not bump the revision");
        assert_eq!(
            last_event_id.as_deref(),
            Some(first_event.event_id.to_string().as_str()),
            "an exact retry keeps the original head event"
        );
        drop(connection);
        cleanup(directory);
    }

    #[test]
    fn mismatched_command_reuse_conflicts() {
        let (directory, _store, writer, session_id) = fixture();
        let command_id = "retry-2".to_string();
        writer
            .append_session_events(
                session_id,
                vec![
                    observed_event(crate::session_events::SessionEventPayload::TurnStarted {})
                        .with_command_id(command_id.clone()),
                ],
            )
            .unwrap();
        let error = writer
            .append_session_events(
                session_id,
                vec![
                    observed_event(crate::session_events::SessionEventPayload::ProcessExited {})
                        .with_command_id(command_id.clone()),
                ],
            )
            .unwrap_err();
        assert!(error.to_string().contains("different events"), "{error}");
        assert_eq!(writer.flush_session_events(session_id).unwrap(), 1);
        cleanup(directory);
    }

    #[test]
    fn duplicate_event_id_rolls_back_whole_batch() {
        let (directory, _store, writer, session_id) = fixture();
        let seeded = observed_event(crate::session_events::SessionEventPayload::TurnStarted {});
        let seeded_id = seeded.event_id;
        writer
            .append_session_events(session_id, vec![seeded.clone()])
            .unwrap();
        // A later batch reusing the committed event id must roll back without
        // disturbing the committed head.
        let error = writer
            .append_session_events(
                session_id,
                vec![
                    observed_event(crate::session_events::SessionEventPayload::ProcessExited {}),
                    seeded,
                    observed_event(crate::session_events::SessionEventPayload::Error {
                        message: "x".into(),
                    }),
                ],
            )
            .unwrap_err();
        assert!(error.to_string().contains("UNIQUE"), "{error}");
        let connection = Connection::open(_store.path()).unwrap();
        configure_sqlite(&connection).unwrap();
        let (rows, head_seq, revision, last_event_id): (i64, i64, i64, Option<String>) = connection
            .query_row(
                "SELECT
                        (SELECT COUNT(*) FROM session_events WHERE stream_id = ?1),
                        head_seq, revision, last_event_id
                     FROM session_heads WHERE stream_id = ?1",
                [session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(rows, 1);
        assert_eq!(head_seq, 1);
        assert_eq!(revision, 1);
        assert_eq!(
            last_event_id.as_deref(),
            Some(seeded_id.to_string().as_str()),
            "rollback must preserve the committed head event"
        );
        drop(connection);
        cleanup(directory);
    }

    #[test]
    fn unknown_schema_and_bad_batches_reject_without_mutation() {
        let (directory, store, writer, session_id) = fixture();
        // Empty batch rejects before any transaction opens.
        assert!(
            writer
                .append_session_events(session_id, Vec::new())
                .is_err()
        );
        // Unknown session rejects and writes no stream.
        assert!(
            writer
                .append_session_events(
                    Uuid::new_v4(),
                    vec![observed_event(
                        crate::session_events::SessionEventPayload::TurnStarted {}
                    )]
                )
                .is_err()
        );
        assert_eq!(
            writer.flush_session_events(Uuid::new_v4()).unwrap(),
            0,
            "no stream exists for an unknown session"
        );
        {
            let connection = Connection::open(store.path()).unwrap();
            configure_sqlite(&connection).unwrap();
            let streams: i64 = connection
                .query_row("SELECT COUNT(*) FROM session_streams", [], |row| row.get(0))
                .unwrap();
            assert_eq!(streams, 0, "the rejected append must leave no stream");
            drop(connection);
        }

        // Seed the fixture session's own head at schema 2; appends must
        // refuse before writing any row.
        drop(writer);
        {
            let mut connection = Connection::open(store.path()).unwrap();
            configure_sqlite(&connection).unwrap();
            let txn = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            txn.execute(
                "INSERT INTO session_streams (stream_id, session_id, created_at_ms)
                 VALUES (?1, ?2, 1)",
                params![session_id.to_string(), session_id.to_string()],
            )
            .unwrap();
            txn.execute(
                "INSERT INTO session_heads (stream_id, schema_version, updated_at_ms)
                 VALUES (?1, 2, 1)",
                params![session_id.to_string()],
            )
            .unwrap();
            txn.commit().unwrap();
        }
        let writer = TrajectoryWriter::open(store.path()).unwrap();
        let error = writer
            .append_session_events(
                session_id,
                vec![observed_event(
                    crate::session_events::SessionEventPayload::TurnStarted {},
                )],
            )
            .unwrap_err();
        assert!(error.to_string().contains("expected 1"), "{error}");
        let connection = Connection::open(store.path()).unwrap();
        configure_sqlite(&connection).unwrap();
        let events: i64 = connection
            .query_row("SELECT COUNT(*) FROM session_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(events, 0, "schema mismatch must not write rows");
        drop(connection);
        cleanup(directory);
    }

    #[test]
    fn deleting_a_session_leaves_diagnostic_event_rows() {
        let (directory, store, writer, session_id) = fixture();
        writer
            .append_session_events(
                session_id,
                vec![observed_event(
                    crate::session_events::SessionEventPayload::TurnStarted {},
                )],
            )
            .unwrap();
        // Seed a legacy usage row so the deletion comparison is meaningful.
        StateStore::daemon(store.path().to_path_buf())
            .insert_usage_event(&crate::usage_history::UsageEvent {
                event_id: Uuid::new_v4(),
                session_id,
                project_path: String::new(),
                provider: crate::model::ProviderId::new("test"),
                model: "m".into(),
                timestamp_ms: 1,
                input: 1,
                output: 1,
                cache_read: 0,
                cache_write: 0,
                reasoning: None,
            })
            .unwrap();
        drop(writer);
        let connection = Connection::open(store.path()).unwrap();
        configure_sqlite(&connection).unwrap();
        connection
            .execute(
                "DELETE FROM sessions WHERE id = ?1",
                params![session_id.to_string()],
            )
            .unwrap();
        let streams: i64 = connection
            .query_row("SELECT COUNT(*) FROM session_streams", [], |row| row.get(0))
            .unwrap();
        let events: i64 = connection
            .query_row("SELECT COUNT(*) FROM session_events", [], |row| row.get(0))
            .unwrap();
        let usage: i64 = connection
            .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(streams, 1, "diagnostic streams survive session delete");
        assert_eq!(events, 1);
        assert_eq!(usage, 1, "usage_events rows are never cascade-deleted");
        drop(connection);
        cleanup(directory);
    }

    #[test]
    fn trajectory_fk_cascade_removes_all_three_tables() {
        let (directory, store, writer, session_id) = fixture();
        writer
            .ensure_initialized(session_id, TrajectoryInitSource::Empty)
            .unwrap();
        writer.apply(user_batch(session_id, "hi")).unwrap();
        drop(writer);

        let connection = Connection::open(store.path()).unwrap();
        configure_sqlite(&connection).unwrap();
        connection
            .execute(
                "DELETE FROM sessions WHERE id = ?1",
                params![session_id.to_string()],
            )
            .unwrap();
        let sessions: i64 = connection
            .query_row("SELECT COUNT(*) FROM trajectory_sessions", [], |row| {
                row.get(0)
            })
            .unwrap();
        let prompts: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM trajectory_prompt_snapshots",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let records: i64 = connection
            .query_row("SELECT COUNT(*) FROM trajectory_records", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!((sessions, prompts, records), (0, 0, 0));
        cleanup(directory);
    }

    #[test]
    fn trajectory_live_update_sees_committed_rows() {
        let (directory, store, _, session_id) = fixture();
        let (live_tx, live_rx) = mpsc::channel();
        let path = store.path().to_path_buf();
        let writer = TrajectoryWriter::open_with_live(store.path(), {
            let path = path.clone();
            move |update| {
                let connection = Connection::open(&path).unwrap();
                configure_sqlite(&connection).unwrap();
                let count: i64 = connection
                    .query_row(
                        "SELECT COUNT(*) FROM trajectory_records WHERE session_id = ?1",
                        params![update.session_id.to_string()],
                        |row| row.get(0),
                    )
                    .unwrap();
                live_tx.send((update.revision, count)).unwrap();
            }
        })
        .unwrap();
        writer
            .ensure_initialized(session_id, TrajectoryInitSource::Empty)
            .unwrap();
        let _ = live_rx.recv_timeout(Duration::from_secs(2));
        let revision = writer.apply(user_batch(session_id, "hi")).unwrap();
        let (live_revision, count) = live_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(live_revision, revision);
        assert!(count > 0);
        cleanup(directory);
    }

    #[test]
    fn trajectory_flush_waits_for_submitted_batches() {
        let (directory, _, writer, session_id) = fixture();
        writer
            .ensure_initialized(session_id, TrajectoryInitSource::Empty)
            .unwrap();
        writer.submit(user_batch(session_id, "one")).unwrap();
        writer.submit(user_batch(session_id, "two")).unwrap();
        let revision = writer.flush(session_id).unwrap();
        let page = writer
            .page(session_id, None, Some(200), Some(revision))
            .unwrap();
        assert!(page.revision >= revision);
        assert!(
            page.records
                .iter()
                .any(|record| record.kind == TrajectoryKind::User)
        );
        cleanup(directory);
    }

    #[test]
    fn trajectory_page_waits_for_at_least_revision() {
        let (directory, _, writer, session_id) = fixture();
        writer
            .ensure_initialized(session_id, TrajectoryInitSource::Empty)
            .unwrap();
        let writer = Arc::new(writer);
        let pending = Arc::clone(&writer);
        let handle = std::thread::spawn(move || {
            pending
                .page(session_id, None, Some(10), Some(2))
                .expect("barrier")
        });
        std::thread::sleep(Duration::from_millis(20));
        writer.apply(user_batch(session_id, "hi")).unwrap();
        let page = handle.join().unwrap();
        assert!(page.revision >= 2);
        cleanup(directory);
    }

    #[test]
    fn trajectory_write_failure_marks_error_and_leaves_sessions() {
        let (directory, store, writer, session_id) = fixture();
        writer
            .ensure_initialized(session_id, TrajectoryInitSource::Empty)
            .unwrap();
        drop(writer);
        let connection = Connection::open(store.path()).unwrap();
        connection
            .execute_batch("DROP TABLE trajectory_records")
            .unwrap();
        drop(connection);
        let writer = TrajectoryWriter::open(store.path()).unwrap();
        let error = writer.apply(user_batch(session_id, "hi")).unwrap_err();
        assert!(matches!(error, TrajectoryError::Store(_)));
        let mut state = store.load().unwrap();
        assert_eq!(state.sessions.len(), 1);
        state.sessions[0].title = "still here".into();
        store.save(&mut state).unwrap();
        cleanup(directory);
    }

    #[test]
    fn trajectory_legacy_backfill_reads_snapshot_file_only() {
        let (directory, store, writer, session_id) = fixture();
        let snapshot = wakuwaku_harness::Session::with_messages(
            Some("sys".into()),
            vec![
                Message::User(wakuwaku_harness::UserMessage::text("file user")),
                Message::Assistant(Arc::new(AssistantMessage {
                    content: vec![ContentBlock::Text(TextBlock {
                        text: "file assistant".into(),
                        signature: None,
                    })],
                    model: "m".into(),
                    provider: "p".into(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                })),
            ],
        )
        .snapshot();
        store
            .persist_harness_snapshot(session_id, snapshot.clone())
            .unwrap();
        writer
            .ensure_initialized(session_id, TrajectoryInitSource::Snapshot(snapshot))
            .unwrap();
        let page = writer.page(session_id, None, Some(50), None).unwrap();
        assert_eq!(page.availability, TrajectoryAvailability::Legacy);
        assert!(
            page.records
                .iter()
                .any(|record| record.preview.contains("file user"))
        );
        assert!(
            page.records
                .iter()
                .all(|record| record.duration_ms.is_none())
        );
        cleanup(directory);
    }

    #[test]
    fn trajectory_missing_snapshot_marks_legacy_partial() {
        let (directory, store, writer, session_id) = fixture();
        let mut state = store.load().unwrap();
        store.hydrate(&mut state.sessions[0]).unwrap();
        state.sessions[0].begin_turn("legacy user");
        state.sessions[0].push_message(MessageRole::Assistant, "legacy assistant");
        let session = state.sessions[0].clone();
        writer
            .ensure_initialized(
                session_id,
                TrajectoryInitSource::LegacyPartial(Box::new(session)),
            )
            .unwrap();
        let page = writer.page(session_id, None, Some(50), None).unwrap();
        assert_eq!(
            page.availability,
            TrajectoryAvailability::LegacyPartialMissingSnapshot
        );
        assert!(
            page.records
                .iter()
                .any(|record| record.preview.contains("legacy user"))
        );
        assert!(page.records.iter().all(|record| record.ttft_ms.is_none()));
        cleanup(directory);
    }

    #[test]
    fn trajectory_fork_copies_records_and_rewind_bumps_generation() {
        let (directory, store, writer, session_id) = fixture();
        writer
            .ensure_initialized(session_id, TrajectoryInitSource::Empty)
            .unwrap();
        writer.apply(user_batch(session_id, "keep")).unwrap();
        let start = Instant::now();
        writer
            .apply(record_drive(
                session_id,
                Some(TrajectoryUserInput {
                    text: "drop".into(),
                    ..TrajectoryUserInput::default()
                }),
                Vec::new(),
                vec![
                    TraceEvent::PromptPrepared {
                        system_prompt: None,
                        tools_json: Arc::from("[]"),
                        options_json: Arc::from("{}"),
                        model_hint: "m".into(),
                    },
                    TraceEvent::RequestStart {
                        visible_turn: 2,
                        step: 1,
                        started_at: start,
                    },
                    TraceEvent::AssistantDone(Arc::new(AssistantMessage {
                        content: vec![ContentBlock::Text(TextBlock {
                            text: "second".into(),
                            signature: None,
                        })],
                        model: "m".into(),
                        provider: "p".into(),
                        response_id: None,
                        usage: Usage::default(),
                        stop_reason: StopReason::Stop,
                        error_message: None,
                    })),
                ],
            ))
            .unwrap();
        let mut state = store.load().unwrap();
        let dest = Uuid::new_v4();
        let mut forked = state.sessions[0].clone();
        forked.id = dest;
        forked.title = "fork".into();
        state.push_session(forked);
        store.save(&mut state).unwrap();
        writer.fork(session_id, dest).unwrap();
        let forked_page = writer.page(dest, None, Some(200), None).unwrap();
        assert!(
            forked_page
                .records
                .iter()
                .any(|record| record.title == "Forked")
        );
        assert!(
            forked_page
                .records
                .iter()
                .any(|record| record.preview.contains("keep"))
        );

        let before = writer.page(session_id, None, Some(200), None).unwrap();
        writer.rewind(session_id, 1).unwrap();
        let after = writer.page(session_id, None, Some(200), None).unwrap();
        assert_eq!(after.generation, before.generation + 1);
        assert!(after.records.iter().all(|record| record.turn_count <= 1));
        cleanup(directory);
    }

    #[test]
    fn trajectory_live_request_commits_before_provider_completes() {
        use crate::trajectory::{TrajectoryStatus, TrajectorySubmit};
        use wakuwaku_harness::{
            AssistantMessage, ContentBlock, StopReason, TextBlock, TraceSink, Usage,
        };

        let (directory, _, writer, session_id) = fixture();
        writer
            .ensure_initialized(session_id, TrajectoryInitSource::Empty)
            .unwrap();
        let writer = Arc::new(writer);
        let handoff = TraceHandoff::spawn(Arc::clone(&writer) as Arc<dyn TrajectorySubmit>);
        handoff.stage_user(
            session_id,
            TrajectoryUserInput {
                text: "hello".into(),
                display_text: Some("hello".into()),
                ..TrajectoryUserInput::default()
            },
        );

        let start = Instant::now();
        let gate = Arc::new((parking_lot::Mutex::new(false), parking_lot::Condvar::new()));
        let worker = {
            let handoff_sink = handoff.sink(session_id);
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                let mut sink = handoff_sink;
                sink.emit(TraceEvent::PromptPrepared {
                    system_prompt: Some("system".into()),
                    tools_json: Arc::from("[]"),
                    options_json: Arc::from("{}"),
                    model_hint: "grok-4.5".into(),
                });
                sink.emit(TraceEvent::RequestStart {
                    visible_turn: 1,
                    step: 1,
                    started_at: start,
                });
                let (lock, ready) = &*gate;
                let mut open = lock.lock();
                while !*open {
                    ready.wait(&mut open);
                }
                sink.emit(TraceEvent::RequestFirstToken {
                    visible_turn: 1,
                    step: 1,
                    at: start + Duration::from_millis(8),
                });
                sink.emit(TraceEvent::AssistantDone(Arc::new(AssistantMessage {
                    content: vec![ContentBlock::Text(TextBlock {
                        text: "done".into(),
                        signature: None,
                    })],
                    model: "grok-4.5".into(),
                    provider: "opencode-go".into(),
                    response_id: Some("response-native".into()),
                    usage: Usage {
                        input: 741,
                        output: 23,
                        cache_read: 128,
                        cache_write: 0,
                        reasoning: Some(16),
                        total_tokens: 908,
                    },
                    stop_reason: StopReason::Stop,
                    error_message: None,
                })));
            })
        };

        let started = wait_until(Duration::from_secs(2), || {
            writer
                .page(session_id, None, Some(50), None)
                .ok()
                .map(|page| {
                    page.records.iter().any(|record| {
                        record.kind == TrajectoryKind::Request
                            && record.status == TrajectoryStatus::Running
                    })
                })
                .unwrap_or(false)
        });
        assert!(
            started,
            "RequestStart must commit before the provider finishes"
        );
        let start_revision = writer.committed_revision(session_id);

        {
            let (lock, ready) = &*gate;
            *lock.lock() = true;
            ready.notify_all();
        }
        worker.join().unwrap();

        let token_advanced = wait_until(Duration::from_secs(2), || {
            writer.committed_revision(session_id) > start_revision
                && writer
                    .page(session_id, None, Some(50), None)
                    .ok()
                    .map(|page| {
                        page.records.iter().any(|record| {
                            record.kind == TrajectoryKind::Request && record.ttft_ms.is_some()
                        })
                    })
                    .unwrap_or(false)
        });
        assert!(
            token_advanced,
            "RequestFirstToken must advance revision before TurnFinished"
        );

        let revision = handoff.finish_and_flush(session_id).expect("finish");
        let page = writer
            .page(session_id, None, Some(50), Some(revision))
            .unwrap();
        assert!(
            page.records.iter().any(|record| {
                record.kind == TrajectoryKind::Request
                    && record.status == TrajectoryStatus::Completed
            }),
            "TurnFinished flush waits for the completion commit"
        );
        let request = page
            .records
            .iter()
            .find(|record| record.kind == TrajectoryKind::Request)
            .expect("request row");
        assert!(request.ttft_ms.is_some());
        assert!(request.duration_ms.is_some());
        let assistant = page
            .records
            .iter()
            .find(|record| record.kind == TrajectoryKind::Assistant)
            .expect("assistant row");
        assert_eq!(assistant.preview, "done");
        assert!(assistant.detail_json.contains("\"input\":741"));
        assert!(assistant.detail_json.contains("\"reasoning\":16"));
        assert_eq!(page.availability, TrajectoryAvailability::Exact);
        cleanup(directory);
    }

    fn wait_until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if ready() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        ready()
    }
}
