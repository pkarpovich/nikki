use std::future::Future;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::watch;
use url::Url;

use super::buffer::{BufferError, BufferHandle, PendingRecord};

pub const RECORDS_PATH: &str = "/api/v1/records";
pub const MAX_BATCH: usize = 500;
pub const MIN_BATCH: usize = 10;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const BACKOFF_START: Duration = Duration::from_secs(1);
pub const BACKOFF_CEILING: Duration = Duration::from_secs(300);
pub const IDLE_POLL: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum ShipError {
    #[error("the records endpoint could not be built from `{service_url}`: {reason}")]
    Endpoint { service_url: String, reason: String },
    #[error("the http client could not be built: {0}")]
    Client(String),
    #[error(transparent)]
    Buffer(#[from] BufferError),
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TransportError(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

pub trait Transport {
    fn post(
        &self,
        body: String,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Rejection {
    pub index: usize,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
struct ServiceResponse {
    accepted: usize,
    duplicates: usize,
    rejected: Option<Vec<Rejection>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    Delete { rejections: Vec<Rejection> },
    Keep { reason: String },
    DeadLetter { reason: String },
    Shrink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Idle,
    Shipped { count: usize },
    Kept { reason: String },
    DeadLettered { count: usize },
    Shrunk { batch_size: usize },
}

pub fn decide(status: u16, body: &str, batch_len: usize) -> Disposition {
    if status == 200 {
        return decide_accepted(body, batch_len);
    }
    if status == 401 || status == 403 || status == 404 || status == 405 {
        return Disposition::Keep {
            reason: format!("{status} is a configuration failure, not a bad batch"),
        };
    }
    if status == 413 {
        return Disposition::Shrink;
    }
    if (400..500).contains(&status) {
        return Disposition::DeadLetter {
            reason: format!("{status} rejects the batch permanently"),
        };
    }
    Disposition::Keep {
        reason: format!("{status} is retryable"),
    }
}

fn decide_accepted(body: &str, batch_len: usize) -> Disposition {
    let Ok(response) = serde_json::from_str::<ServiceResponse>(body) else {
        return Disposition::Keep {
            reason: "a 200 whose body is not the documented shape is treated as a 5xx".to_string(),
        };
    };
    let ServiceResponse {
        accepted,
        duplicates,
        rejected,
    } = response;
    let rejections = rejected.unwrap_or_default();
    let counted = accepted
        .saturating_add(duplicates)
        .saturating_add(rejections.len());
    if counted != batch_len {
        return Disposition::Keep {
            reason: format!(
                "a 200 accounting for {counted} of {batch_len} records is treated as a 5xx"
            ),
        };
    }
    Disposition::Delete { rejections }
}

pub fn endpoint(service_url: &Url) -> Result<Url, ShipError> {
    let service_url = service_url.as_str().trim_end_matches('/');
    let Ok(endpoint) = Url::parse(&format!("{service_url}{RECORDS_PATH}")) else {
        return Err(ShipError::Endpoint {
            service_url: service_url.to_string(),
            reason: format!("appending `{RECORDS_PATH}` does not produce a url"),
        });
    };
    Ok(endpoint)
}

pub fn body_for(batch: &[PendingRecord]) -> String {
    let mut body = String::from("{\"records\":[");
    let mut first = true;
    for PendingRecord { envelope, .. } in batch {
        if !first {
            body.push(',');
        }
        first = false;
        body.push_str(envelope);
    }
    body.push_str("]}");
    body
}

pub struct Shipper<T> {
    buffer: BufferHandle,
    transport: T,
    batch_size: usize,
}

impl<T: Transport> Shipper<T> {
    pub fn new(buffer: BufferHandle, transport: T) -> Shipper<T> {
        Shipper {
            buffer,
            transport,
            batch_size: MAX_BATCH,
        }
    }

    #[cfg(test)]
    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    pub async fn ship_once(&mut self) -> Result<Progress, BufferError> {
        let batch = self.buffer.take_batch(self.batch_size).await?;
        if batch.is_empty() {
            return Ok(Progress::Idle);
        }

        let disposition = match self.transport.post(body_for(&batch)).await {
            Ok(HttpResponse { status, body }) => decide(status, &body, batch.len()),
            Err(TransportError(reason)) => Disposition::Keep { reason },
        };

        match disposition {
            Disposition::Delete { rejections } => {
                for Rejection { index, reason } in &rejections {
                    tracing::warn!(index, %reason, "the service rejected a record permanently");
                }
                let mut ids = Vec::with_capacity(batch.len());
                for PendingRecord { id, .. } in &batch {
                    ids.push(*id);
                }
                let count = ids.len();
                self.buffer.delete_batch(ids).await?;
                Ok(Progress::Shipped { count })
            }
            Disposition::Keep { reason } => {
                tracing::error!(%reason, count = batch.len(), "a batch was kept for retry");
                Ok(Progress::Kept { reason })
            }
            Disposition::DeadLetter { reason } => {
                let count = batch.len();
                tracing::error!(%reason, count, "a batch was moved to the dead letter table");
                self.buffer.dead_letter(batch, reason).await?;
                Ok(Progress::DeadLettered { count })
            }
            Disposition::Shrink => self.shrink(batch).await,
        }
    }

    async fn shrink(&mut self, batch: Vec<PendingRecord>) -> Result<Progress, BufferError> {
        if batch.len() <= MIN_BATCH {
            let count = batch.len();
            let reason = format!("413 at the floor of {MIN_BATCH} records");
            tracing::error!(count, "a batch too large at the floor was dead lettered");
            self.buffer.dead_letter(batch, reason).await?;
            return Ok(Progress::DeadLettered { count });
        }
        self.batch_size = (batch.len() / 2).max(MIN_BATCH);
        tracing::warn!(
            batch_size = self.batch_size,
            "413 halved the batch size for the retry"
        );
        Ok(Progress::Shrunk {
            batch_size: self.batch_size,
        })
    }

    pub async fn run(&mut self, mut shutdown: watch::Receiver<bool>) {
        let mut backoff = BACKOFF_START;
        loop {
            if *shutdown.borrow() {
                return;
            }
            let progress = match self.ship_once().await {
                Ok(progress) => progress,
                Err(error) => {
                    tracing::error!(%error, "the shipper lost the buffer");
                    return;
                }
            };
            let pause = match progress {
                Progress::Idle => IDLE_POLL,
                Progress::Kept { .. } => {
                    let pause = backoff;
                    backoff = (backoff * 2).min(BACKOFF_CEILING);
                    pause
                }
                Progress::Shipped { .. }
                | Progress::DeadLettered { .. }
                | Progress::Shrunk { .. } => {
                    backoff = BACKOFF_START;
                    Duration::ZERO
                }
            };
            if pause.is_zero() {
                continue;
            }
            tokio::select! {
                _ = tokio::time::sleep(pause) => {}
                _ = shutdown.changed() => {}
            }
        }
    }
}

pub struct HttpTransport {
    client: reqwest::Client,
    endpoint: Url,
}

impl HttpTransport {
    pub fn new(service_url: &Url) -> Result<HttpTransport, ShipError> {
        let endpoint = endpoint(service_url)?;
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build();
        let Ok(client) = client else {
            return Err(ShipError::Client(
                "the platform tls stack is unavailable".to_string(),
            ));
        };
        Ok(HttpTransport { client, endpoint })
    }
}

impl Transport for HttpTransport {
    async fn post(&self, body: String) -> Result<HttpResponse, TransportError> {
        let response = self
            .client
            .post(self.endpoint.clone())
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => return Err(TransportError(error.to_string())),
        };
        let status = response.status().as_u16();
        let body = match response.text().await {
            Ok(body) => body,
            Err(error) => return Err(TransportError(error.to_string())),
        };
        Ok(HttpResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Keep, RedactRule};
    use crate::runtime::buffer::{Buffer, BufferConfig, DATABASE_FILE};
    use crate::runtime::redact::WILDCARD_HOST;
    use crate::runtime::{KeySource, Kind, Provider, RecordDraft, Timestamp};
    use rusqlite::Connection;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct Scripted {
        answers: Mutex<Vec<Result<HttpResponse, TransportError>>>,
        bodies: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(answers: Vec<Result<HttpResponse, TransportError>>) -> Scripted {
            let mut answers = answers;
            answers.reverse();
            Scripted {
                answers: Mutex::new(answers),
                bodies: Mutex::new(Vec::new()),
            }
        }

        fn ok(status: u16, body: &str) -> Scripted {
            Scripted::new(vec![Ok(HttpResponse {
                status,
                body: body.to_string(),
            })])
        }

        fn bodies(&self) -> Vec<String> {
            self.bodies
                .lock()
                .expect("the body log is poisoned")
                .clone()
        }
    }

    impl Transport for Scripted {
        async fn post(&self, body: String) -> Result<HttpResponse, TransportError> {
            self.bodies
                .lock()
                .expect("the body log is poisoned")
                .push(body);
            let answer = self
                .answers
                .lock()
                .expect("the answer script is poisoned")
                .pop();
            let Some(answer) = answer else {
                return Err(TransportError("the script ran out of answers".to_string()));
            };
            answer
        }
    }

    struct TempState {
        path: PathBuf,
    }

    impl TempState {
        fn new(name: &str) -> TempState {
            let path = std::env::temp_dir().join(format!("nikki-ship-test-{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("the temporary state directory could not be created");
            TempState { path }
        }

        fn count(&self, table: &str) -> u64 {
            let connection = Connection::open(self.path.join(DATABASE_FILE))
                .expect("the buffer database could not be opened for reading");
            let query = format!("SELECT COUNT(*) FROM {table}");
            let rows: i64 = connection
                .query_row(&query, [], |row| row.get(0))
                .expect("the table could not be counted");
            rows.max(0) as u64
        }

        fn envelopes(&self) -> Vec<String> {
            let connection = Connection::open(self.path.join(DATABASE_FILE))
                .expect("the buffer database could not be opened for reading");
            let mut statement = connection
                .prepare("SELECT envelope FROM pending ORDER BY id")
                .expect("the pending table could not be read");
            let mut found = statement
                .query([])
                .expect("the pending table could not be queried");
            let mut envelopes = Vec::new();
            while let Some(row) = found.next().expect("a pending row could not be read") {
                envelopes.push(row.get(0).expect("a pending envelope is missing"));
            }
            envelopes
        }
    }

    impl Drop for TempState {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn host_only() -> Vec<RedactRule> {
        vec![RedactRule {
            url_host: Some(WILDCARD_HOST.to_string()),
            keep: Some(Keep::Host),
            bundle_id: None,
            drop: Vec::new(),
        }]
    }

    fn keep_everything() -> Vec<RedactRule> {
        vec![RedactRule {
            url_host: Some(WILDCARD_HOST.to_string()),
            keep: Some(Keep::Full),
            bundle_id: None,
            drop: Vec::new(),
        }]
    }

    fn open(state: &TempState, redact: Vec<RedactRule>) -> Buffer {
        Buffer::open(BufferConfig {
            state_dir: state.path.clone(),
            device: "mbp-21".to_string(),
            max_rows: 1_000_000,
            max_bytes: 1_000_000_000,
            redact,
        })
        .expect("the buffer could not be opened")
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

    fn visit(visit_id: i64, url: &str) -> RecordDraft {
        RecordDraft {
            provider: Provider::BrowserHistory,
            kind: Kind::Visit,
            ts: Timestamp::from_millis(1_787_666_156_000),
            degraded: false,
            payload: json!({"url": url, "profile": "MBP_21", "visit_id": visit_id}),
            key: KeySource::BrowserVisit {
                profile: "MBP_21".to_string(),
                generation: 1,
                visit_id,
            },
        }
    }

    async fn fill(buffer: &Buffer, count: i64) {
        let mut drafts = Vec::new();
        for index in 0..count {
            drafts.push(tick(index));
        }
        buffer
            .handle()
            .enqueue(drafts, None)
            .await
            .expect("the batch could not be enqueued");
    }

    fn accepted(count: usize) -> String {
        format!("{{\"accepted\":{count},\"duplicates\":0,\"rejected\":[]}}")
    }

    #[tokio::test(start_paused = true)]
    async fn a_kept_batch_doubles_the_retry_pause_until_one_ships() {
        let state = TempState::new("run-backoff");
        let buffer = open(&state, host_only());
        fill(&buffer, 1).await;

        let mut shipper = Shipper::new(
            buffer.handle(),
            Scripted::new(vec![
                Ok(HttpResponse {
                    status: 503,
                    body: String::new(),
                }),
                Ok(HttpResponse {
                    status: 503,
                    body: String::new(),
                }),
                Ok(HttpResponse {
                    status: 200,
                    body: accepted(1),
                }),
            ]),
        );
        let (_shutdown, listener) = watch::channel(false);

        let started = tokio::time::Instant::now();
        let shipping = tokio::spawn(async move { shipper.run(listener).await });
        let mut elapsed = None;
        for _ in 0..1_000 {
            if state.count("pending") == 0 {
                elapsed = Some(started.elapsed());
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        shipping.abort();

        let elapsed = elapsed.expect("the record never shipped");
        assert!(
            elapsed >= BACKOFF_START * 3 && elapsed < BACKOFF_START * 4,
            "two kept batches must pause 1s then 2s, but the record shipped after {elapsed:?}"
        );

        buffer
            .close()
            .await
            .expect("the buffer could not be closed");
    }

    #[tokio::test]
    async fn the_shipper_stops_rather_than_spinning_once_the_buffer_is_gone() {
        let state = TempState::new("run-closed");
        let buffer = open(&state, host_only());
        let handle = buffer.handle();
        buffer
            .close()
            .await
            .expect("the buffer could not be closed");

        let mut shipper = Shipper::new(handle, Scripted::new(Vec::new()));
        let (_shutdown, listener) = watch::channel(false);
        shipper.run(listener).await;
    }

    #[test]
    fn the_endpoint_is_appended_to_the_service_url() {
        let service_url = Url::parse("http://alpha:8080").expect("the service url parses");
        assert_eq!(
            endpoint(&service_url)
                .expect("the endpoint builds")
                .as_str(),
            "http://alpha:8080/api/v1/records"
        );
    }

    #[test]
    fn a_service_url_with_a_prefix_keeps_it() {
        let service_url = Url::parse("https://alpha.example.com/ingest/").expect("the url parses");
        assert_eq!(
            endpoint(&service_url)
                .expect("the endpoint builds")
                .as_str(),
            "https://alpha.example.com/ingest/api/v1/records"
        );
    }

    #[test]
    fn a_batch_body_is_a_records_array_of_stored_envelopes() {
        let batch = vec![
            PendingRecord {
                id: 1,
                envelope: "{\"seq\":1}".to_string(),
                bytes: 9,
            },
            PendingRecord {
                id: 2,
                envelope: "{\"seq\":2}".to_string(),
                bytes: 9,
            },
        ];
        assert_eq!(
            body_for(&batch),
            "{\"records\":[{\"seq\":1},{\"seq\":2}]}".to_string()
        );
    }

    #[test]
    fn a_well_formed_two_hundred_deletes_the_batch() {
        assert_eq!(
            decide(200, "{\"accepted\":2,\"duplicates\":0,\"rejected\":[]}", 2),
            Disposition::Delete {
                rejections: Vec::new()
            }
        );
    }

    #[test]
    fn a_two_hundred_carrying_a_rejection_still_deletes_the_whole_batch() {
        let body = "{\"accepted\":1,\"duplicates\":0,\"rejected\":[{\"index\":1,\"reason\":\"unknown provider \\\"shell\\\"\"}]}";
        assert_eq!(
            decide(200, body, 2),
            Disposition::Delete {
                rejections: vec![Rejection {
                    index: 1,
                    reason: "unknown provider \"shell\"".to_string(),
                }],
            }
        );
    }

    #[test]
    fn a_two_hundred_counting_duplicates_still_adds_up() {
        assert_eq!(
            decide(200, "{\"accepted\":1,\"duplicates\":1}", 2),
            Disposition::Delete {
                rejections: Vec::new()
            }
        );
    }

    #[test]
    fn a_two_hundred_with_an_unknown_field_is_still_well_formed() {
        let body = "{\"accepted\":2,\"duplicates\":0,\"rejected\":[],\"server\":\"nikki\"}";
        assert_eq!(
            decide(200, body, 2),
            Disposition::Delete {
                rejections: Vec::new()
            }
        );
    }

    #[test]
    fn a_two_hundred_with_a_malformed_body_is_kept() {
        for body in ["<html>proxy</html>", "", "{\"ok\":true}"] {
            match decide(200, body, 2) {
                Disposition::Keep { .. } => {}
                other => panic!("expected `{body}` to be kept, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_two_hundred_whose_counts_do_not_add_up_is_kept() {
        match decide(200, "{\"accepted\":1,\"duplicates\":0,\"rejected\":[]}", 2) {
            Disposition::Keep { .. } => {}
            other => panic!("expected a short count to be kept, got {other:?}"),
        }
    }

    #[test]
    fn every_configuration_status_keeps_the_batch() {
        for status in [401, 403, 404, 405] {
            match decide(status, "", 2) {
                Disposition::Keep { .. } => {}
                other => panic!("expected {status} to keep the batch, got {other:?}"),
            }
        }
    }

    #[test]
    fn every_other_client_error_dead_letters_the_batch() {
        for status in [400, 409, 422] {
            match decide(status, "", 2) {
                Disposition::DeadLetter { .. } => {}
                other => panic!("expected {status} to dead letter, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_payload_too_large_shrinks_the_batch() {
        assert_eq!(decide(413, "", 500), Disposition::Shrink);
    }

    #[test]
    fn every_server_error_keeps_the_batch() {
        for status in [500, 502, 503, 504] {
            match decide(status, "", 2) {
                Disposition::Keep { .. } => {}
                other => panic!("expected {status} to keep the batch, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_unexpected_status_keeps_the_batch_rather_than_destroying_it() {
        for status in [204, 302, 100] {
            match decide(status, "", 2) {
                Disposition::Keep { .. } => {}
                other => panic!("expected {status} to keep the batch, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_two_hundred_deletes_every_record_including_the_rejected_one() {
        let state = TempState::new("delete-on-200");
        let buffer = open(&state, host_only());
        fill(&buffer, 3).await;

        let body = "{\"accepted\":2,\"duplicates\":0,\"rejected\":[{\"index\":1,\"reason\":\"bad kind\"}]}";
        let mut shipper = Shipper::new(buffer.handle(), Scripted::ok(200, body));
        assert_eq!(
            shipper.ship_once().await.expect("the batch could not ship"),
            Progress::Shipped { count: 3 }
        );
        assert_eq!(state.count("pending"), 0);
        assert_eq!(state.count("dead_letter"), 0);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_malformed_two_hundred_keeps_the_batch_for_the_next_attempt() {
        let state = TempState::new("keep-on-malformed-200");
        let buffer = open(&state, host_only());
        fill(&buffer, 3).await;

        let mut shipper = Shipper::new(buffer.handle(), Scripted::ok(200, "<html>proxy</html>"));
        match shipper.ship_once().await.expect("the attempt failed") {
            Progress::Kept { .. } => {}
            other => panic!("expected the batch to be kept, got {other:?}"),
        }
        assert_eq!(state.count("pending"), 3);
        assert_eq!(state.count("dead_letter"), 0);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_not_found_keeps_the_batch_rather_than_dead_lettering_it() {
        let state = TempState::new("keep-on-404");
        let buffer = open(&state, host_only());
        fill(&buffer, 3).await;

        let mut shipper = Shipper::new(buffer.handle(), Scripted::ok(404, "not found"));
        match shipper.ship_once().await.expect("the attempt failed") {
            Progress::Kept { .. } => {}
            other => panic!("expected the batch to be kept, got {other:?}"),
        }
        assert_eq!(state.count("pending"), 3);
        assert_eq!(state.count("dead_letter"), 0);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_bad_request_dead_letters_the_batch_and_shipping_continues() {
        let state = TempState::new("dead-letter-on-400");
        let buffer = open(&state, host_only());
        fill(&buffer, 5).await;

        let transport = Scripted::new(vec![
            Ok(HttpResponse {
                status: 400,
                body: "malformed".to_string(),
            }),
            Ok(HttpResponse {
                status: 200,
                body: accepted(2),
            }),
        ]);
        let mut shipper = Shipper::new(buffer.handle(), transport);
        assert_eq!(
            shipper.ship_once().await.expect("the attempt failed"),
            Progress::DeadLettered { count: 5 }
        );
        assert_eq!(state.count("pending"), 0);
        assert_eq!(state.count("dead_letter"), 5);

        fill(&buffer, 2).await;
        assert_eq!(
            shipper.ship_once().await.expect("the attempt failed"),
            Progress::Shipped { count: 2 }
        );
        assert_eq!(state.count("pending"), 0);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_transport_failure_keeps_the_batch() {
        let state = TempState::new("keep-on-transport-failure");
        let buffer = open(&state, host_only());
        fill(&buffer, 3).await;

        let transport = Scripted::new(vec![Err(TransportError(
            "connection reset by peer".to_string(),
        ))]);
        let mut shipper = Shipper::new(buffer.handle(), transport);
        match shipper.ship_once().await.expect("the attempt failed") {
            Progress::Kept { reason } => assert!(reason.contains("connection reset")),
            other => panic!("expected the batch to be kept, got {other:?}"),
        }
        assert_eq!(state.count("pending"), 3);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_server_error_keeps_records_buffered_until_it_recovers() {
        let state = TempState::new("drain-after-500");
        let buffer = open(&state, host_only());
        fill(&buffer, 4).await;

        let transport = Scripted::new(vec![
            Ok(HttpResponse {
                status: 500,
                body: "boom".to_string(),
            }),
            Ok(HttpResponse {
                status: 200,
                body: accepted(4),
            }),
        ]);
        let mut shipper = Shipper::new(buffer.handle(), transport);
        match shipper.ship_once().await.expect("the attempt failed") {
            Progress::Kept { .. } => {}
            other => panic!("expected the batch to be kept, got {other:?}"),
        }
        assert_eq!(state.count("pending"), 4);

        assert_eq!(
            shipper.ship_once().await.expect("the attempt failed"),
            Progress::Shipped { count: 4 }
        );
        assert_eq!(state.count("pending"), 0);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_payload_too_large_halves_the_batch_down_to_the_floor_then_dead_letters() {
        let state = TempState::new("halve-to-floor");
        let buffer = open(&state, host_only());
        fill(&buffer, MAX_BATCH as i64).await;

        let mut answers = Vec::new();
        for _ in 0..8 {
            answers.push(Ok(HttpResponse {
                status: 413,
                body: String::new(),
            }));
        }
        let mut shipper = Shipper::new(buffer.handle(), Scripted::new(answers));

        let expected = [250, 125, 62, 31, 15, 10];
        for batch_size in expected {
            assert_eq!(
                shipper.ship_once().await.expect("the attempt failed"),
                Progress::Shrunk { batch_size }
            );
            assert_eq!(shipper.batch_size(), batch_size);
        }
        assert_eq!(state.count("pending"), MAX_BATCH as u64);

        assert_eq!(
            shipper.ship_once().await.expect("the attempt failed"),
            Progress::DeadLettered { count: MIN_BATCH }
        );
        assert_eq!(state.count("dead_letter"), MIN_BATCH as u64);
        assert_eq!(state.count("pending"), (MAX_BATCH - MIN_BATCH) as u64);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_batch_never_exceeds_five_hundred_records() {
        let state = TempState::new("batch-cap");
        let buffer = open(&state, host_only());
        fill(&buffer, 600).await;

        let transport = Scripted::new(vec![
            Ok(HttpResponse {
                status: 200,
                body: accepted(MAX_BATCH),
            }),
            Ok(HttpResponse {
                status: 200,
                body: accepted(100),
            }),
        ]);
        let mut shipper = Shipper::new(buffer.handle(), transport);
        assert_eq!(
            shipper.ship_once().await.expect("the attempt failed"),
            Progress::Shipped { count: MAX_BATCH }
        );
        assert_eq!(
            shipper.ship_once().await.expect("the attempt failed"),
            Progress::Shipped { count: 100 }
        );
        assert_eq!(state.count("pending"), 0);

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn an_empty_buffer_reports_itself_idle_without_a_request() {
        let state = TempState::new("idle");
        let buffer = open(&state, host_only());

        let transport = Scripted::new(Vec::new());
        let mut shipper = Shipper::new(buffer.handle(), transport);
        assert_eq!(
            shipper.ship_once().await.expect("the attempt failed"),
            Progress::Idle
        );

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn the_shipped_body_carries_the_stored_envelopes_verbatim() {
        let state = TempState::new("body-shape");
        let buffer = open(&state, host_only());
        fill(&buffer, 2).await;
        let stored = state.envelopes();

        let transport = Scripted::ok(200, &accepted(2));
        let mut shipper = Shipper::new(buffer.handle(), transport);
        shipper.ship_once().await.expect("the attempt failed");

        let bodies = shipper.transport.bodies();
        assert_eq!(bodies.len(), 1);
        let sent: serde_json::Value =
            serde_json::from_str(&bodies[0]).expect("the request body parses");
        let records = sent["records"].as_array().expect("records is an array");
        assert_eq!(records.len(), 2);
        for (index, envelope) in stored.iter().enumerate() {
            let envelope: serde_json::Value =
                serde_json::from_str(envelope).expect("a stored envelope parses");
            assert_eq!(records[index], envelope);
        }

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn no_redacted_path_survives_into_the_buffered_envelope() {
        let token = "unmistakable-secret-token";
        let url = format!("https://example.com/{token}?q={token}");

        let leaky = TempState::new("redaction-control");
        let buffer = open(&leaky, keep_everything());
        buffer
            .handle()
            .enqueue(vec![visit(1, &url)], None)
            .await
            .expect("the control record could not be enqueued");
        let control = leaky.envelopes().join("");
        assert!(
            control.contains(token),
            "the control envelope must carry the token, or the assertion below proves nothing"
        );
        buffer.close().await.expect("the buffer did not close");

        let state = TempState::new("redaction-live");
        let buffer = open(&state, host_only());
        buffer
            .handle()
            .enqueue(vec![visit(1, &url)], None)
            .await
            .expect("the record could not be enqueued");
        let stored = state.envelopes().join("");
        assert!(
            !stored.contains(token),
            "a redacted path reached the buffer: {stored}"
        );
        assert!(stored.contains("https://example.com/"));

        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn redaction_runs_before_the_key_so_no_url_can_reach_it() {
        let state = TempState::new("redaction-before-key");
        let buffer = open(&state, host_only());
        let sealed = buffer
            .handle()
            .enqueue(vec![visit(1, "https://example.com/secret?q=secret")], None)
            .await
            .expect("the record could not be enqueued");

        assert_eq!(sealed[0].payload["url"], "https://example.com/");
        assert_eq!(sealed[0].dedup_key.len(), 16);
        assert!(!sealed[0].dedup_key.contains("secret"));

        buffer.close().await.expect("the buffer did not close");
    }
}
