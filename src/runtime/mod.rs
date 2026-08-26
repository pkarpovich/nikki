pub mod buffer;
pub mod dedup;
pub mod redact;
pub mod ship;

use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::providers::Emission;
use crate::runtime::buffer::{Buffer, BufferConfig, BufferError, BufferHandle};
use crate::runtime::dedup::{browser_key, windows_key};
use crate::runtime::ship::{HttpTransport, ShipError, Shipper};

const RFC3339_MILLIS: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";
const UNREPRESENTABLE_INSTANT: &str = "1970-01-01T00:00:00.000Z";

pub const EMISSION_QUEUE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Windows,
    BrowserHistory,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Windows => "windows",
            Provider::BrowserHistory => "browser_history",
        }
    }
}

impl Serialize for Provider {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Tick,
    Focus,
    StateChange,
    Lock,
    Unlock,
    Sleep,
    Wake,
    BufferOverflow,
    Visit,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Tick => "tick",
            Kind::Focus => "focus",
            Kind::StateChange => "state_change",
            Kind::Lock => "lock",
            Kind::Unlock => "unlock",
            Kind::Sleep => "sleep",
            Kind::Wake => "wake",
            Kind::BufferOverflow => "buffer_overflow",
            Kind::Visit => "visit",
        }
    }
}

impl Serialize for Kind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(i64);

impl Timestamp {
    pub fn now() -> Timestamp {
        Timestamp(Utc::now().timestamp_millis())
    }

    pub fn from_millis(millis: i64) -> Timestamp {
        Timestamp(millis)
    }

    pub fn millis(self) -> i64 {
        self.0
    }

    pub fn to_rfc3339(self) -> String {
        let Some(moment) = DateTime::<Utc>::from_timestamp_millis(self.0) else {
            return UNREPRESENTABLE_INSTANT.to_string();
        };
        moment.format(RFC3339_MILLIS).to_string()
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_rfc3339())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub provider: Provider,
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    Windows,
    BrowserVisit {
        profile: String,
        generation: u64,
        visit_id: i64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordDraft {
    pub provider: Provider,
    pub kind: Kind,
    pub ts: Timestamp,
    pub degraded: bool,
    pub payload: Value,
    pub key: KeySource,
}

impl RecordDraft {
    pub fn into_envelope(self, device: &str, seq: u64) -> Envelope {
        let RecordDraft {
            provider,
            kind,
            ts,
            degraded,
            payload,
            key,
        } = self;
        let dedup_key = match key {
            KeySource::Windows => windows_key(device, kind, ts.millis(), seq),
            KeySource::BrowserVisit {
                profile,
                generation,
                visit_id,
            } => browser_key(device, &profile, generation, visit_id),
        };
        Envelope {
            provider,
            device: device.to_string(),
            ts,
            seq,
            kind,
            dedup_key,
            degraded,
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Envelope {
    pub provider: Provider,
    pub device: String,
    pub ts: Timestamp,
    pub seq: u64,
    pub kind: Kind,
    pub dedup_key: String,
    pub degraded: bool,
    pub payload: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Buffer(#[from] BufferError),
    #[error(transparent)]
    Ship(#[from] ShipError),
}

pub struct Pipeline {
    buffer: Buffer,
    shipper: Shipper<HttpTransport>,
}

impl Pipeline {
    pub fn open(config: &Config) -> Result<Pipeline, RuntimeError> {
        let Config {
            service_url,
            device,
            buffer,
            redact,
            state_dir,
            ..
        } = config;
        let buffer = Buffer::open(BufferConfig {
            state_dir: state_dir.clone(),
            device: device.clone(),
            max_rows: buffer.max_rows,
            max_bytes: buffer.max_bytes,
            redact: redact.clone(),
        })?;
        let shipper = Shipper::new(buffer.handle(), HttpTransport::new(service_url)?);
        Ok(Pipeline { buffer, shipper })
    }

    pub fn records(&self) -> BufferHandle {
        self.buffer.handle()
    }

    pub fn shipper(&mut self) -> &mut Shipper<HttpTransport> {
        &mut self.shipper
    }

    pub async fn close(self) -> Result<(), RuntimeError> {
        self.buffer.close().await?;
        Ok(())
    }
}

pub async fn absorb(
    records: BufferHandle,
    mut emissions: mpsc::Receiver<Emission>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            emission = emissions.recv() => {
                let Some(emission) = emission else {
                    return;
                };
                store(&records, emission).await;
            }
            _ = shutdown.changed() => break,
        }
    }

    emissions.close();
    loop {
        let Some(emission) = emissions.recv().await else {
            return;
        };
        store(&records, emission).await;
    }
}

async fn store(records: &BufferHandle, emission: Emission) {
    let Emission {
        records: drafts,
        cursor,
        committed,
    } = emission;
    let count = drafts.len();
    match records.enqueue(drafts, cursor).await {
        Ok(envelopes) => tracing::debug!(count = envelopes.len(), "records were buffered"),
        Err(error) => tracing::error!(%error, count, "records could not be buffered"),
    }

    let Some(committed) = committed else {
        return;
    };
    if let Err(error) = records.flush_now().await {
        tracing::error!(%error, "the buffer could not be flushed before the acknowledgement");
    }
    let _ = committed.send(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc::Sender;

    use crate::config::{Keep, RedactRule};
    use crate::providers::tests::test_ctx;
    use crate::providers::{Backoff, Ctx, Provider as Supervised, ProviderError, supervise};
    use crate::runtime::redact::WILDCARD_HOST;

    fn tick_draft(ts: Timestamp) -> RecordDraft {
        RecordDraft {
            provider: Provider::Windows,
            kind: Kind::Tick,
            ts,
            degraded: false,
            payload: json!({"app": "Zed"}),
            key: KeySource::Windows,
        }
    }

    fn visit_draft(visit_id: i64, generation: u64) -> RecordDraft {
        RecordDraft {
            provider: Provider::BrowserHistory,
            kind: Kind::Visit,
            ts: Timestamp::from_millis(1_756_130_156_000),
            degraded: false,
            payload: json!({"url": "https://example.com/"}),
            key: KeySource::BrowserVisit {
                profile: "MBP_21".to_string(),
                generation,
                visit_id,
            },
        }
    }

    const TICK_MILLIS: i64 = 1_787_666_152_481;
    const TICK_RFC3339: &str = "2026-08-25T13:55:52.481Z";

    #[test]
    fn every_provider_and_kind_carries_its_wire_name() {
        assert_eq!(Provider::Windows.as_str(), "windows");
        assert_eq!(Provider::BrowserHistory.as_str(), "browser_history");

        let kinds = [
            (Kind::Tick, "tick"),
            (Kind::Focus, "focus"),
            (Kind::StateChange, "state_change"),
            (Kind::Lock, "lock"),
            (Kind::Unlock, "unlock"),
            (Kind::Sleep, "sleep"),
            (Kind::Wake, "wake"),
            (Kind::BufferOverflow, "buffer_overflow"),
            (Kind::Visit, "visit"),
        ];
        for (kind, name) in kinds {
            assert_eq!(kind.as_str(), name);
        }
    }

    #[test]
    fn a_timestamp_formats_as_utc_with_millisecond_precision() {
        let ts = Timestamp::from_millis(TICK_MILLIS);
        assert_eq!(ts.to_rfc3339(), TICK_RFC3339);
    }

    #[test]
    fn a_whole_second_still_carries_three_fractional_digits() {
        let ts = Timestamp::from_millis(0);
        assert_eq!(ts.to_rfc3339(), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn an_unrepresentable_instant_falls_back_to_the_epoch() {
        assert_eq!(
            Timestamp::from_millis(i64::MAX).to_rfc3339(),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn an_envelope_serialises_to_the_wire_shape() {
        let envelope =
            tick_draft(Timestamp::from_millis(TICK_MILLIS)).into_envelope("mbp-21", 41_207);
        let text = serde_json::to_string(&envelope).expect("the envelope serialises");
        let value: Value = serde_json::from_str(&text).expect("the envelope parses back");

        assert_eq!(value["provider"], "windows");
        assert_eq!(value["device"], "mbp-21");
        assert_eq!(value["ts"], TICK_RFC3339);
        assert_eq!(value["seq"], 41_207);
        assert_eq!(value["kind"], "tick");
        assert_eq!(value["degraded"], false);
        assert_eq!(value["payload"]["app"], "Zed");
        assert_eq!(
            value["dedup_key"].as_str().expect("a key is present").len(),
            16
        );
    }

    #[test]
    fn the_windows_key_follows_the_sequence_number() {
        let ts = Timestamp::from_millis(TICK_MILLIS);
        let first = tick_draft(ts).into_envelope("mbp-21", 1);
        let second = tick_draft(ts).into_envelope("mbp-21", 2);
        assert_ne!(first.dedup_key, second.dedup_key);
    }

    #[test]
    fn the_browser_key_ignores_the_sequence_number_and_follows_the_generation() {
        let first = visit_draft(929_269, 1).into_envelope("mbp-21", 1);
        let second = visit_draft(929_269, 1).into_envelope("mbp-21", 9_999);
        assert_eq!(first.dedup_key, second.dedup_key);

        let regenerated = visit_draft(929_269, 2).into_envelope("mbp-21", 1);
        assert_ne!(first.dedup_key, regenerated.dedup_key);
    }

    struct TempState {
        path: PathBuf,
    }

    impl TempState {
        fn new(name: &str) -> TempState {
            let path = std::env::temp_dir().join(format!("nikki-runtime-test-{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("the temporary state directory could not be created");
            TempState { path }
        }
    }

    impl Drop for TempState {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn open_buffer(state: &TempState) -> Buffer {
        Buffer::open(BufferConfig {
            state_dir: state.path.clone(),
            device: "mbp-21".to_string(),
            max_rows: 10_000,
            max_bytes: 10_000_000,
            redact: vec![RedactRule {
                url_host: Some(WILDCARD_HOST.to_string()),
                keep: Some(Keep::Host),
                bundle_id: None,
                drop: Vec::new(),
            }],
        })
        .expect("the buffer opens")
    }

    struct Steady {
        emitted: Arc<AtomicUsize>,
    }

    impl Supervised for Steady {
        fn name(&self) -> &'static str {
            "steady"
        }

        async fn run(&mut self, _ctx: Ctx, out: Sender<Emission>) -> Result<(), ProviderError> {
            loop {
                let draft = tick_draft(Timestamp::now());
                if out.send(Emission::new(vec![draft])).await.is_err() {
                    return Ok(());
                }
                self.emitted.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }

    struct Exploding {
        attempts: Arc<AtomicUsize>,
    }

    impl Supervised for Exploding {
        fn name(&self) -> &'static str {
            "exploding"
        }

        async fn run(&mut self, _ctx: Ctx, _out: Sender<Emission>) -> Result<(), ProviderError> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                panic!("the provider fell over");
            }
            loop {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }

    fn quick_backoff() -> Backoff {
        Backoff {
            start: Duration::from_millis(1),
            ceiling: Duration::from_millis(4),
        }
    }

    async fn buffered(records: &BufferHandle) -> usize {
        records
            .take_batch(1_000)
            .await
            .expect("the buffer answers")
            .len()
    }

    async fn wait_for(what: &str, mut ready: impl AsyncFnMut() -> bool) {
        for _ in 0..2_000 {
            if ready().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("{what} did not happen in time");
    }

    #[tokio::test]
    async fn a_panicking_provider_restarts_while_the_other_keeps_emitting() {
        let state = TempState::new("supervision");
        let buffer = open_buffer(&state);
        let records = buffer.handle();

        let (emissions, drafts) = mpsc::channel(EMISSION_QUEUE);
        let (shutdown, listener) = watch::channel(false);
        let absorbing = tokio::spawn(absorb(records.clone(), drafts, listener));

        let attempts = Arc::new(AtomicUsize::new(0));
        let emitted = Arc::new(AtomicUsize::new(0));
        let exploding = tokio::spawn(supervise(
            Exploding {
                attempts: Arc::clone(&attempts),
            },
            test_ctx(30),
            emissions.clone(),
            quick_backoff(),
        ));
        let steady = tokio::spawn(supervise(
            Steady {
                emitted: Arc::clone(&emitted),
            },
            test_ctx(30),
            emissions.clone(),
            quick_backoff(),
        ));
        drop(emissions);

        wait_for("the panicking provider restarted", async || {
            attempts.load(Ordering::SeqCst) >= 2
        })
        .await;

        let restarted_at = buffered(&records).await;
        wait_for("the steady provider kept emitting", async || {
            buffered(&records).await >= restarted_at + 2
        })
        .await;

        assert!(!exploding.is_finished());
        exploding.abort();
        steady.abort();
        let _ = exploding.await;
        let _ = steady.await;

        let _ = shutdown.send(true);
        absorbing.await.expect("the record writer finishes");
        buffer.close().await.expect("the buffer closes");
    }

    #[tokio::test]
    async fn an_emission_awaiting_a_commit_is_acknowledged_once_it_is_buffered() {
        let state = TempState::new("commit");
        let buffer = open_buffer(&state);
        let records = buffer.handle();

        let (emissions, drafts) = mpsc::channel(EMISSION_QUEUE);
        let (shutdown, listener) = watch::channel(false);
        let absorbing = tokio::spawn(absorb(records.clone(), drafts, listener));

        let (emission, committed) =
            Emission::awaiting_commit(vec![tick_draft(Timestamp::from_millis(TICK_MILLIS))]);
        emissions.send(emission).await.expect("the writer listens");

        committed.await.expect("the record was acknowledged");
        assert_eq!(buffered(&records).await, 1);

        drop(emissions);
        let _ = shutdown.send(true);
        absorbing.await.expect("the record writer finishes");
        buffer.close().await.expect("the buffer closes");
    }
}
