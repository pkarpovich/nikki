use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::Sender;
use tokio::time::{MissedTickBehavior, interval};

use super::{Ctx, Emission, Provider, ProviderError};
use crate::extract::{CommandOutput, SUBPROCESS_DEADLINE, run_with_deadline};
use crate::runtime::buffer::BufferHandle;
use crate::runtime::dedup::{KEY_HEX_CHARS, UNIT_SEPARATOR};
use crate::runtime::{self, Cursor, KeySource, Kind, RecordDraft, Timestamp};

pub const USER_DATA_RELATIVE: &str = "Library/Application Support/Dia/User Data";
pub const LOCAL_STATE_FILE: &str = "Local State";
pub const HISTORY_FILE: &str = "History";
pub const HISTORY_JOURNAL_FILE: &str = "History-journal";
pub const SNAPSHOT_DIR: &str = "history-snapshot";
pub const PAGE_LIMIT: usize = 5_000;

const CLONE_PROGRAM: &str = "/bin/cp";
const CLONE_FLAG: &str = "-c";
const WEBKIT_EPOCH_OFFSET_MILLIS: i64 = 11_644_473_600_000;

pub fn user_data_dir(home: &Path) -> PathBuf {
    home.join(USER_DATA_RELATIVE)
}

pub struct BrowserHistoryProvider {
    user_data: PathBuf,
    cursors: BufferHandle,
    state: Option<HistoryState>,
}

impl BrowserHistoryProvider {
    pub fn new(user_data: PathBuf, cursors: BufferHandle) -> BrowserHistoryProvider {
        BrowserHistoryProvider {
            user_data,
            cursors,
            state: None,
        }
    }
}

impl Provider for BrowserHistoryProvider {
    fn name(&self) -> &'static str {
        runtime::Provider::BrowserHistory.as_str()
    }

    async fn run(&mut self, ctx: Ctx, out: Sender<Emission>) -> Result<(), ProviderError> {
        let BrowserHistoryProvider {
            user_data,
            cursors,
            state,
        } = self;
        let profile = ctx.config.browser.profile.clone();
        let revisit_window = ctx.config.revisit_window as i64;
        let snapshot = ctx.config.state_dir.join(SNAPSHOT_DIR);

        let directory = match directory_for(user_data, &profile) {
            Ok(directory) => directory,
            Err(reason) => return Err(ProviderError(reason)),
        };
        tracing::info!(%profile, %directory, "the browser profile resolved");

        if state.is_none() {
            *state = stored_state(cursors, &profile).await?;
        }

        let mut ticker = interval(Duration::from_secs(ctx.config.history_poll_interval));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;

            let outcome = poll_once(
                user_data,
                &snapshot,
                &profile,
                revisit_window,
                state.clone(),
                PAGE_LIMIT,
            )
            .await;
            let Some(PollOutcome {
                records,
                state: polled,
            }) = outcome
            else {
                continue;
            };

            let cursor = Cursor {
                provider: runtime::Provider::BrowserHistory,
                key: profile.clone(),
                value: polled.encode(),
            };
            if out
                .send(Emission::with_cursor(records, cursor))
                .await
                .is_err()
            {
                tracing::info!("the runtime is gone, so the browser history provider stops");
                return Ok(());
            }
            *state = Some(polled);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryState {
    pub cursor: i64,
    pub generation: u64,
    #[serde(default)]
    pub shipped: BTreeMap<i64, String>,
}

impl HistoryState {
    fn fresh(highest: i64, revisit_window: i64) -> HistoryState {
        HistoryState {
            cursor: (highest - revisit_window).max(0),
            generation: 1,
            shipped: BTreeMap::new(),
        }
    }

    fn regenerated(&self, highest: i64, revisit_window: i64) -> HistoryState {
        HistoryState {
            cursor: (highest - revisit_window).max(0),
            generation: self.generation + 1,
            shipped: BTreeMap::new(),
        }
    }

    fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    fn decode(value: &str) -> Option<HistoryState> {
        match serde_json::from_str(value) {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::warn!(%error, "the stored browser cursor does not parse and is restarted");
                None
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisitRow {
    pub id: i64,
    pub visit_time: i64,
    pub transition: i64,
    pub visit_duration: i64,
    pub url: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PollOutcome {
    pub records: Vec<RecordDraft>,
    pub state: HistoryState,
}

async fn stored_state(
    cursors: &BufferHandle,
    profile: &str,
) -> Result<Option<HistoryState>, ProviderError> {
    let stored = cursors
        .cursor(runtime::Provider::BrowserHistory, profile.to_string())
        .await;
    let stored = match stored {
        Ok(stored) => stored,
        Err(error) => {
            return Err(ProviderError(format!(
                "the buffer refused a cursor: {error}"
            )));
        }
    };
    let Some(stored) = stored else {
        return Ok(None);
    };
    Ok(HistoryState::decode(&stored))
}

fn directory_for(user_data: &Path, profile: &str) -> Result<String, String> {
    let path = user_data.join(LOCAL_STATE_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) => return Err(format!("{} could not be read: {source}", path.display())),
    };
    match resolve_directory(&text, profile) {
        Ok(directory) => Ok(directory),
        Err(reason) => Err(format!("{} {reason}", path.display())),
    }
}

fn resolve_directory(text: &str, profile: &str) -> Result<String, String> {
    let local_state: LocalState = match serde_json::from_str(text) {
        Ok(local_state) => local_state,
        Err(error) => return Err(format!("is not valid json: {error}")),
    };
    let LocalState {
        profile: LocalStateProfile { info_cache },
    } = local_state;

    let mut known = Vec::new();
    for (directory, entry) in info_cache {
        let Some(name) = entry.get("name") else {
            continue;
        };
        let Some(name) = name.as_str() else {
            continue;
        };
        if name == profile {
            return Ok(directory);
        }
        known.push(name.to_string());
    }
    Err(format!(
        "holds no profile named `{profile}`; the profiles that exist are {}",
        listed(&known)
    ))
}

fn listed(known: &[String]) -> String {
    if known.is_empty() {
        return "none".to_string();
    }
    known.join(", ")
}

async fn poll_once(
    user_data: &Path,
    snapshot: &Path,
    profile: &str,
    revisit_window: i64,
    state: Option<HistoryState>,
    page: usize,
) -> Option<PollOutcome> {
    let directory = match directory_for(user_data, profile) {
        Ok(directory) => directory,
        Err(reason) => {
            tracing::warn!(
                reason,
                "the browser profile no longer resolves, so this poll is skipped"
            );
            return None;
        }
    };

    let copy = clone_history(&user_data.join(directory), snapshot).await?;
    let outcome = collect_from(&copy, profile, revisit_window, state, page);
    discard(snapshot);
    outcome
}

async fn clone_history(source: &Path, snapshot: &Path) -> Option<PathBuf> {
    discard(snapshot);
    if let Err(error) = fs::create_dir_all(snapshot) {
        tracing::warn!(path = %snapshot.display(), %error, "the snapshot directory could not be created");
        return None;
    }

    let history = snapshot.join(HISTORY_FILE);
    if !clone_file(&source.join(HISTORY_FILE), &history).await {
        discard(snapshot);
        return None;
    }

    let journal = source.join(HISTORY_JOURNAL_FILE);
    if journal.is_file() && !clone_file(&journal, &snapshot.join(HISTORY_JOURNAL_FILE)).await {
        tracing::warn!("the history journal could not be cloned, so this poll is skipped");
        discard(snapshot);
        return None;
    }
    Some(history)
}

async fn clone_file(source: &Path, destination: &Path) -> bool {
    let source = source.to_string_lossy().into_owned();
    let destination = destination.to_string_lossy().into_owned();
    let output = run_with_deadline(
        Path::new(CLONE_PROGRAM),
        &[CLONE_FLAG, &source, &destination],
        SUBPROCESS_DEADLINE,
    )
    .await;

    let Some(CommandOutput {
        succeeded, stderr, ..
    }) = output
    else {
        return false;
    };
    if !succeeded {
        tracing::warn!(
            source,
            stderr = stderr.trim(),
            "the history could not be cloned"
        );
        return false;
    }
    true
}

fn discard(snapshot: &Path) {
    let _ = fs::remove_dir_all(snapshot);
}

fn collect_from(
    path: &Path,
    profile: &str,
    revisit_window: i64,
    state: Option<HistoryState>,
    page: usize,
) -> Option<PollOutcome> {
    let connection = open_snapshot(path)?;
    match collect(&connection, profile, revisit_window, state, page) {
        Ok(outcome) => Some(outcome),
        Err(error) => {
            tracing::warn!(%error, "the history snapshot could not be read, so this poll is skipped");
            None
        }
    }
}

fn open_snapshot(path: &Path) -> Option<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = match Connection::open_with_flags(path, flags) {
        Ok(connection) => connection,
        Err(error) => {
            tracing::warn!(%error, "the history snapshot could not be opened");
            return None;
        }
    };

    let checked: Result<String, rusqlite::Error> =
        connection.query_row("PRAGMA quick_check", [], |row| row.get(0));
    let checked = match checked {
        Ok(checked) => checked,
        Err(error) => {
            tracing::warn!(%error, "the history snapshot failed its integrity check");
            return None;
        }
    };
    if checked != "ok" {
        tracing::warn!(
            checked,
            "the history snapshot is torn, so this poll is skipped"
        );
        return None;
    }
    Some(connection)
}

fn collect(
    connection: &Connection,
    profile: &str,
    revisit_window: i64,
    state: Option<HistoryState>,
    page: usize,
) -> Result<PollOutcome, rusqlite::Error> {
    let highest = highest_visit_id(connection)?;
    let mut state = match state {
        None => HistoryState::fresh(highest, revisit_window),
        Some(state) if highest < state.cursor => {
            tracing::warn!(
                profile,
                highest,
                cursor = state.cursor,
                generation = state.generation + 1,
                "the history database was replaced, so the generation advances"
            );
            state.regenerated(highest, revisit_window)
        }
        Some(state) => state,
    };

    let mut position = (state.cursor - revisit_window).max(0);
    let mut records = Vec::new();
    loop {
        let rows = read_page(connection, position, page)?;
        let read = rows.len();
        for row in rows {
            position = row.id;
            if row.url.is_empty() {
                continue;
            }
            let digest = digest(&row);
            if state.shipped.get(&row.id) == Some(&digest) {
                continue;
            }
            state.shipped.insert(row.id, digest);
            records.push(visit_record(profile, state.generation, row));
        }
        if read < page {
            break;
        }
    }

    state.cursor = state.cursor.max(position);
    let floor = state.cursor - revisit_window;
    state.shipped.retain(|id, _| *id > floor);
    Ok(PollOutcome { records, state })
}

fn highest_visit_id(connection: &Connection) -> Result<i64, rusqlite::Error> {
    let highest: Option<i64> =
        connection.query_row("SELECT MAX(id) FROM visits", [], |row| row.get(0))?;
    Ok(highest.unwrap_or_default())
}

fn read_page(
    connection: &Connection,
    from: i64,
    limit: usize,
) -> Result<Vec<VisitRow>, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT v.id, v.visit_time, v.transition, v.visit_duration, u.url, u.title
         FROM visits v JOIN urls u ON u.id = v.url
         WHERE v.id > ?1
         ORDER BY v.id
         LIMIT ?2",
    )?;
    let mut found = statement.query(params![from, limit as i64])?;
    let mut rows = Vec::new();
    loop {
        let Some(row) = found.next()? else {
            break;
        };
        let url: Option<String> = row.get(4)?;
        rows.push(VisitRow {
            id: row.get(0)?,
            visit_time: row.get(1)?,
            transition: row.get(2)?,
            visit_duration: row.get(3)?,
            url: url.unwrap_or_default(),
            title: row.get(5)?,
        });
    }
    Ok(rows)
}

fn digest(row: &VisitRow) -> String {
    let VisitRow {
        transition,
        visit_duration,
        title,
        ..
    } = row;
    let title = title.as_deref().unwrap_or_default();
    let joined = [title, &transition.to_string(), &visit_duration.to_string()].join(UNIT_SEPARATOR);
    let digest = Sha256::digest(joined.as_bytes());
    let mut hex = String::with_capacity(KEY_HEX_CHARS);
    for byte in &digest[..KEY_HEX_CHARS / 2] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn visit_record(profile: &str, generation: u64, row: VisitRow) -> RecordDraft {
    let VisitRow {
        id,
        visit_time,
        transition,
        visit_duration,
        url,
        title,
    } = row;

    let mut payload = Map::new();
    payload.insert("url".to_string(), Value::String(url));
    if let Some(title) = title
        && !title.is_empty()
    {
        payload.insert("title".to_string(), Value::String(title));
    }
    payload.insert("profile".to_string(), Value::String(profile.to_string()));
    payload.insert("transition".to_string(), json!(transition));
    payload.insert("visit_id".to_string(), json!(id));
    payload.insert(
        "duration_ms".to_string(),
        json!(duration_millis(visit_duration)),
    );

    RecordDraft {
        provider: runtime::Provider::BrowserHistory,
        kind: Kind::Visit,
        ts: Timestamp::from_millis(unix_millis(visit_time)),
        degraded: false,
        payload: Value::Object(payload),
        key: KeySource::BrowserVisit {
            profile: profile.to_string(),
            generation,
            visit_id: id,
        },
    }
}

fn unix_millis(visit_time: i64) -> i64 {
    visit_time / 1_000 - WEBKIT_EPOCH_OFFSET_MILLIS
}

fn duration_millis(visit_duration: i64) -> i64 {
    visit_duration / 1_000
}

#[derive(Deserialize)]
struct LocalState {
    #[serde(default)]
    profile: LocalStateProfile,
}

#[derive(Default, Deserialize)]
struct LocalStateProfile {
    #[serde(default)]
    info_cache: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{Receiver, channel};

    use crate::config::{Config, default_redact};
    use crate::providers::tests::test_config;
    use crate::runtime::buffer::{Buffer, BufferConfig};
    use crate::runtime::redact::Redactor;

    const HISTORY: &[u8] = include_bytes!("../../fixtures/history_sample.db");
    const LOCAL_STATE: &str = include_str!("../../fixtures/dia_local_state.json");

    const PROFILE: &str = "MBP_21";
    const DIRECTORY: &str = "Default";
    const WINDOW: i64 = 500;
    const NEWEST_VISIT: i64 = 906;

    struct TempHome {
        path: PathBuf,
    }

    impl TempHome {
        fn new(name: &str) -> TempHome {
            let path = std::env::temp_dir().join(format!("nikki-history-test-{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(path.join("state")).expect("the state directory could not be made");
            let home = TempHome { path };
            home.write_local_state(LOCAL_STATE);
            home.write_history(HISTORY);
            home
        }

        fn user_data(&self) -> PathBuf {
            self.path.join("user-data")
        }

        fn state_dir(&self) -> PathBuf {
            self.path.join("state")
        }

        fn snapshot(&self) -> PathBuf {
            self.state_dir().join(SNAPSHOT_DIR)
        }

        fn history(&self) -> PathBuf {
            self.user_data().join(DIRECTORY).join(HISTORY_FILE)
        }

        fn write_local_state(&self, text: &str) {
            let user_data = self.user_data();
            fs::create_dir_all(&user_data).expect("the user data directory could not be made");
            fs::write(user_data.join(LOCAL_STATE_FILE), text)
                .expect("the local state could not be written");
        }

        fn write_history(&self, bytes: &[u8]) {
            let profile = self.user_data().join(DIRECTORY);
            fs::create_dir_all(&profile).expect("the profile directory could not be made");
            fs::write(profile.join(HISTORY_FILE), bytes).expect("the history could not be written");
        }

        fn source(&self) -> Connection {
            let connection =
                Connection::open(self.history()).expect("the history could not be opened");
            connection
                .busy_timeout(Duration::from_millis(50))
                .expect("the busy timeout could not be set");
            connection
        }

        fn config(&self) -> Config {
            let mut config = test_config(30);
            config.state_dir = self.state_dir();
            config.browser.profile = PROFILE.to_string();
            config.history_poll_interval = 3600;
            config
        }

        fn ctx(&self) -> Ctx {
            Ctx {
                config: std::sync::Arc::new(self.config()),
            }
        }

        fn buffer(&self) -> Buffer {
            Buffer::open(BufferConfig {
                state_dir: self.state_dir(),
                device: "mbp-21".to_string(),
                max_rows: 1_000_000,
                max_bytes: 1_000_000_000,
                redact: default_redact(),
            })
            .expect("the buffer could not be opened")
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    async fn poll(home: &TempHome, state: Option<HistoryState>, page: usize) -> PollOutcome {
        poll_once(
            &home.user_data(),
            &home.snapshot(),
            PROFILE,
            WINDOW,
            state,
            page,
        )
        .await
        .expect("the poll produced nothing")
    }

    fn visit_ids(records: &[RecordDraft]) -> Vec<i64> {
        let mut ids = Vec::new();
        for record in records {
            ids.push(record.payload["visit_id"].as_i64().expect("a visit id"));
        }
        ids
    }

    fn record_with(records: &[RecordDraft], visit_id: i64) -> RecordDraft {
        for record in records {
            if record.payload["visit_id"] == json!(visit_id) {
                return record.clone();
            }
        }
        panic!("visit {visit_id} was not emitted");
    }

    #[test]
    fn a_visit_time_converts_to_the_captured_wall_clock() {
        let ts = Timestamp::from_millis(unix_millis(13_432_139_756_000_000));
        assert_eq!(ts.to_rfc3339(), "2026-08-25T13:55:56.000Z");
        assert_eq!(
            13_432_139_756_000_000_i64 / 1_000_000 - 11_644_473_600,
            1_787_666_156
        );
    }

    #[test]
    fn a_visit_duration_is_microseconds_and_ships_as_milliseconds() {
        assert_eq!(duration_millis(450_404), 450);
        assert_eq!(duration_millis(1_076_461), 1_076);
        assert_eq!(duration_millis(22_874_541), 22_874);
        assert_eq!(duration_millis(0), 0);
    }

    #[test]
    fn the_configured_display_name_resolves_to_its_directory() {
        assert_eq!(
            resolve_directory(LOCAL_STATE, "MBP_21"),
            Ok("Default".to_string())
        );
        assert_eq!(
            resolve_directory(LOCAL_STATE, "Intapp"),
            Ok("Profile 2".to_string())
        );
    }

    #[test]
    fn an_entry_without_a_string_name_is_skipped_rather_than_failing_the_read() {
        let reason = resolve_directory(LOCAL_STATE, "Nowhere").expect_err("the name is absent");
        assert!(reason.contains("MBP_21"), "reason was {reason}");
        assert!(reason.contains("MBA_22"), "reason was {reason}");
        assert!(reason.contains("Intapp"), "reason was {reason}");
    }

    #[test]
    fn an_unknown_field_anywhere_in_local_state_is_tolerated() {
        let text = r#"{
            "future_top_level": {"anything": 1},
            "profile": {"info_cache": {"Default": {"name": "MBP_21", "future": [1, 2]}},
                        "last_used": "Default"}
        }"#;
        assert_eq!(resolve_directory(text, "MBP_21"), Ok("Default".to_string()));
    }

    #[test]
    fn a_local_state_without_any_profile_names_them_as_none() {
        let reason = resolve_directory("{}", "MBP_21").expect_err("nothing resolves");
        assert!(reason.contains("none"), "reason was {reason}");
        assert!(
            resolve_directory("not json", "MBP_21")
                .expect_err("nothing resolves")
                .contains("not valid json")
        );
    }

    #[test]
    fn a_stored_state_survives_a_round_trip_through_the_cursor_value() {
        let mut shipped = BTreeMap::new();
        shipped.insert(929_269, "a70f31d9b8c2e546".to_string());
        let state = HistoryState {
            cursor: 929_402,
            generation: 3,
            shipped,
        };
        assert_eq!(HistoryState::decode(&state.encode()), Some(state));
        assert_eq!(HistoryState::decode("not json"), None);
    }

    #[tokio::test]
    async fn a_first_run_starts_a_window_below_the_newest_visit() {
        let home = TempHome::new("first-run");
        let PollOutcome { records, state } = poll(&home, None, PAGE_LIMIT).await;

        assert_eq!(state.generation, 1);
        assert_eq!(state.cursor, NEWEST_VISIT);
        assert_eq!(visit_ids(&records), vec![901, 902, 903, 904, 905, 906]);
        assert_eq!(
            HistoryState::fresh(929_885, WINDOW).cursor,
            929_885 - WINDOW,
            "a populated history starts a window below its newest visit"
        );
    }

    #[tokio::test]
    async fn a_visit_carries_the_wire_payload_for_its_row() {
        let home = TempHome::new("payload");
        let PollOutcome { records, .. } = poll(&home, None, PAGE_LIMIT).await;
        let RecordDraft {
            provider,
            kind,
            ts,
            degraded,
            payload,
            key,
        } = record_with(&records, 904);

        assert_eq!(provider, runtime::Provider::BrowserHistory);
        assert_eq!(kind, Kind::Visit);
        assert_eq!(ts.to_rfc3339(), "2026-08-25T13:58:56.000Z");
        assert!(!degraded);
        assert_eq!(
            payload["url"],
            "chrome-extension://gighmmpiobklfepjocnamgkkbiglidom/options.html"
        );
        assert_eq!(payload["title"], "Extension options");
        assert_eq!(payload["profile"], PROFILE);
        assert_eq!(payload["transition"], 805_306_368_i64);
        assert_eq!(payload["visit_id"], 904);
        assert_eq!(payload["duration_ms"], 22_874);
        assert_eq!(
            key,
            KeySource::BrowserVisit {
                profile: PROFILE.to_string(),
                generation: 1,
                visit_id: 904,
            }
        );
    }

    #[tokio::test]
    async fn a_row_without_a_title_carries_no_title_field() {
        let home = TempHome::new("titleless");
        let PollOutcome { records, .. } = poll(&home, None, PAGE_LIMIT).await;
        let RecordDraft { payload, .. } = record_with(&records, 905);
        assert_eq!(payload.get("title"), None);
        assert_eq!(payload["url"], "data:text/html;base64,PGgxPmhlbGxvPC9oMT4=");
    }

    #[tokio::test]
    async fn a_title_carrying_a_comma_survives_whole() {
        let home = TempHome::new("comma-title");
        let PollOutcome { records, .. } = poll(&home, None, PAGE_LIMIT).await;
        let RecordDraft { payload, .. } = record_with(&records, 902);
        assert_eq!(payload["title"], "ENG-1, window provider, in progress");
        assert_eq!(payload["transition"], 822_083_584_i64);
        assert_eq!(payload["duration_ms"], 1_076);
    }

    #[tokio::test]
    async fn a_file_row_ships_with_its_url_reduced_to_the_scheme_rather_than_dropped() {
        let home = TempHome::new("file-row");
        let PollOutcome { records, .. } = poll(&home, None, PAGE_LIMIT).await;
        let RecordDraft { mut payload, .. } = record_with(&records, 903);

        Redactor::new(&default_redact()).apply(&mut payload);
        assert_eq!(payload["url"], "file:///");
        assert_eq!(payload["title"], "Coverage report");
        assert_eq!(payload["visit_id"], 903);
    }

    #[tokio::test]
    async fn paging_reads_every_row_past_the_page_limit() {
        let home = TempHome::new("paging");
        let PollOutcome { records, state } = poll(&home, None, 2).await;

        assert_eq!(visit_ids(&records), vec![901, 902, 903, 904, 905, 906]);
        assert_eq!(state.cursor, NEWEST_VISIT);
    }

    #[tokio::test]
    async fn an_unchanged_re_read_is_not_emitted_while_a_filled_in_duration_is() {
        let home = TempHome::new("re-read");
        let PollOutcome { state, .. } = poll(&home, None, PAGE_LIMIT).await;

        let PollOutcome { records, state } = poll(&home, Some(state), PAGE_LIMIT).await;
        assert!(
            records.is_empty(),
            "an unchanged re-read must not be emitted"
        );

        home.source()
            .execute(
                "UPDATE visits SET visit_duration = 90000000 WHERE id = 906",
                [],
            )
            .expect("the duration could not be filled in");

        let PollOutcome { records, state } = poll(&home, Some(state), PAGE_LIMIT).await;
        assert_eq!(visit_ids(&records), vec![906]);
        assert_eq!(records[0].payload["duration_ms"], 90_000);
        assert_eq!(state.cursor, NEWEST_VISIT);
    }

    #[tokio::test]
    async fn a_changed_title_is_emitted_as_a_correction_under_the_same_key() {
        let home = TempHome::new("changed-title");
        let PollOutcome { state, .. } = poll(&home, None, PAGE_LIMIT).await;

        home.source()
            .execute(
                "UPDATE urls SET title = 'Coverage report, updated' WHERE id = 3",
                [],
            )
            .expect("the title could not be rewritten");

        let PollOutcome { records, .. } = poll(&home, Some(state), PAGE_LIMIT).await;
        assert_eq!(visit_ids(&records), vec![903]);
        assert_eq!(records[0].payload["title"], "Coverage report, updated");
        assert_eq!(
            records[0].key,
            KeySource::BrowserVisit {
                profile: PROFILE.to_string(),
                generation: 1,
                visit_id: 903,
            }
        );
    }

    #[tokio::test]
    async fn a_replaced_database_advances_the_generation_and_changes_every_key() {
        let home = TempHome::new("generation");
        let PollOutcome { records, state } = poll(&home, None, PAGE_LIMIT).await;
        let before = record_with(&records, 901)
            .into_envelope("mbp-21", 1)
            .dedup_key;
        assert_eq!(state.generation, 1);

        home.source()
            .execute("UPDATE visits SET id = id - 900 WHERE id > 900", [])
            .expect("the visit ids could not be restarted");

        let PollOutcome { records, state } = poll(&home, Some(state), PAGE_LIMIT).await;
        assert_eq!(state.generation, 2);
        assert_eq!(visit_ids(&records), vec![1, 2, 3, 4, 5, 6]);

        let after = record_with(&records, 1)
            .into_envelope("mbp-21", 2)
            .dedup_key;
        assert_ne!(before, after, "a reused visit id must not reuse its key");
    }

    #[tokio::test]
    async fn the_shipped_map_is_pruned_to_the_revisit_window() {
        let home = TempHome::new("pruning");
        let PollOutcome { state, .. } = poll(&home, None, 3).await;
        assert_eq!(state.shipped.len(), 6);

        let narrow = poll_once(
            &home.user_data(),
            &home.snapshot(),
            PROFILE,
            2,
            Some(HistoryState {
                cursor: 0,
                generation: 1,
                shipped: BTreeMap::new(),
            }),
            PAGE_LIMIT,
        )
        .await
        .expect("the narrow poll produced nothing");

        assert_eq!(narrow.state.cursor, NEWEST_VISIT);
        for id in narrow.state.shipped.keys() {
            assert!(*id > NEWEST_VISIT - 2, "visit {id} is past the window");
        }
    }

    #[tokio::test]
    async fn a_profile_that_disappears_at_poll_time_skips_the_poll() {
        let home = TempHome::new("profile-gone");
        home.write_local_state(r#"{"profile":{"info_cache":{"Profile 1":{"name":"MBA_22"}}}}"#);

        let outcome = poll_once(
            &home.user_data(),
            &home.snapshot(),
            PROFILE,
            WINDOW,
            None,
            PAGE_LIMIT,
        )
        .await;
        assert!(outcome.is_none());
    }

    #[tokio::test]
    async fn a_locked_database_is_read_through_a_clone_where_a_direct_read_fails() {
        let home = TempHome::new("clone-under-lock");
        let holder = home.source();
        holder
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("the exclusive lock could not be taken");

        let refused = home
            .source()
            .query_row("SELECT COUNT(*) FROM visits", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect_err("a direct read must fail under the lock");
        assert!(
            refused.to_string().contains("locked"),
            "the direct read failed for another reason: {refused}"
        );

        let PollOutcome { records, state } = poll(&home, None, PAGE_LIMIT).await;
        assert_eq!(visit_ids(&records), vec![901, 902, 903, 904, 905, 906]);
        assert_eq!(state.cursor, NEWEST_VISIT);

        holder
            .execute_batch("COMMIT")
            .expect("the exclusive lock could not be released");
    }

    #[tokio::test]
    async fn a_snapshot_failing_its_integrity_check_is_discarded_and_the_cursor_stays_put() {
        let home = TempHome::new("torn-snapshot");
        let PollOutcome { state, .. } = poll(&home, None, PAGE_LIMIT).await;
        let before = state.clone();

        home.write_history(b"this is not a database at all, only a torn page");
        let outcome = poll_once(
            &home.user_data(),
            &home.snapshot(),
            PROFILE,
            WINDOW,
            Some(state),
            PAGE_LIMIT,
        )
        .await;

        assert!(outcome.is_none());
        assert_eq!(before.cursor, NEWEST_VISIT);
        assert!(
            !home.snapshot().exists(),
            "the discarded snapshot must not survive"
        );
    }

    #[tokio::test]
    async fn a_missing_history_leaves_no_snapshot_behind() {
        let home = TempHome::new("missing-history");
        fs::remove_file(home.history()).expect("the history could not be removed");

        let outcome = poll_once(
            &home.user_data(),
            &home.snapshot(),
            PROFILE,
            WINDOW,
            None,
            PAGE_LIMIT,
        )
        .await;

        assert!(outcome.is_none());
        assert!(!home.snapshot().exists());
    }

    #[tokio::test]
    async fn the_provider_emits_its_visits_with_the_cursor_they_advance_it_to() {
        let home = TempHome::new("provider-run");
        let buffer = home.buffer();
        let mut provider = BrowserHistoryProvider::new(home.user_data(), buffer.handle());
        let (out, mut emissions): (_, Receiver<Emission>) = channel(4);

        let ctx = home.ctx();
        let running = tokio::spawn(async move { provider.run(ctx, out).await });

        let Some(Emission {
            records, cursor, ..
        }) = emissions.recv().await
        else {
            panic!("the provider stopped without emitting");
        };
        assert_eq!(visit_ids(&records), vec![901, 902, 903, 904, 905, 906]);

        let Some(Cursor {
            provider,
            key,
            value,
        }) = cursor
        else {
            panic!("the emission carries no cursor");
        };
        assert_eq!(provider, runtime::Provider::BrowserHistory);
        assert_eq!(key, PROFILE);
        let state = HistoryState::decode(&value).expect("the cursor value parses");
        assert_eq!(state.cursor, NEWEST_VISIT);
        assert_eq!(state.generation, 1);

        running.abort();
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_profile_absent_at_startup_stops_the_provider_and_lists_the_names_that_exist() {
        let home = TempHome::new("startup-absent");
        home.write_local_state(r#"{"profile":{"info_cache":{"Profile 1":{"name":"MBA_22"}}}}"#);
        let buffer = home.buffer();
        let mut provider = BrowserHistoryProvider::new(home.user_data(), buffer.handle());
        let (out, _emissions) = channel(4);

        let ProviderError(reason) = provider
            .run(home.ctx(), out)
            .await
            .expect_err("an absent profile must stop the provider");

        assert!(reason.contains(PROFILE), "reason was {reason}");
        assert!(reason.contains("MBA_22"), "reason was {reason}");
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_restarted_provider_resumes_from_the_stored_cursor() {
        let home = TempHome::new("resume");
        let buffer = home.buffer();
        let handle = buffer.handle();

        let state = HistoryState {
            cursor: NEWEST_VISIT,
            generation: 4,
            shipped: BTreeMap::new(),
        };
        handle
            .enqueue(
                Vec::new(),
                Some(Cursor {
                    provider: runtime::Provider::BrowserHistory,
                    key: PROFILE.to_string(),
                    value: state.encode(),
                }),
            )
            .await
            .expect("the cursor could not be stored");

        let resumed = stored_state(&handle, PROFILE)
            .await
            .expect("the stored cursor could not be read");
        assert_eq!(resumed, Some(state));
        assert_eq!(
            stored_state(&handle, "Intapp")
                .await
                .expect("an unknown profile is not an error"),
            None
        );

        buffer.close().await.expect("the buffer did not close");
    }
}
