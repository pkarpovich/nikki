use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use super::{Cursor, Envelope, KeySource, Kind, Provider, RecordDraft, Timestamp};

pub const DATABASE_FILE: &str = "buffer.db";
pub const DEAD_LETTER_MAX_ROWS: u64 = 5_000;
pub const DEAD_LETTER_MAX_BYTES: u64 = 50 * 1024 * 1024;
pub const OVERFLOW_HEADROOM_BYTES: u64 = 512;
pub const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

const SEQ_KEY: &str = "seq";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS pending(
  id INTEGER PRIMARY KEY,
  envelope TEXT NOT NULL,
  bytes INTEGER NOT NULL,
  created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS cursors(
  provider TEXT NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  PRIMARY KEY(provider, key)
);
CREATE TABLE IF NOT EXISTS dead_letter(
  id INTEGER PRIMARY KEY,
  envelope TEXT NOT NULL,
  bytes INTEGER NOT NULL,
  reason TEXT NOT NULL,
  at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS meta(
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
";

#[derive(Debug, thiserror::Error)]
pub enum BufferError {
    #[error("the state directory {} could not be created: {source}", path.display())]
    StateDir { path: PathBuf, source: io::Error },
    #[error("the buffer database {} could not be opened: {source}", path.display())]
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },
    #[error("the buffer owner thread could not start: {source}")]
    Worker { source: io::Error },
    #[error("a buffer statement failed: {0}")]
    Statement(#[from] rusqlite::Error),
    #[error("a record could not be serialised: {0}")]
    Serialise(#[from] serde_json::Error),
    #[error("the buffer counter `{key}` holds `{value}`, which is not a number")]
    Counter { key: &'static str, value: String },
    #[error("the buffer owner thread is gone")]
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferConfig {
    pub state_dir: PathBuf,
    pub device: String,
    pub max_rows: u64,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRecord {
    pub id: i64,
    pub envelope: String,
    pub bytes: u64,
}

pub struct Buffer {
    handle: BufferHandle,
    worker: Option<JoinHandle<()>>,
}

impl Buffer {
    pub fn open(config: BufferConfig) -> Result<Buffer, BufferError> {
        let BufferConfig {
            state_dir,
            device,
            max_rows,
            max_bytes,
        } = config;
        if let Err(source) = fs::create_dir_all(&state_dir) {
            return Err(BufferError::StateDir {
                path: state_dir,
                source,
            });
        }

        let connection = open_connection(&state_dir.join(DATABASE_FILE))?;
        let caps = Caps {
            max_rows,
            max_bytes,
        };
        let (commands, inbox) = mpsc::unbounded_channel();
        let worker = std::thread::Builder::new()
            .name("nikki-buffer".to_owned())
            .spawn(move || serve(connection, device, caps, inbox));
        let worker = match worker {
            Ok(worker) => worker,
            Err(source) => return Err(BufferError::Worker { source }),
        };

        Ok(Buffer {
            handle: BufferHandle { commands },
            worker: Some(worker),
        })
    }

    pub fn handle(&self) -> BufferHandle {
        self.handle.clone()
    }

    pub async fn close(mut self) -> Result<(), BufferError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let outcome = self.handle.call(|reply| Command::Close { reply }).await;
        if worker.join().is_err() {
            tracing::error!("the buffer owner thread panicked");
        }
        outcome
    }
}

#[derive(Clone)]
pub struct BufferHandle {
    commands: mpsc::UnboundedSender<Command>,
}

impl BufferHandle {
    pub async fn enqueue(
        &self,
        drafts: Vec<RecordDraft>,
        cursor: Option<Cursor>,
    ) -> Result<Vec<Envelope>, BufferError> {
        self.call(move |reply| Command::Enqueue {
            drafts,
            cursor,
            reply,
        })
        .await
    }

    pub async fn take_batch(&self, limit: usize) -> Result<Vec<PendingRecord>, BufferError> {
        self.call(move |reply| Command::TakeBatch { limit, reply })
            .await
    }

    pub async fn delete_batch(&self, ids: Vec<i64>) -> Result<(), BufferError> {
        self.call(move |reply| Command::DeleteBatch { ids, reply })
            .await
    }

    pub async fn dead_letter(
        &self,
        records: Vec<PendingRecord>,
        reason: String,
    ) -> Result<(), BufferError> {
        self.call(move |reply| Command::DeadLetter {
            records,
            reason,
            reply,
        })
        .await
    }

    pub async fn cursor(
        &self,
        provider: Provider,
        key: String,
    ) -> Result<Option<String>, BufferError> {
        self.call(move |reply| Command::ReadCursor {
            provider,
            key,
            reply,
        })
        .await
    }

    pub async fn flush_now(&self) -> Result<(), BufferError> {
        self.call(|reply| Command::FlushNow { reply }).await
    }

    async fn call<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, BufferError>>) -> Command,
    ) -> Result<T, BufferError> {
        let (reply, answer) = oneshot::channel();
        let Ok(()) = self.commands.send(build(reply)) else {
            return Err(BufferError::Closed);
        };
        let Ok(outcome) = answer.await else {
            return Err(BufferError::Closed);
        };
        outcome
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Caps {
    max_rows: u64,
    max_bytes: u64,
}

enum Command {
    Enqueue {
        drafts: Vec<RecordDraft>,
        cursor: Option<Cursor>,
        reply: oneshot::Sender<Result<Vec<Envelope>, BufferError>>,
    },
    TakeBatch {
        limit: usize,
        reply: oneshot::Sender<Result<Vec<PendingRecord>, BufferError>>,
    },
    DeleteBatch {
        ids: Vec<i64>,
        reply: oneshot::Sender<Result<(), BufferError>>,
    },
    DeadLetter {
        records: Vec<PendingRecord>,
        reason: String,
        reply: oneshot::Sender<Result<(), BufferError>>,
    },
    ReadCursor {
        provider: Provider,
        key: String,
        reply: oneshot::Sender<Result<Option<String>, BufferError>>,
    },
    FlushNow {
        reply: oneshot::Sender<Result<(), BufferError>>,
    },
    Close {
        reply: oneshot::Sender<Result<(), BufferError>>,
    },
}

fn serve(
    mut connection: Connection,
    device: String,
    caps: Caps,
    mut inbox: mpsc::UnboundedReceiver<Command>,
) {
    loop {
        let Some(command) = inbox.blocking_recv() else {
            return;
        };
        match command {
            Command::Enqueue {
                drafts,
                cursor,
                reply,
            } => {
                let _ = reply.send(enqueue(&mut connection, &device, caps, drafts, cursor));
            }
            Command::TakeBatch { limit, reply } => {
                let _ = reply.send(take_batch(&connection, limit));
            }
            Command::DeleteBatch { ids, reply } => {
                let _ = reply.send(delete_batch(&mut connection, &ids));
            }
            Command::DeadLetter {
                records,
                reason,
                reply,
            } => {
                let _ = reply.send(dead_letter(&mut connection, &records, &reason));
            }
            Command::ReadCursor {
                provider,
                key,
                reply,
            } => {
                let _ = reply.send(read_cursor(&connection, provider, &key));
            }
            Command::FlushNow { reply } => {
                let _ = reply.send(checkpoint(&connection));
            }
            Command::Close { reply } => {
                let _ = reply.send(checkpoint(&connection));
                return;
            }
        }
    }
}

fn open_connection(path: &Path) -> Result<Connection, BufferError> {
    let connection = match Connection::open(path) {
        Ok(connection) => connection,
        Err(source) => {
            return Err(BufferError::Open {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.query_row("PRAGMA journal_mode = WAL", [], |row| {
        row.get::<_, String>(0)
    })?;
    connection.execute_batch(SCHEMA)?;
    Ok(connection)
}

fn enqueue(
    connection: &mut Connection,
    device: &str,
    caps: Caps,
    drafts: Vec<RecordDraft>,
    cursor: Option<Cursor>,
) -> Result<Vec<Envelope>, BufferError> {
    let transaction = connection.transaction()?;
    let created_at = Timestamp::now().to_rfc3339();
    let mut seq = read_seq(&transaction)?;
    let mut sealed = Vec::with_capacity(drafts.len());

    for draft in drafts {
        seq += 1;
        let envelope = draft.into_envelope(device, seq);
        insert_pending(&transaction, &envelope, &created_at)?;
        sealed.push(envelope);
    }

    if let Some(Cursor {
        provider,
        key,
        value,
    }) = cursor
    {
        transaction.execute(
            "INSERT INTO cursors(provider, key, value) VALUES(?1, ?2, ?3)
             ON CONFLICT(provider, key) DO UPDATE SET value = excluded.value",
            params![provider.as_str(), key, value],
        )?;
    }

    if let Some(evicted) = evict_pending(&transaction, caps)? {
        tracing::warn!(
            dropped = evicted.dropped,
            from = %evicted.dropped_from,
            to = %evicted.dropped_to,
            "the buffer is full and the oldest records were dropped"
        );
        seq += 1;
        let envelope = overflow_draft(evicted).into_envelope(device, seq);
        insert_pending(&transaction, &envelope, &created_at)?;
        sealed.push(envelope);
    }

    write_seq(&transaction, seq)?;
    transaction.commit()?;
    Ok(sealed)
}

fn insert_pending(
    transaction: &Transaction,
    envelope: &Envelope,
    created_at: &str,
) -> Result<(), BufferError> {
    let text = serde_json::to_string(envelope)?;
    let bytes = text.len() as i64;
    transaction.execute(
        "INSERT INTO pending(envelope, bytes, created_at) VALUES(?1, ?2, ?3)",
        params![text, bytes, created_at],
    )?;
    Ok(())
}

fn read_seq(transaction: &Transaction) -> Result<u64, BufferError> {
    let stored: Option<String> = transaction
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![SEQ_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(0);
    };
    let Ok(seq) = stored.parse::<u64>() else {
        return Err(BufferError::Counter {
            key: SEQ_KEY,
            value: stored,
        });
    };
    Ok(seq)
}

fn write_seq(transaction: &Transaction, seq: u64) -> Result<(), BufferError> {
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SEQ_KEY, seq.to_string()],
    )?;
    Ok(())
}

struct Evicted {
    dropped: u64,
    dropped_from: String,
    dropped_to: String,
}

fn evict_pending(transaction: &Transaction, caps: Caps) -> Result<Option<Evicted>, BufferError> {
    let Caps {
        max_rows,
        max_bytes,
    } = caps;
    let (rows, bytes) = totals(transaction, "pending")?;
    if rows <= max_rows && bytes <= max_bytes {
        return Ok(None);
    }

    let evicted = plan_eviction(
        transaction,
        "SELECT id, bytes, created_at FROM pending ORDER BY id",
        rows,
        bytes,
        max_rows.saturating_sub(1),
        max_bytes.saturating_sub(OVERFLOW_HEADROOM_BYTES),
    )?;
    let Some((cutoff, evicted)) = evicted else {
        return Ok(None);
    };
    transaction.execute("DELETE FROM pending WHERE id <= ?1", params![cutoff])?;
    Ok(Some(evicted))
}

fn evict_dead_letter(transaction: &Transaction) -> Result<(), BufferError> {
    let (rows, bytes) = totals(transaction, "dead_letter")?;
    if rows <= DEAD_LETTER_MAX_ROWS && bytes <= DEAD_LETTER_MAX_BYTES {
        return Ok(());
    }

    let evicted = plan_eviction(
        transaction,
        "SELECT id, bytes, at FROM dead_letter ORDER BY id",
        rows,
        bytes,
        DEAD_LETTER_MAX_ROWS,
        DEAD_LETTER_MAX_BYTES,
    )?;
    let Some((cutoff, evicted)) = evicted else {
        return Ok(());
    };
    transaction.execute("DELETE FROM dead_letter WHERE id <= ?1", params![cutoff])?;
    tracing::warn!(
        dropped = evicted.dropped,
        "the dead letter table is full and its oldest records were dropped"
    );
    Ok(())
}

fn plan_eviction(
    transaction: &Transaction,
    query: &str,
    rows: u64,
    bytes: u64,
    row_target: u64,
    byte_target: u64,
) -> Result<Option<(i64, Evicted)>, BufferError> {
    let mut statement = transaction.prepare(query)?;
    let mut candidates = statement.query([])?;
    let mut rows = rows;
    let mut bytes = bytes;
    let mut cutoff = None;
    let mut dropped = 0;
    let mut dropped_from = None;
    let mut dropped_to = None;

    loop {
        if rows <= row_target && bytes <= byte_target {
            break;
        }
        let Some(candidate) = candidates.next()? else {
            break;
        };
        let id: i64 = candidate.get(0)?;
        let size: i64 = candidate.get(1)?;
        let at: String = candidate.get(2)?;
        rows -= 1;
        bytes = bytes.saturating_sub(size.max(0) as u64);
        cutoff = Some(id);
        dropped += 1;
        if dropped_from.is_none() {
            dropped_from = Some(at.clone());
        }
        dropped_to = Some(at);
    }

    let (Some(cutoff), Some(dropped_from), Some(dropped_to)) = (cutoff, dropped_from, dropped_to)
    else {
        return Ok(None);
    };
    Ok(Some((
        cutoff,
        Evicted {
            dropped,
            dropped_from,
            dropped_to,
        },
    )))
}

fn overflow_draft(evicted: Evicted) -> RecordDraft {
    let Evicted {
        dropped,
        dropped_from,
        dropped_to,
    } = evicted;
    RecordDraft {
        provider: Provider::Windows,
        kind: Kind::BufferOverflow,
        ts: Timestamp::now(),
        degraded: false,
        payload: json!({
            "details": {
                "dropped": dropped,
                "dropped_from": dropped_from,
                "dropped_to": dropped_to,
            }
        }),
        key: KeySource::Windows,
    }
}

fn totals(transaction: &Transaction, table: &str) -> Result<(u64, u64), BufferError> {
    let query = format!("SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM {table}");
    let (rows, bytes): (i64, i64) =
        transaction.query_row(&query, [], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok((rows.max(0) as u64, bytes.max(0) as u64))
}

fn take_batch(connection: &Connection, limit: usize) -> Result<Vec<PendingRecord>, BufferError> {
    let mut statement =
        connection.prepare("SELECT id, envelope, bytes FROM pending ORDER BY id LIMIT ?1")?;
    let mut found = statement.query(params![limit as i64])?;
    let mut records = Vec::new();
    loop {
        let Some(row) = found.next()? else {
            break;
        };
        let bytes: i64 = row.get(2)?;
        records.push(PendingRecord {
            id: row.get(0)?,
            envelope: row.get(1)?,
            bytes: bytes.max(0) as u64,
        });
    }
    Ok(records)
}

fn delete_batch(connection: &mut Connection, ids: &[i64]) -> Result<(), BufferError> {
    if ids.is_empty() {
        return Ok(());
    }
    let transaction = connection.transaction()?;
    {
        let mut remove = transaction.prepare("DELETE FROM pending WHERE id = ?1")?;
        for id in ids {
            remove.execute(params![id])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn dead_letter(
    connection: &mut Connection,
    records: &[PendingRecord],
    reason: &str,
) -> Result<(), BufferError> {
    if records.is_empty() {
        return Ok(());
    }
    let at = Timestamp::now().to_rfc3339();
    let transaction = connection.transaction()?;
    {
        let mut store = transaction.prepare(
            "INSERT INTO dead_letter(envelope, bytes, reason, at) VALUES(?1, ?2, ?3, ?4)",
        )?;
        let mut remove = transaction.prepare("DELETE FROM pending WHERE id = ?1")?;
        for PendingRecord {
            id,
            envelope,
            bytes,
        } in records
        {
            store.execute(params![envelope, *bytes as i64, reason, at])?;
            remove.execute(params![id])?;
        }
    }
    evict_dead_letter(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn read_cursor(
    connection: &Connection,
    provider: Provider,
    key: &str,
) -> Result<Option<String>, BufferError> {
    let value = connection
        .query_row(
            "SELECT value FROM cursors WHERE provider = ?1 AND key = ?2",
            params![provider.as_str(), key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value)
}

fn checkpoint(connection: &Connection) -> Result<(), BufferError> {
    connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    struct TempState {
        path: PathBuf,
    }

    impl TempState {
        fn new(name: &str) -> TempState {
            let path = std::env::temp_dir().join(format!("nikki-buffer-test-{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("the temporary state directory could not be created");
            TempState { path }
        }

        fn reader(&self) -> Connection {
            let connection = Connection::open(self.path.join(DATABASE_FILE))
                .expect("the buffer database could not be opened for reading");
            connection
                .busy_timeout(BUSY_TIMEOUT)
                .expect("the busy timeout could not be set");
            connection
        }

        fn totals(&self, table: &str) -> (u64, u64) {
            let connection = self.reader();
            let query = format!("SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM {table}");
            let (rows, bytes): (i64, i64) = connection
                .query_row(&query, [], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("the table could not be counted");
            (rows.max(0) as u64, bytes.max(0) as u64)
        }
    }

    impl Drop for TempState {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn config(state: &TempState, max_rows: u64, max_bytes: u64) -> BufferConfig {
        BufferConfig {
            state_dir: state.path.clone(),
            device: "mbp-21".to_string(),
            max_rows,
            max_bytes,
        }
    }

    fn open(state: &TempState, max_rows: u64, max_bytes: u64) -> Buffer {
        Buffer::open(config(state, max_rows, max_bytes)).expect("the buffer could not be opened")
    }

    fn tick(millis: i64) -> RecordDraft {
        RecordDraft {
            provider: Provider::Windows,
            kind: Kind::Tick,
            ts: Timestamp::from_millis(millis),
            degraded: false,
            payload: json!({"app": "Zed", "tick_interval_sec": 30}),
            key: KeySource::Windows,
        }
    }

    fn visit(visit_id: i64) -> RecordDraft {
        RecordDraft {
            provider: Provider::BrowserHistory,
            kind: Kind::Visit,
            ts: Timestamp::from_millis(1_787_666_156_000),
            degraded: false,
            payload: json!({"url": "https://example.com/", "visit_id": visit_id}),
            key: KeySource::BrowserVisit {
                profile: "MBP_21".to_string(),
                generation: 1,
                visit_id,
            },
        }
    }

    fn browser_cursor(value: &str) -> Cursor {
        Cursor {
            provider: Provider::BrowserHistory,
            key: "MBP_21".to_string(),
            value: value.to_string(),
        }
    }

    #[tokio::test]
    async fn enqueue_seals_every_draft_and_advances_the_cursor_together() {
        let state = TempState::new("enqueue-round-trip");
        let buffer = open(&state, 1_000, 1_000_000);
        let handle = buffer.handle();

        let sealed = handle
            .enqueue(
                vec![tick(1_787_666_152_481), visit(929_269)],
                Some(browser_cursor("929269")),
            )
            .await
            .expect("the batch could not be enqueued");

        assert_eq!(sealed.len(), 2);
        assert_eq!(sealed[0].seq, 1);
        assert_eq!(sealed[1].seq, 2);
        assert_eq!(sealed[0].dedup_key.len(), 16);
        assert_ne!(sealed[0].dedup_key, sealed[1].dedup_key);
        assert_eq!(
            handle
                .cursor(Provider::BrowserHistory, "MBP_21".to_string())
                .await
                .expect("the cursor could not be read"),
            Some("929269".to_string())
        );
        assert_eq!(state.totals("pending").0, 2);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_refused_envelope_insert_leaves_the_cursor_and_the_counter_untouched() {
        let state = TempState::new("enqueue-atomicity");
        let buffer = open(&state, 1_000, 1_000_000);
        let handle = buffer.handle();

        let guard = state.reader();
        guard
            .execute_batch(
                "CREATE TRIGGER refuse BEFORE INSERT ON pending
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
            .expect("the trigger could not be installed");

        handle
            .enqueue(
                vec![tick(1_787_666_152_481)],
                Some(browser_cursor("929269")),
            )
            .await
            .expect_err("the refused insert must fail the enqueue");

        assert_eq!(state.totals("pending").0, 0);
        assert_eq!(
            handle
                .cursor(Provider::BrowserHistory, "MBP_21".to_string())
                .await
                .expect("the cursor could not be read"),
            None
        );

        guard
            .execute_batch("DROP TRIGGER refuse")
            .expect("the trigger could not be dropped");
        let sealed = handle
            .enqueue(vec![tick(1_787_666_152_481)], None)
            .await
            .expect("the retry could not be enqueued");
        assert_eq!(sealed[0].seq, 1);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn seq_survives_a_restart_and_never_repeats() {
        let state = TempState::new("seq-restart");

        let buffer = open(&state, 1_000, 1_000_000);
        let sealed = buffer
            .handle()
            .enqueue(vec![tick(1), tick(2), tick(3)], None)
            .await
            .expect("the first batch could not be enqueued");
        assert_eq!(sealed[2].seq, 3);
        buffer.close().await.expect("the buffer did not close");

        let buffer = open(&state, 1_000, 1_000_000);
        let sealed = buffer
            .handle()
            .enqueue(vec![tick(4)], None)
            .await
            .expect("the second batch could not be enqueued");
        assert_eq!(sealed[0].seq, 4);
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn two_records_of_one_kind_in_the_same_millisecond_get_different_keys() {
        let state = TempState::new("same-millisecond");
        let buffer = open(&state, 1_000, 1_000_000);

        let sealed = buffer
            .handle()
            .enqueue(vec![tick(1_787_666_152_481), tick(1_787_666_152_481)], None)
            .await
            .expect("the batch could not be enqueued");

        assert_eq!(sealed[0].ts, sealed[1].ts);
        assert_eq!(sealed[0].kind, sealed[1].kind);
        assert_ne!(sealed[0].dedup_key, sealed[1].dedup_key);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn overflow_evicts_until_both_limits_hold_and_emits_one_marker() {
        let state = TempState::new("overflow-both-limits");
        let buffer = open(&state, 10, 1_000_000);
        let handle = buffer.handle();

        for index in 0..12 {
            handle
                .enqueue(vec![tick(index)], None)
                .await
                .expect("a record could not be enqueued");
        }

        let (rows, bytes) = state.totals("pending");
        assert!(rows <= 10, "the row cap is still exceeded at {rows}");
        assert!(bytes <= 1_000_000, "the byte cap is still exceeded");

        let markers = markers(&state);
        assert_eq!(markers.len(), 2, "one marker per overflowing enqueue");

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn the_byte_cap_alone_forces_eviction() {
        let state = TempState::new("overflow-byte-cap");
        let buffer = open(&state, 1_000_000, 2_000);
        let handle = buffer.handle();

        for index in 0..30 {
            handle
                .enqueue(vec![tick(index)], None)
                .await
                .expect("a record could not be enqueued");
        }

        let (_, bytes) = state.totals("pending");
        assert!(bytes <= 2_000, "the byte cap is still exceeded at {bytes}");
        assert!(
            !markers(&state).is_empty(),
            "no overflow marker was emitted"
        );

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn the_overflow_marker_carries_a_real_seq_a_real_key_and_the_pinned_payload() {
        let state = TempState::new("overflow-marker");
        let buffer = open(&state, 3, 1_000_000);
        let handle = buffer.handle();

        handle
            .enqueue(vec![tick(1), tick(2), tick(3)], None)
            .await
            .expect("the first batch could not be enqueued");
        let sealed = handle
            .enqueue(vec![tick(4)], None)
            .await
            .expect("the overflowing batch could not be enqueued");

        let marker = sealed.last().expect("the batch is empty");
        assert_eq!(marker.kind, Kind::BufferOverflow);
        assert_eq!(marker.provider, Provider::Windows);
        assert!(marker.seq > 0, "the marker carries no sequence number");
        assert_eq!(marker.dedup_key.len(), 16);
        assert!(!marker.degraded);

        let details = &marker.payload["details"];
        assert_eq!(details["dropped"], 2);
        assert!(details["dropped_from"].is_string());
        assert!(details["dropped_to"].is_string());

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn the_overflow_marker_fits_the_reserved_headroom() {
        let state = TempState::new("overflow-headroom");
        let buffer = open(&state, 2, 1_000_000);
        let handle = buffer.handle();

        handle
            .enqueue(vec![tick(1), tick(2), tick(3)], None)
            .await
            .expect("the overflowing batch could not be enqueued");

        let markers = markers(&state);
        let (bytes, _) = markers.first().expect("no overflow marker was emitted");
        let bytes = *bytes;
        assert!(
            bytes <= OVERFLOW_HEADROOM_BYTES,
            "the marker is {bytes} bytes, past the {OVERFLOW_HEADROOM_BYTES} reserved"
        );

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn take_and_delete_round_trip_in_order() {
        let state = TempState::new("take-delete");
        let buffer = open(&state, 1_000, 1_000_000);
        let handle = buffer.handle();

        handle
            .enqueue(vec![tick(1), tick(2), tick(3), tick(4)], None)
            .await
            .expect("the batch could not be enqueued");

        let batch = handle
            .take_batch(2)
            .await
            .expect("the batch could not be taken");
        assert_eq!(batch.len(), 2);
        assert!(batch[0].id < batch[1].id);
        assert!(batch[0].bytes > 0);
        assert_eq!(state.totals("pending").0, 4, "taking must not delete");

        let mut ids = Vec::new();
        for PendingRecord { id, .. } in &batch {
            ids.push(*id);
        }
        handle
            .delete_batch(ids)
            .await
            .expect("the batch could not be deleted");
        assert_eq!(state.totals("pending").0, 2);

        let rest = handle
            .take_batch(500)
            .await
            .expect("the rest could not be taken");
        assert_eq!(rest.len(), 2);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn dead_letter_fills_its_own_cap_without_evicting_a_pending_row() {
        let state = TempState::new("dead-letter-cap");
        let buffer = open(&state, 1_000_000, 1_000_000_000);
        let handle = buffer.handle();

        let surplus = 4;
        let mut drafts = Vec::new();
        for index in 0..DEAD_LETTER_MAX_ROWS + 1 + surplus {
            drafts.push(tick(index as i64));
        }
        handle
            .enqueue(drafts, None)
            .await
            .expect("the batch could not be enqueued");

        let doomed = handle
            .take_batch((DEAD_LETTER_MAX_ROWS + 1) as usize)
            .await
            .expect("the batch could not be taken");
        assert_eq!(doomed.len(), (DEAD_LETTER_MAX_ROWS + 1) as usize);
        handle
            .dead_letter(doomed, "400 malformed batch".to_string())
            .await
            .expect("the batch could not be dead lettered");

        assert_eq!(state.totals("dead_letter").0, DEAD_LETTER_MAX_ROWS);
        assert_eq!(state.totals("pending").0, surplus);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn flush_now_returns_with_every_earlier_record_on_disk() {
        let state = TempState::new("flush-now");
        let buffer = open(&state, 1_000, 1_000_000);
        let handle = buffer.handle();

        handle
            .enqueue(vec![tick(1), tick(2)], None)
            .await
            .expect("the batch could not be enqueued");
        handle.flush_now().await.expect("the flush failed");

        assert_eq!(state.totals("pending").0, 2);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_closed_buffer_reports_itself_rather_than_hanging() {
        let state = TempState::new("closed");
        let buffer = open(&state, 1_000, 1_000_000);
        let handle = buffer.handle();
        buffer.close().await.expect("the buffer did not close");

        let error = handle
            .enqueue(vec![tick(1)], None)
            .await
            .expect_err("a closed buffer must refuse work");
        match error {
            BufferError::Closed => {}
            other => panic!("expected a closed buffer, got {other}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn both_providers_enqueue_while_the_shipper_drains() {
        let state = TempState::new("concurrency");
        let buffer = open(&state, 1_000_000, 1_000_000_000);

        let per_provider = 100;
        let mut providers = Vec::new();
        for provider in [Provider::Windows, Provider::BrowserHistory] {
            let handle = buffer.handle();
            providers.push(tokio::spawn(async move {
                for index in 0..per_provider {
                    let draft = match provider {
                        Provider::Windows => tick(index),
                        Provider::BrowserHistory => visit(index),
                    };
                    handle
                        .enqueue(vec![draft], None)
                        .await
                        .expect("a provider could not enqueue");
                }
            }));
        }

        let handle = buffer.handle();
        let shipper = tokio::spawn(async move {
            let mut shipped = 0;
            while shipped < per_provider * 2 {
                let batch = handle
                    .take_batch(37)
                    .await
                    .expect("the shipper could not take a batch");
                if batch.is_empty() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue;
                }
                let mut ids = Vec::new();
                for PendingRecord { id, .. } in &batch {
                    ids.push(*id);
                }
                shipped += ids.len() as i64;
                handle
                    .delete_batch(ids)
                    .await
                    .expect("the shipper could not delete a batch");
            }
            shipped
        });

        for provider in providers {
            provider.await.expect("a provider task panicked");
        }
        let shipped = shipper.await.expect("the shipper task panicked");

        assert_eq!(shipped, per_provider * 2);
        assert_eq!(state.totals("pending").0, 0);

        buffer.close().await.expect("the buffer did not close");
    }

    fn markers(state: &TempState) -> Vec<(u64, Value)> {
        let connection = state.reader();
        let mut statement = connection
            .prepare("SELECT bytes, envelope FROM pending ORDER BY id")
            .expect("the pending table could not be read");
        let mut found = statement
            .query([])
            .expect("the pending table could not be queried");
        let mut markers = Vec::new();
        while let Some(row) = found.next().expect("a pending row could not be read") {
            let bytes: i64 = row.get(0).expect("a pending size is missing");
            let envelope: String = row.get(1).expect("a pending envelope is missing");
            let envelope: Value =
                serde_json::from_str(&envelope).expect("a pending envelope does not parse");
            if envelope["kind"] == "buffer_overflow" {
                markers.push((bytes.max(0) as u64, envelope));
            }
        }
        markers
    }
}
