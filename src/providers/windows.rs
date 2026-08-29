use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::mpsc::{Sender, UnboundedReceiver};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until, timeout};

use super::{Ctx, Emission, Provider, ProviderError};
use crate::extract::document::document_path;
use crate::extract::{Details, details_for_focused};
use crate::macos::activity::{
    InputCounterTracker, InputCounters, InputDelta, cursor_display_index, idle_seconds,
    input_counters, microphone_active,
};
use crate::macos::ax::{AxApplication, accessibility_is_trusted};
use crate::macos::events::{
    MacEvent, Observed, RescanHandle, SLEEP_FLUSH_BUDGET, SleepAcknowledgement,
};
use crate::macos::screen::{displays_asleep, screen_locked};
use crate::macos::window_list::{
    DisplayEntry, RunningApplication, WindowEntry, bundle_id_for_pid, display_list,
    frontmost_application, is_lock_screen, window_list,
};
use crate::runtime::{self, KeySource, Kind, RecordDraft, Timestamp};
use crate::window::visibility::{VisibleWindow, visible_windows};

pub const DEBOUNCE: Duration = Duration::from_millis(300);
pub const DEBOUNCE_CEILING: Duration = Duration::from_secs(1);
pub const AMBIGUOUS: &str = "ambiguous";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusedWindow {
    Window {
        title: Option<String>,
        path: Option<String>,
    },
    Absent,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowTitle {
    Sole(Option<String>),
    Ambiguous,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Activity {
    pub idle_sec: i64,
    pub counters: InputCounters,
    pub mic_active: bool,
    pub screen_locked: bool,
    pub display_asleep: bool,
}

pub trait Sources: Send + Sync + 'static {
    fn windows(&self) -> Vec<WindowEntry>;
    fn displays(&self) -> Vec<DisplayEntry>;
    fn frontmost(&self) -> Option<RunningApplication>;
    fn bundle_id(&self, pid: i32) -> Option<String>;
    fn cursor_display(&self, displays: &[DisplayEntry]) -> Option<usize>;
    fn focused_window(&self, pid: i32) -> FocusedWindow;
    fn window_title(&self, pid: i32) -> WindowTitle;
    fn activity(&self) -> Activity;
    fn rescan_observers(&self);
    fn details(&self, bundle_id: &str) -> impl Future<Output = Details> + Send;
}

pub struct MacSources {
    rescan: RescanHandle,
}

impl MacSources {
    pub fn new(rescan: RescanHandle) -> Self {
        Self { rescan }
    }
}

impl Sources for MacSources {
    fn windows(&self) -> Vec<WindowEntry> {
        window_list()
    }

    fn displays(&self) -> Vec<DisplayEntry> {
        display_list()
    }

    fn frontmost(&self) -> Option<RunningApplication> {
        frontmost_application()
    }

    fn bundle_id(&self, pid: i32) -> Option<String> {
        bundle_id_for_pid(pid)
    }

    fn cursor_display(&self, displays: &[DisplayEntry]) -> Option<usize> {
        cursor_display_index(displays)
    }

    fn focused_window(&self, pid: i32) -> FocusedWindow {
        if !accessibility_is_trusted() {
            return FocusedWindow::Unavailable;
        }
        let application = AxApplication::for_pid(pid);
        let Some(window) = application.focused_window() else {
            return FocusedWindow::Absent;
        };
        FocusedWindow::Window {
            title: window.title(),
            path: document_path(&window),
        }
    }

    fn window_title(&self, pid: i32) -> WindowTitle {
        if !accessibility_is_trusted() {
            return WindowTitle::Unavailable;
        }
        let mut windows = AxApplication::for_pid(pid).windows();
        if windows.len() != 1 {
            return WindowTitle::Ambiguous;
        }
        WindowTitle::Sole(windows.remove(0).title())
    }

    fn activity(&self) -> Activity {
        Activity {
            idle_sec: idle_seconds(),
            counters: input_counters(),
            mic_active: microphone_active(),
            screen_locked: screen_locked(),
            display_asleep: displays_asleep(),
        }
    }

    fn rescan_observers(&self) {
        self.rescan.request();
    }

    async fn details(&self, bundle_id: &str) -> Details {
        details_for_focused(bundle_id).await
    }
}

pub struct WindowProvider<S> {
    sources: S,
    events: UnboundedReceiver<Observed>,
    counters: InputCounterTracker,
}

impl<S: Sources> WindowProvider<S> {
    pub fn new(sources: S, events: UnboundedReceiver<Observed>) -> WindowProvider<S> {
        WindowProvider {
            sources,
            events,
            counters: InputCounterTracker::new(),
        }
    }
}

impl<S: Sources> Provider for WindowProvider<S> {
    fn name(&self) -> &'static str {
        runtime::Provider::Windows.as_str()
    }

    async fn run(&mut self, ctx: Ctx, out: Sender<Emission>) -> Result<(), ProviderError> {
        let WindowProvider {
            sources,
            events,
            counters,
        } = self;
        let tick_interval = ctx.config.tick_interval;
        let mut ticker = interval(Duration::from_secs(tick_interval));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker.tick().await;

        let mut pending: Option<Pending> = None;
        loop {
            let deadline = pending.as_ref().map(|Pending { deadline, .. }| *deadline);

            tokio::select! {
                event = events.recv() => {
                    let Some(Observed { at, event }) = event else {
                        tracing::info!("the event thread is gone, so the window provider stops");
                        return Ok(());
                    };
                    match signal(event) {
                        Signal::Sample { kind, application } => {
                            pending = Some(schedule(pending, kind, application, at));
                        }
                        Signal::Marker { kind } => {
                            if out.send(Emission::new(vec![marker(kind)])).await.is_err() {
                                return Ok(());
                            }
                        }
                        Signal::Sleep { acknowledgement } => {
                            deliver_sleep(&out, acknowledgement).await;
                        }
                    }
                }
                _ = ticker.tick() => {
                    sources.rescan_observers();
                    let activity = sources.activity();
                    let Some(sample) = assemble(sources, None).await else {
                        continue;
                    };
                    let delta = counters.advance(activity.counters);
                    let record = tick_record(sample, tick_interval, activity, delta);
                    if out.send(Emission::new(vec![record])).await.is_err() {
                        return Ok(());
                    }
                }
                _ = wait_until(deadline) => {
                    let Some(Pending { kind, ts, application, .. }) = pending.take() else {
                        continue;
                    };
                    let Some(sample) = assemble(sources, application).await else {
                        continue;
                    };
                    if out.send(Emission::new(vec![sample_record(kind, ts, sample)])).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

struct Pending {
    kind: Kind,
    ts: Timestamp,
    application: Option<RunningApplication>,
    first: Instant,
    deadline: Instant,
}

enum Signal {
    Sample {
        kind: Kind,
        application: Option<RunningApplication>,
    },
    Marker {
        kind: Kind,
    },
    Sleep {
        acknowledgement: SleepAcknowledgement,
    },
}

struct Sample {
    app: String,
    bundle_id: String,
    title: Option<String>,
    path: Option<String>,
    details: Details,
    display: usize,
    visible: Vec<Value>,
    degraded: bool,
}

fn signal(event: MacEvent) -> Signal {
    match event {
        MacEvent::ApplicationActivated { application } => Signal::Sample {
            kind: Kind::Focus,
            application: Some(application),
        },
        MacEvent::FocusedWindowChanged { pid: _ }
        | MacEvent::TitleChanged { pid: _ }
        | MacEvent::WindowCreated { pid: _ }
        | MacEvent::WindowDestroyed { pid: _ }
        | MacEvent::DisplaysReconfigured => Signal::Sample {
            kind: Kind::StateChange,
            application: None,
        },
        MacEvent::ScreenLocked => Signal::Marker { kind: Kind::Lock },
        MacEvent::ScreenUnlocked => Signal::Marker { kind: Kind::Unlock },
        MacEvent::DidWake => Signal::Marker { kind: Kind::Wake },
        MacEvent::WillSleep { acknowledgement } => Signal::Sleep { acknowledgement },
    }
}

fn schedule(
    pending: Option<Pending>,
    kind: Kind,
    application: Option<RunningApplication>,
    ts: Timestamp,
) -> Pending {
    let now = Instant::now();
    let Some(pending) = pending else {
        return Pending {
            kind,
            ts,
            application,
            first: now,
            deadline: now + DEBOUNCE,
        };
    };
    let Pending {
        kind: scheduled,
        ts: scheduled_ts,
        application: earlier,
        first,
        ..
    } = pending;
    let activation = kind == Kind::Focus;
    let kind = if activation || scheduled == Kind::Focus {
        Kind::Focus
    } else {
        Kind::StateChange
    };
    let ts = if activation { ts } else { scheduled_ts };
    Pending {
        kind,
        ts,
        application: application.or(earlier),
        first,
        deadline: (now + DEBOUNCE).min(first + DEBOUNCE_CEILING),
    }
}

async fn wait_until(deadline: Option<Instant>) {
    let Some(deadline) = deadline else {
        std::future::pending::<()>().await;
        return;
    };
    sleep_until(deadline).await;
}

async fn deliver_sleep(out: &Sender<Emission>, acknowledgement: SleepAcknowledgement) {
    let (emission, committed) = Emission::awaiting_commit(vec![marker(Kind::Sleep)], None);
    if out.send(emission).await.is_err() {
        acknowledgement.acknowledge();
        return;
    }
    match timeout(SLEEP_FLUSH_BUDGET, committed).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => tracing::error!("the sleep record was not buffered"),
        Err(_) => tracing::warn!("the sleep record was not committed within the budget"),
    }
    acknowledgement.acknowledge();
}

async fn assemble<S: Sources>(
    sources: &S,
    activated: Option<RunningApplication>,
) -> Option<Sample> {
    let windows = sources.windows();
    let displays = sources.displays();
    let visible = visible_windows(&windows, &displays);

    let application = match activated {
        Some(application) if !is_lock_screen(&application) => Some(application),
        _ => sources.frontmost(),
    };
    let Some(RunningApplication {
        pid,
        name,
        bundle_id,
    }) = application
    else {
        tracing::debug!("there is no frontmost application, so no sample is assembled");
        return None;
    };

    let front = front_window(&windows, pid);
    let app = match name {
        Some(name) => name,
        None => match front {
            Some(WindowEntry { owner_name, .. }) => owner_name.clone(),
            None => String::new(),
        },
    };
    let bundle_id = match bundle_id {
        Some(bundle_id) => bundle_id,
        None => sources.bundle_id(pid).unwrap_or_default(),
    };

    let focused = sources.focused_window(pid);
    let degraded = match &focused {
        FocusedWindow::Window { .. } => false,
        FocusedWindow::Absent => front.is_some(),
        FocusedWindow::Unavailable => true,
    };
    let (title, path) = match focused {
        FocusedWindow::Window { title, path } => (title, path),
        FocusedWindow::Absent | FocusedWindow::Unavailable => (None, None),
    };

    let display = match display_for(&visible, pid) {
        Some(display) => display,
        None => sources.cursor_display(&displays).unwrap_or_default(),
    };

    let details = match bundle_id.is_empty() {
        true => Details::new(),
        false => sources.details(&bundle_id).await,
    };

    let focused_window = front.map(|WindowEntry { window_number, .. }| *window_number);
    let visible = visible_entries(sources, &visible, pid, focused_window, title.as_deref());

    Some(Sample {
        app,
        bundle_id,
        title,
        path,
        details,
        display,
        visible,
        degraded,
    })
}

fn front_window(windows: &[WindowEntry], pid: i32) -> Option<&WindowEntry> {
    for window in windows {
        let WindowEntry {
            owner_pid, layer, ..
        } = window;
        if *layer == 0 && *owner_pid == pid {
            return Some(window);
        }
    }
    None
}

fn display_for(visible: &[VisibleWindow], pid: i32) -> Option<usize> {
    for VisibleWindow { window, display } in visible {
        let WindowEntry { owner_pid, .. } = window;
        if *owner_pid == pid {
            return Some(*display);
        }
    }
    None
}

fn visible_entries<S: Sources>(
    sources: &S,
    visible: &[VisibleWindow],
    focused_pid: i32,
    focused_window: Option<u32>,
    focused_title: Option<&str>,
) -> Vec<Value> {
    let mut bundles: HashMap<i32, Option<String>> = HashMap::new();
    let mut titles: HashMap<i32, WindowTitle> = HashMap::new();
    let mut entries = Vec::with_capacity(visible.len());

    for VisibleWindow { window, display } in visible {
        let WindowEntry {
            owner_pid,
            owner_name,
            window_number,
            z,
            ..
        } = window;
        let bundle_id = bundles
            .entry(*owner_pid)
            .or_insert_with(|| sources.bundle_id(*owner_pid))
            .clone();

        let focused = *owner_pid == focused_pid && Some(*window_number) == focused_window;
        let (title, reason) = match focused {
            true => (focused_title.map(str::to_string), None),
            false => {
                let lookup = titles
                    .entry(*owner_pid)
                    .or_insert_with(|| sources.window_title(*owner_pid));
                match lookup {
                    WindowTitle::Sole(title) => (title.clone(), None),
                    WindowTitle::Ambiguous => (None, Some(AMBIGUOUS)),
                    WindowTitle::Unavailable => (None, None),
                }
            }
        };

        entries.push(json!({
            "app": owner_name,
            "bundle_id": bundle_id,
            "title": title,
            "title_reason": reason,
            "display": display,
            "z": z,
        }));
    }
    entries
}

fn marker(kind: Kind) -> RecordDraft {
    RecordDraft {
        provider: runtime::Provider::Windows,
        kind,
        ts: Timestamp::now(),
        degraded: false,
        payload: json!({}),
        key: KeySource::Windows,
    }
}

fn sample_record(kind: Kind, ts: Timestamp, sample: Sample) -> RecordDraft {
    let degraded = sample.degraded;
    RecordDraft {
        provider: runtime::Provider::Windows,
        kind,
        ts,
        degraded,
        payload: Value::Object(base_payload(sample)),
        key: KeySource::Windows,
    }
}

fn tick_record(
    sample: Sample,
    tick_interval: u64,
    activity: Activity,
    delta: InputDelta,
) -> RecordDraft {
    let degraded = sample.degraded;
    let Activity {
        idle_sec,
        counters: _,
        mic_active,
        screen_locked,
        display_asleep,
    } = activity;
    let InputDelta { keys, mouse } = delta;

    let mut payload = base_payload(sample);
    payload.insert("tick_interval_sec".to_string(), json!(tick_interval));
    payload.insert("idle_sec".to_string(), json!(idle_sec));
    payload.insert("keys_delta".to_string(), json!(keys));
    payload.insert("mouse_delta".to_string(), json!(mouse));
    payload.insert("mic_active".to_string(), json!(mic_active));
    payload.insert("screen_locked".to_string(), json!(screen_locked));
    payload.insert("display_asleep".to_string(), json!(display_asleep));

    RecordDraft {
        provider: runtime::Provider::Windows,
        kind: Kind::Tick,
        ts: Timestamp::now(),
        degraded,
        payload: Value::Object(payload),
        key: KeySource::Windows,
    }
}

fn base_payload(sample: Sample) -> Map<String, Value> {
    let Sample {
        app,
        bundle_id,
        title,
        path,
        details,
        display,
        visible,
        degraded: _,
    } = sample;

    let mut payload = Map::new();
    payload.insert("app".to_string(), Value::String(app));
    payload.insert("bundle_id".to_string(), Value::String(bundle_id));
    if let Some(title) = title {
        payload.insert("title".to_string(), Value::String(title));
    }
    if let Some(path) = path {
        payload.insert("path".to_string(), Value::String(path));
    }
    if !details.is_empty() {
        payload.insert("details".to_string(), Value::Object(details));
    }
    payload.insert("display".to_string(), json!(display));
    payload.insert("visible".to_string(), Value::Array(visible));
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc::{Receiver, UnboundedSender, channel, unbounded_channel};

    use crate::macos::events::{AxNotification, ax_event};
    use crate::macos::window_list::Rect;
    use crate::providers::tests::test_ctx;

    const ZED_PID: i32 = 501;
    const DIA_PID: i32 = 502;
    const TELEGRAM_PID: i32 = 503;
    const LOGIN_WINDOW_PID: i32 = 504;
    const LONG_TICK: u64 = 3600;

    #[derive(Clone)]
    struct FakeSources {
        windows: Vec<WindowEntry>,
        displays: Vec<DisplayEntry>,
        frontmost: Arc<Mutex<Option<RunningApplication>>>,
        bundle_ids: HashMap<i32, String>,
        cursor_display: Option<usize>,
        focused: FocusedWindow,
        titles: HashMap<i32, WindowTitle>,
        activity: Arc<Mutex<Activity>>,
        details: Details,
        detail_calls: Arc<Mutex<Vec<String>>>,
        title_calls: Arc<AtomicUsize>,
        rescans: Arc<AtomicUsize>,
    }

    impl Sources for FakeSources {
        fn windows(&self) -> Vec<WindowEntry> {
            self.windows.clone()
        }

        fn displays(&self) -> Vec<DisplayEntry> {
            self.displays.clone()
        }

        fn frontmost(&self) -> Option<RunningApplication> {
            self.frontmost
                .lock()
                .expect("the frontmost application is poisoned")
                .clone()
        }

        fn bundle_id(&self, pid: i32) -> Option<String> {
            self.bundle_ids.get(&pid).cloned()
        }

        fn cursor_display(&self, _displays: &[DisplayEntry]) -> Option<usize> {
            self.cursor_display
        }

        fn focused_window(&self, _pid: i32) -> FocusedWindow {
            self.focused.clone()
        }

        fn window_title(&self, pid: i32) -> WindowTitle {
            self.title_calls.fetch_add(1, Ordering::SeqCst);
            match self.titles.get(&pid) {
                Some(title) => title.clone(),
                None => WindowTitle::Ambiguous,
            }
        }

        fn activity(&self) -> Activity {
            *self.activity.lock().expect("the activity is poisoned")
        }

        fn rescan_observers(&self) {
            self.rescans.fetch_add(1, Ordering::SeqCst);
        }

        async fn details(&self, bundle_id: &str) -> Details {
            self.detail_calls
                .lock()
                .expect("the extractor log is poisoned")
                .push(bundle_id.to_string());
            self.details.clone()
        }
    }

    fn window(pid: i32, name: &str, number: u32, z: usize, bounds: Rect) -> WindowEntry {
        WindowEntry {
            owner_pid: pid,
            owner_name: name.to_string(),
            window_number: number,
            bounds,
            layer: 0,
            z,
        }
    }

    fn no_application(_pid: i32) -> Option<RunningApplication> {
        None
    }

    fn application(pid: i32, name: &str, bundle_id: &str) -> RunningApplication {
        RunningApplication {
            pid,
            name: Some(name.to_string()),
            bundle_id: Some(bundle_id.to_string()),
        }
    }

    fn sources() -> FakeSources {
        let mut bundle_ids = HashMap::new();
        bundle_ids.insert(ZED_PID, "dev.zed.Zed".to_string());
        bundle_ids.insert(DIA_PID, "company.thebrowser.dia".to_string());
        bundle_ids.insert(TELEGRAM_PID, "ru.keepcoder.Telegram".to_string());

        let mut titles = HashMap::new();
        titles.insert(
            DIA_PID,
            WindowTitle::Sole(Some("Home – Home Assistant".to_string())),
        );
        titles.insert(TELEGRAM_PID, WindowTitle::Ambiguous);

        FakeSources {
            windows: vec![
                window(ZED_PID, "Zed", 900, 0, Rect::new(0.0, 0.0, 1000.0, 800.0)),
                window(
                    DIA_PID,
                    "Dia",
                    901,
                    1,
                    Rect::new(2000.0, 0.0, 1000.0, 800.0),
                ),
                window(
                    TELEGRAM_PID,
                    "Telegram",
                    902,
                    4,
                    Rect::new(2200.0, 100.0, 400.0, 600.0),
                ),
            ],
            displays: vec![
                DisplayEntry {
                    index: 0,
                    bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
                },
                DisplayEntry {
                    index: 1,
                    bounds: Rect::new(1920.0, 0.0, 1920.0, 1080.0),
                },
            ],
            frontmost: Arc::new(Mutex::new(Some(application(ZED_PID, "Zed", "dev.zed.Zed")))),
            bundle_ids,
            cursor_display: Some(0),
            focused: FocusedWindow::Window {
                title: Some("nikki — windows.rs".to_string()),
                path: Some("file:///Users/pavel.karpovich/Projects/nikki/src/main.rs".to_string()),
            },
            titles,
            activity: Arc::new(Mutex::new(Activity {
                idle_sec: 3,
                counters: InputCounters {
                    keys: 900_000,
                    mouse: 40_000,
                },
                mic_active: false,
                screen_locked: false,
                display_asleep: false,
            })),
            details: Details::new(),
            detail_calls: Arc::new(Mutex::new(Vec::new())),
            title_calls: Arc::new(AtomicUsize::new(0)),
            rescans: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn start(
        sources: FakeSources,
        tick_interval: u64,
    ) -> (UnboundedSender<Observed>, Receiver<Emission>) {
        let (events, inbox) = unbounded_channel();
        let (out, emissions) = channel(16);
        let mut provider = WindowProvider::new(sources, inbox);
        tokio::spawn(async move { provider.run(test_ctx(tick_interval), out).await });
        (events, emissions)
    }

    fn send(events: &UnboundedSender<Observed>, event: MacEvent) {
        events
            .send(Observed::now(event))
            .expect("the provider is listening");
    }

    fn observed_at(events: &UnboundedSender<Observed>, at: Timestamp, event: MacEvent) {
        events
            .send(Observed { at, event })
            .expect("the provider is listening");
    }

    async fn one_record(emissions: &mut Receiver<Emission>) -> RecordDraft {
        let Some(Emission { mut records, .. }) = emissions.recv().await else {
            panic!("the provider stopped without emitting");
        };
        assert_eq!(records.len(), 1);
        records.remove(0)
    }

    #[tokio::test(start_paused = true)]
    async fn a_tick_carries_the_interval_the_activity_and_the_visible_set() {
        let (_events, mut emissions) = start(sources(), 30);

        let RecordDraft {
            provider,
            kind,
            degraded,
            payload,
            key,
            ..
        } = one_record(&mut emissions).await;

        assert_eq!(provider, runtime::Provider::Windows);
        assert_eq!(kind, Kind::Tick);
        assert!(!degraded);
        assert_eq!(key, KeySource::Windows);
        assert_eq!(payload["app"], "Zed");
        assert_eq!(payload["bundle_id"], "dev.zed.Zed");
        assert_eq!(payload["title"], "nikki — windows.rs");
        assert_eq!(
            payload["path"],
            "file:///Users/pavel.karpovich/Projects/nikki/src/main.rs"
        );
        assert_eq!(payload["display"], 0);
        assert_eq!(payload["tick_interval_sec"], 30);
        assert_eq!(payload["idle_sec"], 3);
        assert_eq!(payload["mic_active"], false);
        assert_eq!(payload["screen_locked"], false);
        assert_eq!(payload["display_asleep"], false);
        assert_eq!(
            payload["visible"].as_array().expect("a visible set").len(),
            3
        );
    }

    #[tokio::test(start_paused = true)]
    async fn every_tick_asks_for_the_observed_applications_to_be_rescanned() {
        let sources = sources();
        let rescans = Arc::clone(&sources.rescans);
        let (_events, mut emissions) = start(sources, 1);

        one_record(&mut emissions).await;
        assert_eq!(rescans.load(Ordering::SeqCst), 1);

        one_record(&mut emissions).await;
        assert_eq!(rescans.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn an_event_alone_never_rescans_the_observed_applications() {
        let sources = sources();
        let rescans = Arc::clone(&sources.rescans);
        let (events, mut emissions) = start(sources, LONG_TICK);

        send(&events, MacEvent::TitleChanged { pid: ZED_PID });

        one_record(&mut emissions).await;
        assert_eq!(rescans.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_tick_reports_no_delta_and_the_next_reports_the_difference() {
        let sources = sources();
        let activity = Arc::clone(&sources.activity);
        let (_events, mut emissions) = start(sources, 1);

        let RecordDraft { payload, .. } = one_record(&mut emissions).await;
        assert_eq!(payload["keys_delta"], 0);
        assert_eq!(payload["mouse_delta"], 0);

        *activity.lock().expect("the activity is poisoned") = Activity {
            idle_sec: 0,
            counters: InputCounters {
                keys: 900_184,
                mouse: 40_022,
            },
            mic_active: true,
            screen_locked: false,
            display_asleep: false,
        };

        let RecordDraft { payload, .. } = one_record(&mut emissions).await;
        assert_eq!(payload["keys_delta"], 184);
        assert_eq!(payload["mouse_delta"], 22);
        assert_eq!(payload["mic_active"], true);
    }

    #[tokio::test(start_paused = true)]
    async fn a_tick_that_assembles_nothing_leaves_its_input_delta_to_the_next_one() {
        let sources = sources();
        let activity = Arc::clone(&sources.activity);
        let frontmost = Arc::clone(&sources.frontmost);
        let (_events, mut emissions) = start(sources, 1);

        let RecordDraft { payload, .. } = one_record(&mut emissions).await;
        assert_eq!(payload["keys_delta"], 0);

        *frontmost
            .lock()
            .expect("the frontmost application is poisoned") = None;
        *activity.lock().expect("the activity is poisoned") = Activity {
            idle_sec: 0,
            counters: InputCounters {
                keys: 900_100,
                mouse: 40_010,
            },
            mic_active: false,
            screen_locked: false,
            display_asleep: false,
        };
        tokio::time::sleep(Duration::from_secs(3)).await;

        *frontmost
            .lock()
            .expect("the frontmost application is poisoned") =
            Some(application(ZED_PID, "Zed", "dev.zed.Zed"));
        *activity.lock().expect("the activity is poisoned") = Activity {
            idle_sec: 0,
            counters: InputCounters {
                keys: 900_150,
                mouse: 40_015,
            },
            mic_active: false,
            screen_locked: false,
            display_asleep: false,
        };

        let RecordDraft { payload, .. } = one_record(&mut emissions).await;
        assert_eq!(payload["keys_delta"], 150);
        assert_eq!(payload["mouse_delta"], 15);
    }

    #[tokio::test(start_paused = true)]
    async fn a_tick_reads_the_screen_state_its_source_reports() {
        let sources = sources();
        let activity = Arc::clone(&sources.activity);
        *activity.lock().expect("the activity is poisoned") = Activity {
            idle_sec: 600,
            counters: InputCounters {
                keys: 900_000,
                mouse: 40_000,
            },
            mic_active: false,
            screen_locked: true,
            display_asleep: true,
        };
        let (_events, mut emissions) = start(sources, 30);

        let RecordDraft { kind, payload, .. } = one_record(&mut emissions).await;
        assert_eq!(kind, Kind::Tick);
        assert_eq!(payload["screen_locked"], true);
        assert_eq!(payload["display_asleep"], true);
    }

    #[tokio::test(start_paused = true)]
    async fn a_locked_session_with_a_lit_panel_reports_the_lock_alone() {
        let sources = sources();
        let activity = Arc::clone(&sources.activity);
        *activity.lock().expect("the activity is poisoned") = Activity {
            idle_sec: 30,
            counters: InputCounters {
                keys: 900_000,
                mouse: 40_000,
            },
            mic_active: false,
            screen_locked: true,
            display_asleep: false,
        };
        let (_events, mut emissions) = start(sources, 30);

        let RecordDraft { payload, .. } = one_record(&mut emissions).await;
        assert_eq!(payload["screen_locked"], true);
        assert_eq!(payload["display_asleep"], false);
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_never_pushes_the_deadline_past_the_ceiling() {
        let mut pending = schedule(None, Kind::StateChange, None, Timestamp::now());
        let ceiling = pending.first + DEBOUNCE_CEILING;

        for _ in 0..10 {
            tokio::time::advance(Duration::from_millis(200)).await;
            pending = schedule(Some(pending), Kind::StateChange, None, Timestamp::now());
            assert!(pending.deadline <= ceiling);
        }
        assert_eq!(pending.deadline, ceiling);
    }

    #[tokio::test(start_paused = true)]
    async fn an_activation_emits_a_focus_record_for_the_application_it_carried() {
        let (events, mut emissions) = start(sources(), LONG_TICK);

        send(
            &events,
            MacEvent::ApplicationActivated {
                application: application(DIA_PID, "Dia", "company.thebrowser.dia"),
            },
        );

        let RecordDraft { kind, payload, .. } = one_record(&mut emissions).await;
        assert_eq!(kind, Kind::Focus);
        assert_eq!(payload["app"], "Dia");
        assert_eq!(payload["bundle_id"], "company.thebrowser.dia");
        assert_eq!(payload["display"], 1);
        assert!(payload.get("screen_locked").is_none());
        assert!(payload.get("display_asleep").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_focus_record_carries_the_moment_the_switch_was_observed_not_the_moment_it_was_read()
    {
        let (events, mut emissions) = start(sources(), LONG_TICK);
        let switched = Timestamp::from_millis(1_756_000_000_000);

        observed_at(
            &events,
            switched,
            MacEvent::ApplicationActivated {
                application: application(DIA_PID, "Dia", "company.thebrowser.dia"),
            },
        );

        let RecordDraft { kind, ts, .. } = one_record(&mut emissions).await;
        assert_eq!(kind, Kind::Focus);
        assert_eq!(ts, switched);
    }

    #[tokio::test(start_paused = true)]
    async fn a_deactivation_and_an_activation_twenty_milliseconds_apart_emit_one_focus_record() {
        let (events, mut emissions) = start(sources(), LONG_TICK);
        let dia = application(DIA_PID, "Dia", "company.thebrowser.dia");
        let named = |pid: i32| match pid == DIA_PID {
            true => Some(dia.clone()),
            false => None,
        };

        assert!(ax_event(AxNotification::ApplicationDeactivated, ZED_PID, named).is_none());
        tokio::time::sleep(Duration::from_millis(20)).await;
        let event = ax_event(AxNotification::ApplicationActivated, DIA_PID, named)
            .expect("an activation describes an event");
        send(&events, event);

        let RecordDraft { kind, payload, .. } = one_record(&mut emissions).await;
        assert_eq!(kind, Kind::Focus);
        assert_eq!(payload["app"], "Dia");
        assert_eq!(payload["bundle_id"], "company.thebrowser.dia");
        assert!(emissions.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn an_activation_of_the_lock_screen_reports_what_is_still_on_screen() {
        let (events, mut emissions) = start(sources(), LONG_TICK);

        send(
            &events,
            MacEvent::ApplicationActivated {
                application: application(LOGIN_WINDOW_PID, "loginwindow", "com.apple.loginwindow"),
            },
        );

        let RecordDraft { kind, payload, .. } = one_record(&mut emissions).await;
        assert_eq!(kind, Kind::Focus);
        assert_eq!(payload["app"], "Zed");
        assert_eq!(payload["bundle_id"], "dev.zed.Zed");
    }

    #[tokio::test(start_paused = true)]
    async fn a_title_change_on_the_focused_application_is_a_state_change_and_never_a_focus() {
        let (events, mut emissions) = start(sources(), LONG_TICK);

        let event = ax_event(AxNotification::TitleChanged, ZED_PID, no_application)
            .expect("a title change describes an event");
        send(&events, event);

        let RecordDraft { kind, payload, .. } = one_record(&mut emissions).await;
        assert_eq!(kind, Kind::StateChange);
        assert_eq!(payload["app"], "Zed");
        assert_eq!(payload["title"], "nikki — windows.rs");
        assert!(emissions.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn every_window_event_emits_a_state_change() {
        let events = [
            MacEvent::FocusedWindowChanged { pid: ZED_PID },
            MacEvent::TitleChanged { pid: ZED_PID },
            MacEvent::WindowCreated { pid: ZED_PID },
            MacEvent::WindowDestroyed { pid: ZED_PID },
            MacEvent::DisplaysReconfigured,
        ];
        for event in events {
            let (sender, mut emissions) = start(sources(), LONG_TICK);
            send(&sender, event);

            let RecordDraft { kind, payload, .. } = one_record(&mut emissions).await;
            assert_eq!(kind, Kind::StateChange);
            assert_eq!(payload["app"], "Zed");
            assert_eq!(payload["bundle_id"], "dev.zed.Zed");
            assert_eq!(payload["display"], 0);
            assert!(payload.get("screen_locked").is_none());
            assert!(payload.get("display_asleep").is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn lock_unlock_and_wake_emit_bare_markers() {
        let expectations = [
            (MacEvent::ScreenLocked, Kind::Lock),
            (MacEvent::ScreenUnlocked, Kind::Unlock),
            (MacEvent::DidWake, Kind::Wake),
        ];
        for (event, expected) in expectations {
            let (sender, mut emissions) = start(sources(), LONG_TICK);
            send(&sender, event);

            let RecordDraft {
                kind,
                payload,
                degraded,
                ..
            } = one_record(&mut emissions).await;
            assert_eq!(kind, expected);
            assert!(!degraded);
            assert_eq!(payload, json!({}));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_sleep_event_is_acknowledged_only_once_the_record_is_committed() {
        let (events, mut emissions) = start(sources(), LONG_TICK);
        let (acknowledgement, receipt) = SleepAcknowledgement::channel();
        send(&events, MacEvent::WillSleep { acknowledgement });

        let Some(emission) = emissions.recv().await else {
            panic!("the provider stopped without emitting");
        };
        let Emission {
            records, committed, ..
        } = &emission;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, Kind::Sleep);
        assert!(committed.is_some());
        assert!(receipt.try_recv().is_err());

        emission.commit();
        let acknowledged = tokio::task::spawn_blocking(move || {
            receipt.recv_timeout(Duration::from_secs(2)).is_ok()
        })
        .await
        .expect("the acknowledgement thread panicked");

        assert!(acknowledged);
    }

    #[tokio::test(start_paused = true)]
    async fn a_sole_accessibility_window_carries_its_title_while_several_are_ambiguous() {
        let sources = sources();
        let title_calls = Arc::clone(&sources.title_calls);
        let (_events, mut emissions) = start(sources, 30);

        let RecordDraft { payload, .. } = one_record(&mut emissions).await;
        let visible = payload["visible"].as_array().expect("a visible set");

        assert_eq!(visible[0]["app"], "Zed");
        assert_eq!(visible[0]["title"], "nikki — windows.rs");
        assert_eq!(visible[0]["title_reason"], Value::Null);
        assert_eq!(visible[0]["z"], 0);

        assert_eq!(visible[1]["app"], "Dia");
        assert_eq!(visible[1]["bundle_id"], "company.thebrowser.dia");
        assert_eq!(visible[1]["title"], "Home – Home Assistant");
        assert_eq!(visible[1]["title_reason"], Value::Null);
        assert_eq!(visible[1]["display"], 1);

        assert_eq!(visible[2]["app"], "Telegram");
        assert_eq!(visible[2]["title"], Value::Null);
        assert_eq!(visible[2]["title_reason"], AMBIGUOUS);
        assert_eq!(visible[2]["z"], 4);

        assert_eq!(title_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_display_is_still_resolved_when_accessibility_is_unavailable() {
        let mut sources = sources();
        sources.focused = FocusedWindow::Unavailable;
        sources.titles.clear();
        for pid in [ZED_PID, DIA_PID, TELEGRAM_PID] {
            sources.titles.insert(pid, WindowTitle::Unavailable);
        }
        let (_events, mut emissions) = start(sources, 30);

        let RecordDraft {
            degraded, payload, ..
        } = one_record(&mut emissions).await;

        assert!(degraded);
        assert_eq!(payload["display"], 0);
        assert_eq!(payload["app"], "Zed");
        assert_eq!(payload["bundle_id"], "dev.zed.Zed");
        assert_eq!(payload.get("title"), None);
        assert_eq!(payload.get("path"), None);

        let visible = payload["visible"].as_array().expect("a visible set");
        assert_eq!(visible.len(), 3);
        for entry in visible {
            assert_eq!(entry["title"], Value::Null);
            assert_eq!(entry["title_reason"], Value::Null);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_accessibility_call_over_a_real_window_is_degraded() {
        let mut sources = sources();
        sources.focused = FocusedWindow::Absent;
        let (_events, mut emissions) = start(sources, 30);

        let RecordDraft { degraded, .. } = one_record(&mut emissions).await;

        assert!(degraded);
    }

    #[tokio::test(start_paused = true)]
    async fn an_application_without_a_window_is_not_degraded_and_uses_the_cursor_display() {
        let mut sources = sources();
        sources.windows.clear();
        sources.focused = FocusedWindow::Absent;
        sources.cursor_display = Some(1);
        let (_events, mut emissions) = start(sources, 30);

        let RecordDraft {
            degraded, payload, ..
        } = one_record(&mut emissions).await;

        assert!(!degraded);
        assert_eq!(payload["display"], 1);
        assert_eq!(payload["visible"], json!([]));
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_inside_the_debounce_window_produces_one_sample_and_one_extraction() {
        let sources = sources();
        let detail_calls = Arc::clone(&sources.detail_calls);
        let (events, mut emissions) = start(sources, LONG_TICK);

        for _ in 0..5 {
            send(&events, MacEvent::TitleChanged { pid: ZED_PID });
        }

        let RecordDraft { kind, .. } = one_record(&mut emissions).await;
        assert_eq!(kind, Kind::StateChange);
        assert!(emissions.try_recv().is_err());
        assert_eq!(
            detail_calls
                .lock()
                .expect("the extractor log is poisoned")
                .len(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_activation_inside_a_burst_wins_over_a_state_change() {
        let (events, mut emissions) = start(sources(), LONG_TICK);

        send(&events, MacEvent::TitleChanged { pid: ZED_PID });
        send(
            &events,
            MacEvent::ApplicationActivated {
                application: application(DIA_PID, "Dia", "company.thebrowser.dia"),
            },
        );
        send(&events, MacEvent::WindowCreated { pid: DIA_PID });

        let RecordDraft { kind, payload, .. } = one_record(&mut emissions).await;
        assert_eq!(kind, Kind::Focus);
        assert_eq!(payload["app"], "Dia");
        assert!(emissions.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_machine_without_a_frontmost_application_emits_nothing() {
        let sources = sources();
        *sources
            .frontmost
            .lock()
            .expect("the frontmost application is poisoned") = None;
        let (events, mut emissions) = start(sources, LONG_TICK);

        send(&events, MacEvent::TitleChanged { pid: ZED_PID });
        send(&events, MacEvent::ScreenLocked);

        let RecordDraft { kind, .. } = one_record(&mut emissions).await;
        assert_eq!(kind, Kind::Lock);
    }

    #[tokio::test(start_paused = true)]
    async fn the_extractor_runs_for_the_focused_bundle_only() {
        let mut sources = sources();
        let mut details = Details::new();
        details.insert(
            "url".to_string(),
            Value::String("https://homeassistant.pkarpovich.space/".to_string()),
        );
        sources.details = details;
        let detail_calls = Arc::clone(&sources.detail_calls);
        let (events, mut emissions) = start(sources, LONG_TICK);

        send(
            &events,
            MacEvent::ApplicationActivated {
                application: application(DIA_PID, "Dia", "company.thebrowser.dia"),
            },
        );

        let RecordDraft { payload, .. } = one_record(&mut emissions).await;
        assert_eq!(
            payload["details"]["url"],
            "https://homeassistant.pkarpovich.space/"
        );
        assert_eq!(
            detail_calls
                .lock()
                .expect("the extractor log is poisoned")
                .as_slice(),
            ["company.thebrowser.dia".to_string()]
        );
    }

    #[test]
    fn a_burst_of_state_changes_keeps_the_timestamp_of_the_first() {
        let first = schedule(None, Kind::StateChange, None, Timestamp::from_millis(1_000));
        let merged = schedule(
            Some(first),
            Kind::StateChange,
            None,
            Timestamp::from_millis(1_200),
        );

        let Pending { kind, ts, .. } = merged;
        assert_eq!(kind, Kind::StateChange);
        assert_eq!(ts, Timestamp::from_millis(1_000));
    }

    #[test]
    fn an_activation_inside_a_burst_carries_the_moment_the_switch_arrived() {
        let first = schedule(None, Kind::StateChange, None, Timestamp::from_millis(1_000));
        let dia = application(DIA_PID, "Dia", "company.thebrowser.dia");
        let merged = schedule(
            Some(first),
            Kind::Focus,
            Some(dia.clone()),
            Timestamp::from_millis(1_300),
        );

        let Pending {
            kind,
            ts,
            application,
            ..
        } = merged;
        assert_eq!(kind, Kind::Focus);
        assert_eq!(ts, Timestamp::from_millis(1_300));
        assert_eq!(application, Some(dia));
    }

    #[test]
    fn a_state_change_after_an_activation_keeps_the_moment_of_the_switch() {
        let dia = application(DIA_PID, "Dia", "company.thebrowser.dia");
        let first = schedule(
            None,
            Kind::Focus,
            Some(dia.clone()),
            Timestamp::from_millis(1_000),
        );
        let merged = schedule(
            Some(first),
            Kind::StateChange,
            None,
            Timestamp::from_millis(1_200),
        );

        let Pending {
            kind,
            ts,
            application,
            ..
        } = merged;
        assert_eq!(kind, Kind::Focus);
        assert_eq!(ts, Timestamp::from_millis(1_000));
        assert_eq!(application, Some(dia));
    }

    #[test]
    fn the_front_window_of_an_application_is_the_closest_to_the_front() {
        let windows = vec![
            window(DIA_PID, "Dia", 901, 0, Rect::new(0.0, 0.0, 100.0, 100.0)),
            window(ZED_PID, "Zed", 900, 1, Rect::new(0.0, 0.0, 100.0, 100.0)),
            window(ZED_PID, "Zed", 899, 2, Rect::new(0.0, 0.0, 100.0, 100.0)),
        ];
        let front = front_window(&windows, ZED_PID).expect("Zed has a window");
        assert_eq!(front.window_number, 900);
        assert!(front_window(&windows, 9999).is_none());
    }

    #[test]
    fn a_chrome_layer_window_is_never_the_front_window() {
        let mut chrome = window(ZED_PID, "Zed", 900, 0, Rect::new(0.0, 0.0, 100.0, 100.0));
        chrome.layer = 25;
        let windows = vec![chrome];
        assert!(front_window(&windows, ZED_PID).is_none());
    }
}
