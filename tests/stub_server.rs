use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PATIENCE: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(25);
const SHUTDOWN_PATIENCE: Duration = Duration::from_secs(15);
const RECORDS_PATH: &str = "/api/v1/records";
const DEVICE: &str = "acceptance-mac";
const PROFILE: &str = "MBP_21";
const FIXTURE_VISITS: usize = 6;
const SQLITE: &str = "/usr/bin/sqlite3";
const KILL: &str = "/bin/kill";

const REDACTED_AWAY: [&str; 5] = [
    "home-v2/home",
    "issue/ENG-1/window-provider",
    "target/coverage/index.html",
    "PGgxPmhlbGxvPC9oMT4",
    "anon/nikki/pull/12",
];

const REDACTED_VISITS: [&str; 6] = [
    "https://homeassistant.pkarpovich.space/",
    "https://linear.app/",
    "file:///",
    "chrome-extension://gighmmpiobklfepjocnamgkkbiglidom/",
    "data:",
    "https://github.com/",
];

#[derive(Debug, Clone)]
enum Answer {
    Accept,
    AcceptWithRejection { index: usize, reason: String },
    Reply { status: u16, body: String },
}

#[derive(Debug, Clone)]
struct Request {
    path: String,
    content_type: String,
    body: String,
}

impl Request {
    fn records(&self) -> Vec<Value> {
        let body: Value = match serde_json::from_str(&self.body) {
            Ok(body) => body,
            Err(error) => panic!("a request body is not json: {error}\n{}", self.body),
        };
        let Some(records) = body["records"].as_array() else {
            panic!("a request body carries no records array: {}", self.body);
        };
        records.clone()
    }

    fn keys(&self) -> HashSet<String> {
        let mut keys = HashSet::new();
        for record in self.records() {
            let Some(key) = record["dedup_key"].as_str() else {
                panic!("a record carries no dedup key: {record}");
            };
            keys.insert(key.to_string());
        }
        keys
    }
}

struct StubState {
    answers: Mutex<VecDeque<Answer>>,
    requests: Mutex<Vec<Request>>,
    running: AtomicBool,
}

impl StubState {
    fn next_answer(&self) -> Answer {
        let mut answers = self.answers.lock().expect("the answer script is poisoned");
        if answers.len() > 1 {
            return answers.pop_front().expect("the script is not empty");
        }
        match answers.front() {
            Some(answer) => answer.clone(),
            None => Answer::Accept,
        }
    }

    fn record(&self, request: Request) {
        self.requests
            .lock()
            .expect("the request log is poisoned")
            .push(request);
    }
}

struct Stub {
    port: u16,
    state: Arc<StubState>,
    server: Option<JoinHandle<()>>,
}

impl Stub {
    fn start(answers: Vec<Answer>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the stub could not take a port");
        let port = listener
            .local_addr()
            .expect("the stub has no address")
            .port();
        listener
            .set_nonblocking(true)
            .expect("the stub could not poll for connections");

        let state = Arc::new(StubState {
            answers: Mutex::new(VecDeque::from(answers)),
            requests: Mutex::new(Vec::new()),
            running: AtomicBool::new(true),
        });
        let served = Arc::clone(&state);
        let server = thread::spawn(move || serve(listener, &served));

        Stub {
            port,
            state,
            server: Some(server),
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<Request> {
        self.state
            .requests
            .lock()
            .expect("the request log is poisoned")
            .clone()
    }

    fn envelopes(&self) -> Vec<Value> {
        let mut envelopes = Vec::new();
        for request in self.requests() {
            for record in request.records() {
                envelopes.push(record);
            }
        }
        envelopes
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.state.running.store(false, Ordering::SeqCst);
        let Some(server) = self.server.take() else {
            return;
        };
        let _ = server.join();
    }
}

fn serve(listener: TcpListener, state: &StubState) {
    while state.running.load(Ordering::SeqCst) {
        let accepted = listener.accept();
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(POLL);
                continue;
            }
            Err(_) => return,
        };
        handle(stream, state);
    }
}

fn handle(mut stream: TcpStream, state: &StubState) {
    let Ok(peer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(peer);

    let mut line = String::new();
    let Ok(read) = reader.read_line(&mut line) else {
        return;
    };
    if read == 0 {
        return;
    }
    let mut words = line.split_whitespace();
    let _method = words.next();
    let path = words.next().unwrap_or_default().to_string();

    let mut length = 0;
    let mut content_type = String::new();
    loop {
        let mut header = String::new();
        let Ok(read) = reader.read_line(&mut header) else {
            return;
        };
        if read == 0 {
            return;
        }
        let header = header.trim();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let name = name.trim().to_lowercase();
        let value = value.trim().to_string();
        if name == "content-length" {
            length = value.parse().unwrap_or_default();
        }
        if name == "content-type" {
            content_type = value;
        }
    }

    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let body = String::from_utf8_lossy(&body).into_owned();

    let answer = state.next_answer();
    let (status, payload) = respond(&answer, &body);
    state.record(Request {
        path,
        content_type,
        body,
    });

    let response = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        reason(status),
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn respond(answer: &Answer, body: &str) -> (u16, String) {
    match answer {
        Answer::Accept => (200, accepted(counted_records(body), Vec::new())),
        Answer::AcceptWithRejection { index, reason } => {
            let rejected = vec![json!({"index": index, "reason": reason})];
            (200, accepted(counted_records(body) - 1, rejected))
        }
        Answer::Reply { status, body } => (*status, body.clone()),
    }
}

fn accepted(count: usize, rejected: Vec<Value>) -> String {
    json!({"accepted": count, "duplicates": 0, "rejected": rejected}).to_string()
}

fn counted_records(body: &str) -> usize {
    let Ok(body) = serde_json::from_str::<Value>(body) else {
        return 0;
    };
    let Some(records) = body["records"].as_array() else {
        return 0;
    };
    records.len()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum History {
    Fixture,
    Absent,
}

struct Settings {
    tick_interval: u64,
    history_poll_interval: u64,
    max_rows: u64,
    history: History,
    events: Vec<&'static str>,
}

impl Settings {
    fn new() -> Settings {
        Settings {
            tick_interval: 3600,
            history_poll_interval: 3600,
            max_rows: 200_000,
            history: History::Absent,
            events: Vec::new(),
        }
    }
}

struct Daemon {
    root: PathBuf,
    home: PathBuf,
    config: PathBuf,
    state_dir: PathBuf,
    events: PathBuf,
    log: PathBuf,
    child: Option<Child>,
}

impl Daemon {
    fn install(name: &str, service_url: &str, settings: &Settings) -> Daemon {
        let root = std::env::temp_dir().join(format!("nikki-acceptance-{name}"));
        let _ = fs::remove_dir_all(&root);

        let home = root.join("home");
        let user_data = home.join("Library/Application Support/Dia/User Data");
        let profile = user_data.join("Default");
        fs::create_dir_all(&profile).expect("the harness home could not be created");
        fs::create_dir_all(root.join("state")).expect("the harness state could not be created");

        fs::copy(
            fixture("dia_local_state.json"),
            user_data.join("Local State"),
        )
        .expect("the local state fixture could not be installed");
        if settings.history == History::Fixture {
            fs::copy(fixture("history_sample.db"), profile.join("History"))
                .expect("the history fixture could not be installed");
        }

        let config = root.join("config.toml");
        fs::write(&config, config_text(service_url, settings))
            .expect("the harness config could not be written");

        let events = root.join("events.tsv");
        let mut script = String::new();
        for line in &settings.events {
            script.push_str(line);
            script.push('\n');
        }
        fs::write(&events, script).expect("the harness event script could not be written");

        Daemon {
            state_dir: root.join("state"),
            log: root.join("nikki.log"),
            root,
            home,
            config,
            events,
            child: None,
        }
    }

    fn start(&mut self) {
        let log = File::create(&self.log).expect("the daemon log could not be created");
        let errors = log
            .try_clone()
            .expect("the daemon log could not be shared with stderr");
        let child = Command::new(env!("CARGO_BIN_EXE_nikki"))
            .env("HOME", &self.home)
            .env("NIKKI_CONFIG", &self.config)
            .env("NIKKI_STATE_DIR", &self.state_dir)
            .env("NIKKI_TEST_EVENTS", &self.events)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("RUST_LOG", "info")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(errors))
            .spawn()
            .expect("the daemon could not be started");
        self.child = Some(child);
    }

    fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = Command::new(KILL)
            .arg("-TERM")
            .arg(child.id().to_string())
            .status();

        let deadline = Instant::now() + SHUTDOWN_PATIENCE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(_) => return,
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            thread::sleep(POLL);
        }
    }

    fn log(&self) -> String {
        let Ok(text) = fs::read_to_string(&self.log) else {
            return String::new();
        };
        plain(&text)
    }

    fn dead_letter_rows(&self) -> usize {
        let database = self.state_dir.join("buffer.db");
        let output = Command::new(SQLITE)
            .arg(&database)
            .arg("SELECT COUNT(*) FROM dead_letter;")
            .output()
            .expect("the dead letter table could not be counted");
        assert!(
            output.status.success(),
            "sqlite3 refused the buffer: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let count = String::from_utf8_lossy(&output.stdout);
        count
            .trim()
            .parse()
            .expect("the dead letter count is not a number")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

fn config_text(service_url: &str, settings: &Settings) -> String {
    let Settings {
        tick_interval,
        history_poll_interval,
        max_rows,
        ..
    } = settings;
    format!(
        "service_url = \"{service_url}\"\n\
         device = \"{DEVICE}\"\n\
         tick_interval = {tick_interval}\n\
         history_poll_interval = {history_poll_interval}\n\
         \n\
         [browser]\n\
         profile = \"{PROFILE}\"\n\
         \n\
         [buffer]\n\
         max_rows = {max_rows}\n\
         max_bytes = 536870912\n"
    )
}

fn plain(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            plain.push(character);
            continue;
        }
        for escape in characters.by_ref() {
            if escape.is_ascii_alphabetic() {
                break;
            }
        }
    }
    plain
}

fn accessibility_granted(log: &str) -> bool {
    for line in log.lines() {
        let Some((_, tail)) = line.split_once("accessibility=") else {
            continue;
        };
        return tail.starts_with("true");
    }
    panic!("the daemon never reported whether accessibility is available:\n{log}");
}

fn wait_for(daemon: &Daemon, what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if ready() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "{what} did not happen within {PATIENCE:?}\n--- daemon log ---\n{}",
                daemon.log()
            );
        }
        thread::sleep(POLL);
    }
}

fn distinct(envelopes: &[Value]) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for envelope in envelopes {
        let Some(key) = envelope["dedup_key"].as_str() else {
            panic!("a record carries no dedup key: {envelope}");
        };
        if !seen.insert(key.to_string()) {
            continue;
        }
        unique.push(envelope.clone());
    }
    unique
}

fn counted(envelopes: &[Value], provider: &str, kind: &str) -> usize {
    let mut count = 0;
    for envelope in envelopes {
        if envelope["provider"] == provider && envelope["kind"] == kind {
            count += 1;
        }
    }
    count
}

fn of_kind<'a>(envelopes: &'a [Value], kind: &str) -> Vec<&'a Value> {
    let mut found = Vec::new();
    for envelope in envelopes {
        if envelope["kind"] == kind {
            found.push(envelope);
        }
    }
    found
}

fn samples(envelopes: &[Value]) -> Vec<&Value> {
    let mut found = Vec::new();
    for envelope in envelopes {
        if envelope["provider"] != "windows" {
            continue;
        }
        let kind = envelope["kind"].as_str().unwrap_or_default();
        if kind == "tick" || kind == "focus" || kind == "state_change" {
            found.push(envelope);
        }
    }
    found
}

fn seqs(envelopes: &[Value]) -> Vec<u64> {
    let mut seqs = Vec::new();
    for envelope in envelopes {
        let Some(seq) = envelope["seq"].as_u64() else {
            panic!("a record carries no sequence number: {envelope}");
        };
        seqs.push(seq);
    }
    seqs
}

fn check_envelope(envelope: &Value) {
    let Some(object) = envelope.as_object() else {
        panic!("a record is not an object: {envelope}");
    };
    let fields = [
        "provider",
        "device",
        "ts",
        "seq",
        "kind",
        "dedup_key",
        "degraded",
        "payload",
    ];
    for field in fields {
        assert!(
            object.contains_key(field),
            "a record is missing `{field}`: {envelope}"
        );
    }
    assert_eq!(
        object.len(),
        fields.len(),
        "a record carries fields the contract does not name: {envelope}"
    );

    assert_eq!(envelope["device"], DEVICE);
    assert!(
        envelope["seq"].is_u64(),
        "`seq` is not a number: {envelope}"
    );
    assert!(
        envelope["degraded"].is_boolean(),
        "`degraded` is not a boolean: {envelope}"
    );
    assert!(
        envelope["payload"].is_object(),
        "`payload` is not an object: {envelope}"
    );
    check_timestamp(&envelope["ts"]);
    check_dedup_key(&envelope["dedup_key"]);
    check_payload(envelope);
}

fn check_timestamp(ts: &Value) {
    let Some(ts) = ts.as_str() else {
        panic!("`ts` is not a string: {ts}");
    };
    assert_eq!(ts.len(), 24, "`ts` is not rfc 3339 with millis: {ts}");
    assert!(ts.ends_with('Z'), "`ts` is not in utc: {ts}");
    let bytes = ts.as_bytes();
    assert_eq!(bytes[10], b'T', "`ts` has no date separator: {ts}");
    assert_eq!(bytes[19], b'.', "`ts` carries no milliseconds: {ts}");
}

fn check_dedup_key(key: &Value) {
    let Some(key) = key.as_str() else {
        panic!("`dedup_key` is not a string: {key}");
    };
    assert_eq!(key.len(), 16, "`dedup_key` is not 16 characters: {key}");
    for character in key.chars() {
        assert!(
            character.is_ascii_hexdigit(),
            "`dedup_key` is not hexadecimal: {key}"
        );
    }
}

fn check_payload(envelope: &Value) {
    let payload = &envelope["payload"];
    let provider = envelope["provider"].as_str().unwrap_or_default();
    let kind = envelope["kind"].as_str().unwrap_or_default();

    match (provider, kind) {
        ("windows", "tick") => {
            require(
                payload,
                &["app", "bundle_id", "display", "visible"],
                envelope,
            );
            require(
                payload,
                &[
                    "tick_interval_sec",
                    "idle_sec",
                    "keys_delta",
                    "mouse_delta",
                    "mic_active",
                ],
                envelope,
            );
            check_visible(payload, envelope);
        }
        ("windows", "focus") | ("windows", "state_change") => {
            require(payload, &["app", "bundle_id", "display"], envelope);
            check_visible(payload, envelope);
        }
        ("windows", "lock")
        | ("windows", "unlock")
        | ("windows", "sleep")
        | ("windows", "wake") => {
            assert_eq!(
                payload,
                &json!({}),
                "a marker carries a payload: {envelope}"
            );
        }
        ("windows", "buffer_overflow") => {
            let details = &payload["details"];
            require(
                details,
                &["dropped", "dropped_from", "dropped_to"],
                envelope,
            );
            assert!(
                details["dropped"].as_u64().unwrap_or_default() > 0,
                "an overflow marker dropped nothing: {envelope}"
            );
        }
        ("browser_history", "visit") => {
            require(payload, &["url", "profile", "visit_id"], envelope);
            assert_eq!(payload["profile"], PROFILE);
        }
        _ => panic!("the contract names no `{provider}` `{kind}` pair: {envelope}"),
    }
}

fn require(payload: &Value, fields: &[&str], envelope: &Value) {
    for field in fields {
        assert!(
            !payload[field].is_null(),
            "a payload is missing `{field}`: {envelope}"
        );
    }
}

fn check_visible(payload: &Value, envelope: &Value) {
    let Some(visible) = payload["visible"].as_array() else {
        panic!("`visible` is not an array: {envelope}");
    };
    for entry in visible {
        let Some(entry) = entry.as_object() else {
            panic!("a visible window is not an object: {envelope}");
        };
        for field in ["app", "bundle_id", "title", "title_reason", "display", "z"] {
            assert!(
                entry.contains_key(field),
                "a visible window is missing `{field}`: {envelope}"
            );
        }
    }
}

fn check_degradation(envelope: &Value) {
    let payload = &envelope["payload"];
    assert!(
        payload["display"].is_u64(),
        "a sample carries no display: {envelope}"
    );
    if envelope["degraded"] != Value::Bool(true) {
        return;
    }
    assert!(
        payload.get("title").is_none(),
        "a degraded sample carries a title: {envelope}"
    );
    assert!(
        payload.get("path").is_none(),
        "a degraded sample carries a path: {envelope}"
    );
    let Some(visible) = payload["visible"].as_array() else {
        return;
    };
    for entry in visible {
        assert!(
            entry["title"].is_null(),
            "a degraded sample carries a visible title: {envelope}"
        );
    }
}

#[test]
fn a_capture_run_matches_the_wire_contract() {
    let stub = Stub::start(vec![Answer::Accept]);
    let mut settings = Settings::new();
    settings.tick_interval = 1;
    settings.history = History::Fixture;
    settings.events = vec!["title_changed\t1"];

    let mut daemon = Daemon::install("wire-contract", &stub.url(), &settings);
    daemon.start();
    wait_for(&daemon, "the stub recorded a full capture", || {
        let envelopes = distinct(&stub.envelopes());
        counted(&envelopes, "windows", "tick") >= 2
            && counted(&envelopes, "windows", "state_change") >= 1
            && counted(&envelopes, "browser_history", "visit") == FIXTURE_VISITS
    });
    daemon.stop();

    for request in stub.requests() {
        assert_eq!(request.path, RECORDS_PATH);
        assert!(
            request.content_type.starts_with("application/json"),
            "the daemon sent `{}`",
            request.content_type
        );
    }

    let envelopes = distinct(&stub.envelopes());
    for envelope in &envelopes {
        check_envelope(envelope);
    }

    for tick in of_kind(&envelopes, "tick") {
        assert_eq!(
            tick["payload"]["tick_interval_sec"], 1,
            "a tick carries the wrong interval: {tick}"
        );
    }

    let changes = of_kind(&envelopes, "state_change");
    let change = changes.first().expect("no state change was recorded");
    assert!(change["payload"]["app"].is_string());
    assert!(change["payload"]["bundle_id"].is_string());
    assert!(change["payload"]["display"].is_u64());

    let mut urls = BTreeSet::new();
    for visit in of_kind(&envelopes, "visit") {
        let Some(url) = visit["payload"]["url"].as_str() else {
            panic!("a visit carries no url: {visit}");
        };
        urls.insert(url.to_string());
    }
    assert_eq!(urls, BTreeSet::from(REDACTED_VISITS.map(String::from)));

    for request in stub.requests() {
        for token in REDACTED_AWAY {
            assert!(
                !request.body.contains(token),
                "a redacted path reached the wire: {token}"
            );
        }
    }

    let granted = accessibility_granted(&daemon.log());
    let samples = samples(&envelopes);
    assert!(!samples.is_empty(), "no window sample was recorded");
    for sample in &samples {
        check_degradation(sample);
    }
    if granted {
        eprintln!(
            "acceptance: accessibility is granted to this process, so the degraded path was not \
             exercised here; src/providers/windows.rs covers it"
        );
        return;
    }
    for sample in &samples {
        assert_eq!(
            sample["degraded"],
            Value::Bool(true),
            "accessibility is unavailable yet a sample is not degraded: {sample}"
        );
    }
}

#[test]
fn the_sequence_number_keeps_climbing_across_a_restart() {
    let stub = Stub::start(vec![Answer::Accept]);
    let mut settings = Settings::new();
    settings.tick_interval = 1;

    let mut daemon = Daemon::install("restart", &stub.url(), &settings);
    daemon.start();
    wait_for(&daemon, "the first run shipped its ticks", || {
        stub.envelopes().len() >= 2
    });
    daemon.stop();
    let first = seqs(&distinct(&stub.envelopes()));

    daemon.start();
    wait_for(&daemon, "the second run shipped its ticks", || {
        distinct(&stub.envelopes()).len() >= first.len() + 2
    });
    daemon.stop();

    let all = seqs(&distinct(&stub.envelopes()));
    let mut previous = 0;
    for seq in &all {
        assert!(
            *seq > previous,
            "the sequence number did not climb: {previous} then {seq}"
        );
        previous = *seq;
    }

    let highest = first.last().expect("the first run shipped nothing");
    let last = all.last().expect("the second run shipped nothing");
    assert!(
        last > highest,
        "the restart did not advance the counter: {highest} then {last}"
    );
}

#[test]
fn a_server_error_keeps_records_buffered_until_it_recovers() {
    let stub = Stub::start(vec![
        Answer::Reply {
            status: 500,
            body: "boom".to_string(),
        },
        Answer::Accept,
    ]);
    let mut settings = Settings::new();
    settings.tick_interval = 1;
    settings.history = History::Fixture;

    let mut daemon = Daemon::install("server-error", &stub.url(), &settings);
    daemon.start();
    wait_for(
        &daemon,
        "the stub answered a failure and a recovery",
        || stub.requests().len() >= 3,
    );
    daemon.stop();

    let requests = stub.requests();
    let refused = requests[0].keys();
    let retried = requests[1].keys();
    assert!(!refused.is_empty(), "the refused batch was empty");
    for key in &refused {
        assert!(
            retried.contains(key),
            "a record refused with 500 was not retried: {key}"
        );
    }

    let drained = requests[2].keys();
    for key in &retried {
        assert!(
            !drained.contains(key),
            "an accepted record was shipped again: {key}"
        );
    }
}

#[test]
fn a_rejection_deletes_the_whole_batch_and_is_logged() {
    let stub = Stub::start(vec![
        Answer::AcceptWithRejection {
            index: 0,
            reason: "unknown kind \"tick\"".to_string(),
        },
        Answer::Accept,
    ]);
    let mut settings = Settings::new();
    settings.tick_interval = 1;
    settings.history = History::Fixture;

    let mut daemon = Daemon::install("rejection", &stub.url(), &settings);
    daemon.start();
    wait_for(
        &daemon,
        "the stub answered a rejection and a clean batch",
        || stub.requests().len() >= 2,
    );
    daemon.stop();

    let requests = stub.requests();
    let rejected = requests[0].keys();
    assert!(!rejected.is_empty(), "the rejected batch was empty");
    for key in &rejected {
        assert!(
            !requests[1].keys().contains(key),
            "a rejected batch was retried instead of deleted: {key}"
        );
    }

    let log = daemon.log();
    assert!(
        log.contains("the service rejected a record permanently"),
        "the rejection was not logged:\n{log}"
    );
    assert!(
        log.contains("unknown kind"),
        "the rejection reason was not logged:\n{log}"
    );
    assert_eq!(daemon.dead_letter_rows(), 0);
}

#[test]
fn a_malformed_two_hundred_is_retried_rather_than_believed() {
    let stub = Stub::start(vec![
        Answer::Reply {
            status: 200,
            body: "<html>proxy</html>".to_string(),
        },
        Answer::Accept,
    ]);
    let mut settings = Settings::new();
    settings.tick_interval = 1;
    settings.history = History::Fixture;

    let mut daemon = Daemon::install("malformed-200", &stub.url(), &settings);
    daemon.start();
    wait_for(
        &daemon,
        "the stub answered a malformed body and a retry",
        || stub.requests().len() >= 2,
    );
    daemon.stop();

    let requests = stub.requests();
    let kept = requests[0].keys();
    assert!(!kept.is_empty(), "the kept batch was empty");
    for key in &kept {
        assert!(
            requests[1].keys().contains(key),
            "a record answered with a malformed 200 was not retried: {key}"
        );
    }
    assert_eq!(daemon.dead_letter_rows(), 0);
}

#[test]
fn a_bad_request_dead_letters_the_batch_and_shipping_continues() {
    let stub = Stub::start(vec![
        Answer::Reply {
            status: 400,
            body: "malformed".to_string(),
        },
        Answer::Accept,
    ]);
    let mut settings = Settings::new();
    settings.tick_interval = 1;
    settings.history = History::Fixture;

    let mut daemon = Daemon::install("bad-request", &stub.url(), &settings);
    daemon.start();
    wait_for(
        &daemon,
        "the stub answered a rejection and a clean batch",
        || stub.requests().len() >= 2,
    );
    daemon.stop();

    let requests = stub.requests();
    let doomed = requests[0].keys();
    assert!(!doomed.is_empty(), "the dead lettered batch was empty");
    for key in &doomed {
        assert!(
            !requests[1].keys().contains(key),
            "a dead lettered record was shipped again: {key}"
        );
    }
    assert!(
        !requests[1].keys().is_empty(),
        "shipping did not continue after the dead letter"
    );
    assert_eq!(daemon.dead_letter_rows(), doomed.len());
}

#[test]
fn a_not_found_keeps_the_batch_rather_than_dead_lettering_it() {
    let stub = Stub::start(vec![
        Answer::Reply {
            status: 404,
            body: "not found".to_string(),
        },
        Answer::Accept,
    ]);
    let mut settings = Settings::new();
    settings.tick_interval = 1;
    settings.history = History::Fixture;

    let mut daemon = Daemon::install("not-found", &stub.url(), &settings);
    daemon.start();
    wait_for(&daemon, "the stub answered a 404 and a retry", || {
        stub.requests().len() >= 2
    });
    daemon.stop();

    let requests = stub.requests();
    let kept = requests[0].keys();
    assert!(!kept.is_empty(), "the kept batch was empty");
    for key in &kept {
        assert!(
            requests[1].keys().contains(key),
            "a record answered with a 404 was not retried: {key}"
        );
    }
    assert_eq!(daemon.dead_letter_rows(), 0);
}

#[test]
fn a_full_buffer_drops_its_oldest_records_and_says_so() {
    let stub = Stub::start(vec![
        Answer::Reply {
            status: 500,
            body: "boom".to_string(),
        },
        Answer::Accept,
    ]);
    let mut settings = Settings::new();
    settings.max_rows = 5;
    settings.events = vec![
        "screen_locked",
        "screen_unlocked",
        "screen_locked",
        "screen_unlocked",
        "screen_locked",
        "screen_unlocked",
    ];

    let mut daemon = Daemon::install("overflow", &stub.url(), &settings);
    daemon.start();
    wait_for(&daemon, "the overflow marker reached the stub", || {
        counted(&stub.envelopes(), "windows", "buffer_overflow") >= 1
    });
    daemon.stop();

    let envelopes = distinct(&stub.envelopes());
    for envelope in &envelopes {
        check_envelope(envelope);
    }

    let markers = of_kind(&envelopes, "buffer_overflow");
    assert_eq!(markers.len(), 1, "the overflow was reported more than once");
    let marker = markers[0];
    assert!(
        marker["seq"].as_u64().unwrap_or_default() > 0,
        "the overflow marker carries no sequence number: {marker}"
    );
    assert_eq!(marker["payload"]["details"]["dropped"], 2);

    let shipped = BTreeSet::from_iter(seqs(&envelopes));
    assert_eq!(
        shipped,
        BTreeSet::from([3, 4, 5, 6, 7]),
        "the buffer did not drop exactly its two oldest records"
    );
}
