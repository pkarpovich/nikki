use std::collections::{HashMap, HashSet, VecDeque};
use std::env::var_os;
use std::ffi::c_void;
use std::fs;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification, NSWorkspaceDidWakeNotification,
    NSWorkspaceWillSleepNotification,
};
use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext, CFString, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
    CGDisplayRemoveReconfigurationCallback,
};
use objc2_foundation::{NSDistributedNotificationCenter, NSNotification, NSString};
use tokio::sync::mpsc::UnboundedSender;

use super::ax::{MESSAGING_TIMEOUT_SECONDS, accessibility_is_trusted};
use super::window_list::{
    RunningApplication, WindowEntry, application_for_pid, every_window, is_regular_application,
    running_application,
};

pub const SLEEP_FLUSH_BUDGET: Duration = Duration::from_secs(2);
pub const TEST_EVENTS_VAR: &str = "NIKKI_TEST_EVENTS";

const SCREEN_LOCKED_NOTIFICATION: &str = "com.apple.screenIsLocked";
const SCREEN_UNLOCKED_NOTIFICATION: &str = "com.apple.screenIsUnlocked";

const AX_NOTIFICATIONS: [(&str, AxNotification); 6] = [
    (
        "AXApplicationActivated",
        AxNotification::ApplicationActivated,
    ),
    (
        "AXApplicationDeactivated",
        AxNotification::ApplicationDeactivated,
    ),
    (
        "AXFocusedWindowChanged",
        AxNotification::FocusedWindowChanged,
    ),
    ("AXTitleChanged", AxNotification::TitleChanged),
    ("AXWindowCreated", AxNotification::WindowCreated),
    ("AXUIElementDestroyed", AxNotification::UiElementDestroyed),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxNotification {
    ApplicationActivated,
    ApplicationDeactivated,
    FocusedWindowChanged,
    TitleChanged,
    WindowCreated,
    UiElementDestroyed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepFlush {
    Acknowledged,
    BudgetExpired,
    Abandoned,
}

#[derive(Debug)]
pub struct SleepAcknowledgement {
    sender: SyncSender<()>,
}

impl SleepAcknowledgement {
    pub fn channel() -> (SleepAcknowledgement, Receiver<()>) {
        let (sender, receiver) = sync_channel::<()>(1);
        (SleepAcknowledgement { sender }, receiver)
    }

    pub fn acknowledge(self) {
        let _ = self.sender.try_send(());
    }
}

#[derive(Debug)]
pub enum MacEvent {
    ApplicationActivated {
        application: RunningApplication,
    },
    FocusedWindowChanged {
        #[cfg_attr(not(test), allow(dead_code))]
        pid: i32,
    },
    TitleChanged {
        #[cfg_attr(not(test), allow(dead_code))]
        pid: i32,
    },
    WindowCreated {
        #[cfg_attr(not(test), allow(dead_code))]
        pid: i32,
    },
    WindowDestroyed {
        #[cfg_attr(not(test), allow(dead_code))]
        pid: i32,
    },
    DisplaysReconfigured,
    ScreenLocked,
    ScreenUnlocked,
    WillSleep {
        acknowledgement: SleepAcknowledgement,
    },
    DidWake,
}

enum Injected {
    Event(MacEvent),
    Sleep,
}

struct Inbox {
    sender: UnboundedSender<MacEvent>,
    injected: Mutex<VecDeque<Injected>>,
    rescan: AtomicBool,
}

impl Inbox {
    fn emit(&self, event: MacEvent) {
        if self.sender.send(event).is_ok() {
            return;
        }
        tracing::debug!("an event was dropped because the runtime is gone");
    }
}

struct RunLoopRef(CFRetained<CFRunLoop>);

unsafe impl Send for RunLoopRef {}

struct SourceRef(CFRetained<CFRunLoopSource>);

unsafe impl Send for SourceRef {}

pub struct RescanHandle {
    inbox: Weak<Inbox>,
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
}

unsafe impl Send for RescanHandle {}
unsafe impl Sync for RescanHandle {}

impl RescanHandle {
    pub fn request(&self) {
        let Some(inbox) = self.inbox.upgrade() else {
            return;
        };
        inbox.rescan.store(true, Ordering::SeqCst);
        self.source.signal();
        self.run_loop.wake_up();
    }
}

struct SourceState {
    inbox: Arc<Inbox>,
    run_loop: CFRetained<CFRunLoop>,
    observers: HashMap<i32, ObserverRegistration>,
    refused: HashSet<i32>,
}

struct ObserverRegistration {
    observer: CFRetained<AXObserver>,
    element: CFRetained<AXUIElement>,
    run_loop: CFRetained<CFRunLoop>,
    #[allow(dead_code)]
    inbox: Arc<Inbox>,
}

impl Drop for ObserverRegistration {
    fn drop(&mut self) {
        for (name, _) in AX_NOTIFICATIONS {
            let name = CFString::from_str(name);
            let status = unsafe { self.observer.remove_notification(&self.element, &name) };
            if status == AXError::Success {
                continue;
            }
            tracing::debug!(
                status = status.0,
                "could not remove an accessibility source"
            );
        }
        let source = unsafe { self.observer.run_loop_source() };
        self.run_loop
            .remove_source(Some(&source), unsafe { kCFRunLoopDefaultMode });
    }
}

pub struct EventThread {
    run_loop: RunLoopRef,
    source: SourceRef,
    inbox: Arc<Inbox>,
    thread: Option<JoinHandle<()>>,
}

impl EventThread {
    pub fn spawn(sender: UnboundedSender<MacEvent>) -> std::io::Result<Self> {
        let Some(script) = var_os(TEST_EVENTS_VAR) else {
            return Self::start(sender, RegisterSystemSources::Yes, VecDeque::new());
        };
        let script = Path::new(&script);
        let scripted = scripted_events(script);
        tracing::warn!(
            path = %script.display(),
            events = scripted.len(),
            "{TEST_EVENTS_VAR} replaces the system event sources with a scripted file"
        );
        Self::start(sender, RegisterSystemSources::No, scripted)
    }

    #[cfg(test)]
    pub fn spawn_without_system_sources(
        sender: UnboundedSender<MacEvent>,
    ) -> std::io::Result<Self> {
        Self::start(sender, RegisterSystemSources::No, VecDeque::new())
    }

    fn start(
        sender: UnboundedSender<MacEvent>,
        register: RegisterSystemSources,
        scripted: VecDeque<Injected>,
    ) -> std::io::Result<Self> {
        let inbox = Arc::new(Inbox {
            sender,
            injected: Mutex::new(scripted),
            rescan: AtomicBool::new(false),
        });

        let (ready_sender, ready_receiver) = sync_channel::<Option<(RunLoopRef, SourceRef)>>(1);
        let thread_inbox = Arc::clone(&inbox);
        let thread = std::thread::Builder::new()
            .name("nikki-events".to_owned())
            .spawn(move || run_event_loop(thread_inbox, register, ready_sender))?;

        let Ok(Some((run_loop, source))) = ready_receiver.recv() else {
            let _ = thread.join();
            return Err(std::io::Error::other(
                "the event thread could not take its run loop",
            ));
        };

        Ok(Self {
            run_loop,
            source,
            inbox,
            thread: Some(thread),
        })
    }

    pub fn rescan_handle(&self) -> RescanHandle {
        RescanHandle {
            inbox: Arc::downgrade(&self.inbox),
            source: self.source.0.clone(),
            run_loop: self.run_loop.0.clone(),
        }
    }

    #[cfg(test)]
    pub fn inject(&self, event: MacEvent) {
        let Ok(mut injected) = self.inbox.injected.lock() else {
            tracing::error!("the injection queue is poisoned");
            return;
        };
        injected.push_back(Injected::Event(event));
        drop(injected);
        self.source.0.signal();
        self.run_loop.0.wake_up();
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.run_loop.0.stop();
        self.run_loop.0.wake_up();
        if thread.join().is_ok() {
            return;
        }
        tracing::error!("the event thread did not shut down cleanly");
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegisterSystemSources {
    Yes,
    No,
}

fn run_event_loop(
    inbox: Arc<Inbox>,
    register: RegisterSystemSources,
    ready: SyncSender<Option<(RunLoopRef, SourceRef)>>,
) {
    let Some(run_loop) = CFRunLoop::current() else {
        let _ = ready.send(None);
        return;
    };

    let state = Box::into_raw(Box::new(SourceState {
        inbox: Arc::clone(&inbox),
        run_loop: run_loop.clone(),
        observers: HashMap::new(),
        refused: HashSet::new(),
    }));
    let source = create_source(state.cast::<c_void>());
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });

    let observers = match register {
        RegisterSystemSources::Yes => register_system_sources(&inbox, &source, &run_loop),
        RegisterSystemSources::No => SystemSources::default(),
    };
    source.signal();

    let published = ready.send(Some((
        RunLoopRef(run_loop.clone()),
        SourceRef(source.clone()),
    )));
    if published.is_ok() {
        CFRunLoop::run();
    }

    drop(observers);
    source.invalidate();
    drop(unsafe { Box::from_raw(state) });
}

#[derive(Default)]
struct SystemSources {
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    display_callback: Option<*mut c_void>,
}

impl Drop for SystemSources {
    fn drop(&mut self) {
        if self.tokens.is_empty() && self.display_callback.is_none() {
            return;
        }
        let workspace = NSWorkspace::sharedWorkspace();
        let workspace_center = workspace.notificationCenter();
        let distributed_center = NSDistributedNotificationCenter::defaultCenter();
        for token in self.tokens.drain(..) {
            unsafe { workspace_center.removeObserver(token.as_ref()) };
            unsafe { distributed_center.removeObserver(token.as_ref()) };
        }

        let Some(user_info) = self.display_callback.take() else {
            return;
        };
        unsafe { CGDisplayRemoveReconfigurationCallback(Some(on_display_reconfigured), user_info) };
        drop(unsafe { Arc::from_raw(user_info.cast::<Inbox>()) });
    }
}

fn register_system_sources(
    inbox: &Arc<Inbox>,
    source: &CFRetained<CFRunLoopSource>,
    run_loop: &CFRetained<CFRunLoop>,
) -> SystemSources {
    let mut sources = SystemSources::default();

    let workspace = NSWorkspace::sharedWorkspace();
    let workspace_center = workspace.notificationCenter();

    let activation_inbox = Arc::clone(inbox);
    let activation_source = source.clone();
    let activation_run_loop = run_loop.clone();
    let activation = RcBlock::new(move |notification: NonNull<NSNotification>| {
        let notification = unsafe { notification.as_ref() };
        let Some(application) = activated_application(notification) else {
            return;
        };
        activation_inbox.rescan.store(true, Ordering::SeqCst);
        activation_inbox.emit(MacEvent::ApplicationActivated { application });
        activation_source.signal();
        activation_run_loop.wake_up();
    });
    let token = unsafe {
        workspace_center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidActivateApplicationNotification),
            None,
            None,
            &activation,
        )
    };
    sources.tokens.push(token);

    let sleep_inbox = Arc::clone(inbox);
    let sleep = RcBlock::new(move |_: NonNull<NSNotification>| {
        let outcome = deliver_will_sleep(&sleep_inbox, SLEEP_FLUSH_BUDGET);
        tracing::info!(?outcome, "the machine is going to sleep");
    });
    let token = unsafe {
        workspace_center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceWillSleepNotification),
            None,
            None,
            &sleep,
        )
    };
    sources.tokens.push(token);

    let wake_inbox = Arc::clone(inbox);
    let wake = RcBlock::new(move |_: NonNull<NSNotification>| {
        wake_inbox.emit(MacEvent::DidWake);
    });
    let token = unsafe {
        workspace_center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidWakeNotification),
            None,
            None,
            &wake,
        )
    };
    sources.tokens.push(token);

    let distributed_center = NSDistributedNotificationCenter::defaultCenter();
    for name in [SCREEN_LOCKED_NOTIFICATION, SCREEN_UNLOCKED_NOTIFICATION] {
        let screen_inbox = Arc::clone(inbox);
        let screen = RcBlock::new(move |notification: NonNull<NSNotification>| {
            let notification = unsafe { notification.as_ref() };
            let name = notification.name().to_string();
            let Some(event) = screen_event_from_name(&name) else {
                return;
            };
            screen_inbox.emit(event);
        });
        let token = unsafe {
            distributed_center.addObserverForName_object_queue_usingBlock(
                Some(&NSString::from_str(name)),
                None,
                None,
                &screen,
            )
        };
        sources.tokens.push(token);
    }

    let user_info = Arc::into_raw(Arc::clone(inbox)).cast::<c_void>().cast_mut();
    let status = unsafe {
        CGDisplayRegisterReconfigurationCallback(Some(on_display_reconfigured), user_info)
    };
    if status.0 == 0 {
        sources.display_callback = Some(user_info);
    } else {
        tracing::warn!(
            status = status.0,
            "could not observe display reconfiguration"
        );
        drop(unsafe { Arc::from_raw(user_info.cast::<Inbox>()) });
    }

    inbox.rescan.store(true, Ordering::SeqCst);
    sources
}

fn activated_application(notification: &NSNotification) -> Option<RunningApplication> {
    let user_info = notification.userInfo()?;
    let key: &AnyObject = unsafe { NSWorkspaceApplicationKey }.as_ref();
    let application = user_info.objectForKey(key)?;
    let application = application.downcast_ref::<NSRunningApplication>()?;
    Some(running_application(application))
}

fn deliver_will_sleep(inbox: &Inbox, budget: Duration) -> SleepFlush {
    let (acknowledgement, receiver) = SleepAcknowledgement::channel();
    let event = MacEvent::WillSleep { acknowledgement };
    if inbox.sender.send(event).is_err() {
        return SleepFlush::Abandoned;
    }
    match receiver.recv_timeout(budget) {
        Ok(()) => SleepFlush::Acknowledged,
        Err(RecvTimeoutError::Timeout) => SleepFlush::BudgetExpired,
        Err(RecvTimeoutError::Disconnected) => SleepFlush::Abandoned,
    }
}

fn scripted_events(path: &Path) -> VecDeque<Injected> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "the scripted event file could not be read");
            return VecDeque::new();
        }
    };

    let mut scripted = VecDeque::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let Some(event) = scripted_event(line) else {
            tracing::warn!(
                line,
                "a scripted event line was not understood and is skipped"
            );
            continue;
        };
        scripted.push_back(event);
    }
    scripted
}

fn scripted_event(line: &str) -> Option<Injected> {
    let mut fields = line.split('\t');
    let name = fields.next()?;
    let event = match name {
        "application_activated" => {
            let application = RunningApplication {
                pid: fields.next()?.parse().ok()?,
                name: fields.next().map(str::to_string),
                bundle_id: fields.next().map(str::to_string),
            };
            MacEvent::ApplicationActivated { application }
        }
        "focused_window_changed" => MacEvent::FocusedWindowChanged {
            pid: fields.next()?.parse().ok()?,
        },
        "title_changed" => MacEvent::TitleChanged {
            pid: fields.next()?.parse().ok()?,
        },
        "window_created" => MacEvent::WindowCreated {
            pid: fields.next()?.parse().ok()?,
        },
        "window_destroyed" => MacEvent::WindowDestroyed {
            pid: fields.next()?.parse().ok()?,
        },
        "displays_reconfigured" => MacEvent::DisplaysReconfigured,
        "screen_locked" => MacEvent::ScreenLocked,
        "screen_unlocked" => MacEvent::ScreenUnlocked,
        "did_wake" => MacEvent::DidWake,
        "will_sleep" => return Some(Injected::Sleep),
        _ => return None,
    };
    Some(Injected::Event(event))
}

fn screen_event_from_name(name: &str) -> Option<MacEvent> {
    if name == SCREEN_LOCKED_NOTIFICATION {
        return Some(MacEvent::ScreenLocked);
    }
    if name == SCREEN_UNLOCKED_NOTIFICATION {
        return Some(MacEvent::ScreenUnlocked);
    }
    None
}

fn ax_notification_from_name(name: &str) -> Option<AxNotification> {
    for (candidate, notification) in AX_NOTIFICATIONS {
        if candidate == name {
            return Some(notification);
        }
    }
    None
}

pub fn ax_event<F>(notification: AxNotification, pid: i32, application_of: F) -> Option<MacEvent>
where
    F: Fn(i32) -> Option<RunningApplication>,
{
    match notification {
        AxNotification::ApplicationActivated => {
            let application = match application_of(pid) {
                Some(application) => application,
                None => RunningApplication {
                    pid,
                    name: None,
                    bundle_id: None,
                },
            };
            Some(MacEvent::ApplicationActivated { application })
        }
        AxNotification::ApplicationDeactivated => None,
        AxNotification::FocusedWindowChanged => Some(MacEvent::FocusedWindowChanged { pid }),
        AxNotification::TitleChanged => Some(MacEvent::TitleChanged { pid }),
        AxNotification::WindowCreated => Some(MacEvent::WindowCreated { pid }),
        AxNotification::UiElementDestroyed => Some(MacEvent::WindowDestroyed { pid }),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ObserverPlan {
    attach: Vec<i32>,
    detach: Vec<i32>,
    forget: Vec<i32>,
}

fn observer_plan(attached: &[i32], refused: &[i32], present: &[i32]) -> ObserverPlan {
    let mut plan = ObserverPlan::default();
    for pid in present {
        if attached.contains(pid) || refused.contains(pid) || plan.attach.contains(pid) {
            continue;
        }
        plan.attach.push(*pid);
    }
    for pid in attached {
        if present.contains(pid) {
            continue;
        }
        plan.detach.push(*pid);
    }
    for pid in refused {
        if present.contains(pid) {
            continue;
        }
        plan.forget.push(*pid);
    }
    plan
}

fn application_pids<F>(windows: &[WindowEntry], is_application: F) -> Vec<i32>
where
    F: Fn(i32) -> bool,
{
    let mut pids = Vec::new();
    for window in windows {
        let WindowEntry {
            owner_pid, layer, ..
        } = window;
        if *layer != 0 || pids.contains(owner_pid) {
            continue;
        }
        if !is_application(*owner_pid) {
            continue;
        }
        pids.push(*owner_pid);
    }
    pids
}

fn create_source(info: *mut c_void) -> CFRetained<CFRunLoopSource> {
    let mut context = CFRunLoopSourceContext {
        version: 0,
        info,
        retain: None,
        release: None,
        copyDescription: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: Some(perform_source),
    };
    let source = unsafe { CFRunLoopSource::new(None, 0, &raw mut context) };
    source.expect("core foundation could not create a run loop source")
}

unsafe extern "C-unwind" fn perform_source(info: *mut c_void) {
    let Some(state) = NonNull::new(info.cast::<SourceState>()) else {
        return;
    };
    let state = unsafe { &mut *state.as_ptr() };

    let mut drained = VecDeque::new();
    if let Ok(mut injected) = state.inbox.injected.lock() {
        std::mem::swap(&mut drained, &mut injected);
    }
    for item in drained {
        match item {
            Injected::Event(event) => state.inbox.emit(event),
            Injected::Sleep => {
                let outcome = deliver_will_sleep(&state.inbox, SLEEP_FLUSH_BUDGET);
                tracing::info!(?outcome, "a scripted sleep was delivered");
            }
        }
    }

    if !state.inbox.rescan.swap(false, Ordering::SeqCst) {
        return;
    }
    rescan_observers(
        state,
        &application_pids(&every_window(), is_regular_application),
    );
}

fn rescan_observers(state: &mut SourceState, present: &[i32]) {
    if !accessibility_is_trusted() {
        tracing::debug!("accessibility is not granted, so no application can be observed yet");
        return;
    }

    let mut attached = Vec::with_capacity(state.observers.len());
    for pid in state.observers.keys() {
        attached.push(*pid);
    }
    let mut refused = Vec::with_capacity(state.refused.len());
    for pid in &state.refused {
        refused.push(*pid);
    }

    let ObserverPlan {
        attach,
        detach,
        forget,
    } = observer_plan(&attached, &refused, present);

    for pid in detach {
        state.observers.remove(&pid);
    }
    for pid in forget {
        state.refused.remove(&pid);
    }
    for pid in attach {
        let Some(registration) =
            ObserverRegistration::create(pid, Arc::clone(&state.inbox), state.run_loop.clone())
        else {
            tracing::warn!(
                pid,
                "an application refuses accessibility and is not observed"
            );
            state.refused.insert(pid);
            continue;
        };
        state.observers.insert(pid, registration);
    }
}

impl ObserverRegistration {
    fn create(pid: i32, inbox: Arc<Inbox>, run_loop: CFRetained<CFRunLoop>) -> Option<Self> {
        let mut raw: *mut AXObserver = std::ptr::null_mut();
        let status =
            unsafe { AXObserver::create(pid, Some(on_ax_notification), NonNull::from(&mut raw)) };
        if status != AXError::Success {
            tracing::debug!(status = status.0, pid, "could not observe the application");
            return None;
        }
        let observer = NonNull::new(raw)?;
        let observer = unsafe { CFRetained::from_raw(observer) };

        let element = unsafe { AXUIElement::new_application(pid) };
        let timeout = unsafe { element.set_messaging_timeout(MESSAGING_TIMEOUT_SECONDS) };
        if timeout != AXError::Success {
            tracing::debug!(
                status = timeout.0,
                "could not set the accessibility timeout"
            );
        }

        let refcon = Arc::as_ptr(&inbox).cast::<c_void>().cast_mut();
        let mut registered = 0;
        for (name, _) in AX_NOTIFICATIONS {
            let name = CFString::from_str(name);
            let status = unsafe { observer.add_notification(&element, &name, refcon) };
            if status == AXError::Success {
                registered += 1;
                continue;
            }
            tracing::debug!(status = status.0, "could not add an accessibility source");
        }
        if registered == 0 {
            return None;
        }

        let source = unsafe { observer.run_loop_source() };
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });

        Some(Self {
            observer,
            element,
            run_loop,
            inbox,
        })
    }
}

unsafe extern "C-unwind" fn on_ax_notification(
    _observer: NonNull<AXObserver>,
    element: NonNull<AXUIElement>,
    notification: NonNull<CFString>,
    refcon: *mut c_void,
) {
    let Some(inbox) = NonNull::new(refcon.cast::<Inbox>()) else {
        return;
    };
    let inbox = unsafe { inbox.as_ref() };
    let name = unsafe { notification.as_ref() }.to_string();
    let Some(notification) = ax_notification_from_name(&name) else {
        return;
    };

    let mut pid: i32 = 0;
    let status = unsafe { element.as_ref().pid(NonNull::from(&mut pid)) };
    if status != AXError::Success {
        return;
    }
    let Some(event) = ax_event(notification, pid, application_for_pid) else {
        return;
    };
    inbox.emit(event);
}

unsafe extern "C-unwind" fn on_display_reconfigured(
    _display: CGDirectDisplayID,
    flags: CGDisplayChangeSummaryFlags,
    user_info: *mut c_void,
) {
    if flags.contains(CGDisplayChangeSummaryFlags::BeginConfigurationFlag) {
        return;
    }
    let Some(inbox) = NonNull::new(user_info.cast::<Inbox>()) else {
        return;
    };
    unsafe { inbox.as_ref() }.emit(MacEvent::DisplaysReconfigured);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::sync::mpsc::unbounded_channel;

    use crate::macos::window_list::Rect;

    fn inbox() -> (Arc<Inbox>, tokio::sync::mpsc::UnboundedReceiver<MacEvent>) {
        let (sender, receiver) = unbounded_channel();
        let inbox = Arc::new(Inbox {
            sender,
            injected: Mutex::new(VecDeque::new()),
            rescan: AtomicBool::new(false),
        });
        (inbox, receiver)
    }

    fn describe(event: &MacEvent) -> (&'static str, i32) {
        match event {
            MacEvent::ApplicationActivated { application } => {
                ("application_activated", application.pid)
            }
            MacEvent::FocusedWindowChanged { pid } => ("focused_window_changed", *pid),
            MacEvent::TitleChanged { pid } => ("title_changed", *pid),
            MacEvent::WindowCreated { pid } => ("window_created", *pid),
            MacEvent::WindowDestroyed { pid } => ("window_destroyed", *pid),
            MacEvent::DisplaysReconfigured => ("displays_reconfigured", 0),
            MacEvent::ScreenLocked => ("screen_locked", 0),
            MacEvent::ScreenUnlocked => ("screen_unlocked", 0),
            MacEvent::WillSleep { .. } => ("will_sleep", 0),
            MacEvent::DidWake => ("did_wake", 0),
        }
    }

    fn nobody_is_a_running_application(_pid: i32) -> Option<RunningApplication> {
        None
    }

    #[test]
    fn accessibility_notification_names_map_to_kinds() {
        assert_eq!(
            ax_notification_from_name("AXApplicationActivated"),
            Some(AxNotification::ApplicationActivated)
        );
        assert_eq!(
            ax_notification_from_name("AXApplicationDeactivated"),
            Some(AxNotification::ApplicationDeactivated)
        );
        assert_eq!(
            ax_notification_from_name("AXFocusedWindowChanged"),
            Some(AxNotification::FocusedWindowChanged)
        );
        assert_eq!(
            ax_notification_from_name("AXTitleChanged"),
            Some(AxNotification::TitleChanged)
        );
        assert_eq!(
            ax_notification_from_name("AXWindowCreated"),
            Some(AxNotification::WindowCreated)
        );
        assert_eq!(
            ax_notification_from_name("AXUIElementDestroyed"),
            Some(AxNotification::UiElementDestroyed)
        );
    }

    #[test]
    fn an_unwatched_accessibility_notification_maps_to_nothing() {
        assert_eq!(ax_notification_from_name("AXValueChanged"), None);
        assert_eq!(ax_notification_from_name(""), None);
    }

    #[test]
    fn every_accessibility_notification_becomes_an_event_for_its_pid() {
        let expectations = [
            (
                AxNotification::FocusedWindowChanged,
                "focused_window_changed",
            ),
            (AxNotification::TitleChanged, "title_changed"),
            (AxNotification::WindowCreated, "window_created"),
            (AxNotification::UiElementDestroyed, "window_destroyed"),
        ];
        for (notification, expected) in expectations {
            let event = ax_event(notification, 4242, nobody_is_a_running_application)
                .expect("the notification describes an event");
            assert_eq!(describe(&event), (expected, 4242));
        }
    }

    #[test]
    fn an_activation_carries_the_application_the_pid_belongs_to() {
        let event = ax_event(AxNotification::ApplicationActivated, 502, |pid| {
            Some(RunningApplication {
                pid,
                name: Some("Dia".to_string()),
                bundle_id: Some("company.thebrowser.dia".to_string()),
            })
        })
        .expect("an activation describes an event");

        let MacEvent::ApplicationActivated { application } = event else {
            panic!("expected an activation");
        };
        let RunningApplication {
            pid,
            name,
            bundle_id,
        } = application;
        assert_eq!(pid, 502);
        assert_eq!(name.as_deref(), Some("Dia"));
        assert_eq!(bundle_id.as_deref(), Some("company.thebrowser.dia"));
    }

    #[test]
    fn an_activation_of_a_pid_nobody_names_still_reports_the_pid() {
        let event = ax_event(
            AxNotification::ApplicationActivated,
            502,
            nobody_is_a_running_application,
        )
        .expect("an activation describes an event");
        assert_eq!(describe(&event), ("application_activated", 502));
    }

    #[test]
    fn a_deactivation_is_only_a_hint_and_becomes_no_event() {
        assert!(
            ax_event(
                AxNotification::ApplicationDeactivated,
                502,
                nobody_is_a_running_application
            )
            .is_none()
        );
    }

    #[test]
    fn a_new_pid_is_attached_and_a_departed_one_is_detached() {
        let plan = observer_plan(&[10, 20], &[], &[20, 30]);
        assert_eq!(
            plan,
            ObserverPlan {
                attach: vec![30],
                detach: vec![10],
                forget: Vec::new(),
            }
        );
    }

    #[test]
    fn a_pid_that_is_already_observed_is_left_alone() {
        let plan = observer_plan(&[10, 20], &[], &[10, 20]);
        assert_eq!(plan, ObserverPlan::default());
    }

    #[test]
    fn a_pid_owning_several_windows_is_attached_once() {
        let plan = observer_plan(&[], &[], &[10, 10, 10]);
        assert_eq!(plan.attach, vec![10]);
    }

    #[test]
    fn a_refusing_pid_is_not_retried_while_it_is_still_there() {
        let plan = observer_plan(&[20], &[10], &[10, 20]);
        assert_eq!(plan, ObserverPlan::default());
    }

    #[test]
    fn a_refusing_pid_that_left_is_forgotten_rather_than_refused_forever() {
        let plan = observer_plan(&[], &[10], &[20]);
        assert_eq!(
            plan,
            ObserverPlan {
                attach: vec![20],
                detach: Vec::new(),
                forget: vec![10],
            }
        );
    }

    fn owned(pid: i32, layer: i32, z: usize) -> WindowEntry {
        WindowEntry {
            owner_pid: pid,
            owner_name: String::new(),
            window_number: z as u32,
            bounds: Rect::default(),
            layer,
            z,
        }
    }

    fn every_owner_is_an_application(_pid: i32) -> bool {
        true
    }

    #[test]
    fn the_owners_of_ordinary_windows_are_the_applications_to_observe() {
        let windows = vec![
            owned(10, 0, 0),
            owned(99, 25, 1),
            owned(20, 0, 2),
            owned(10, 0, 3),
        ];
        assert_eq!(
            application_pids(&windows, every_owner_is_an_application),
            vec![10, 20]
        );
    }

    #[test]
    fn an_owner_that_is_not_an_application_is_never_observed() {
        let windows = vec![owned(10, 0, 0), owned(77, 0, 1), owned(20, 0, 2)];
        assert_eq!(application_pids(&windows, |pid| pid != 77), vec![10, 20]);
    }

    #[test]
    fn a_machine_without_windows_has_nothing_to_observe() {
        assert!(application_pids(&[], every_owner_is_an_application).is_empty());
    }

    fn describe_scripted(item: &Injected) -> (&'static str, i32) {
        match item {
            Injected::Event(event) => describe(event),
            Injected::Sleep => ("will_sleep", 0),
        }
    }

    fn scripted(line: &str) -> (&'static str, i32) {
        let item = scripted_event(line).expect("the line describes an event");
        describe_scripted(&item)
    }

    #[test]
    fn every_scripted_line_maps_to_the_event_it_names() {
        assert_eq!(scripted("title_changed\t501"), ("title_changed", 501));
        assert_eq!(
            scripted("focused_window_changed\t501"),
            ("focused_window_changed", 501)
        );
        assert_eq!(scripted("window_created\t7"), ("window_created", 7));
        assert_eq!(scripted("window_destroyed\t7"), ("window_destroyed", 7));
        assert_eq!(
            scripted("displays_reconfigured"),
            ("displays_reconfigured", 0)
        );
        assert_eq!(scripted("screen_locked"), ("screen_locked", 0));
        assert_eq!(scripted("screen_unlocked"), ("screen_unlocked", 0));
        assert_eq!(scripted("did_wake"), ("did_wake", 0));
        assert_eq!(scripted("will_sleep"), ("will_sleep", 0));
    }

    #[test]
    fn a_scripted_activation_carries_the_application_it_names() {
        let item = scripted_event("application_activated\t502\tDia\tcompany.thebrowser.dia")
            .expect("the line describes an event");
        let Injected::Event(MacEvent::ApplicationActivated { application }) = item else {
            panic!("expected an activation");
        };
        let RunningApplication {
            pid,
            name,
            bundle_id,
        } = application;
        assert_eq!(pid, 502);
        assert_eq!(name.as_deref(), Some("Dia"));
        assert_eq!(bundle_id.as_deref(), Some("company.thebrowser.dia"));
    }

    #[test]
    fn an_unusable_scripted_line_is_refused_rather_than_guessed() {
        assert!(scripted_event("title_changed").is_none());
        assert!(scripted_event("title_changed\tnot-a-pid").is_none());
        assert!(scripted_event("nothing_like_an_event").is_none());
        assert!(scripted_event("").is_none());
    }

    #[test]
    fn a_scripted_file_is_read_in_order_and_skips_what_it_cannot_parse() {
        let path = std::env::temp_dir().join("nikki-events-test-script.tsv");
        std::fs::write(
            &path,
            "title_changed\t501\n\nnonsense\nscreen_locked\nwill_sleep\n",
        )
        .expect("the script could not be written");

        let scripted = scripted_events(&path);
        let mut described = Vec::new();
        for item in &scripted {
            described.push(describe_scripted(item));
        }
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            described,
            vec![
                ("title_changed", 501),
                ("screen_locked", 0),
                ("will_sleep", 0)
            ]
        );
    }

    #[test]
    fn a_missing_scripted_file_leaves_the_queue_empty_rather_than_failing() {
        let path = std::env::temp_dir().join("nikki-events-test-absent.tsv");
        let _ = std::fs::remove_file(&path);
        assert!(scripted_events(&path).is_empty());
    }

    #[test]
    fn screen_notification_names_map_to_lock_and_unlock() {
        let locked = screen_event_from_name(SCREEN_LOCKED_NOTIFICATION).expect("no lock event");
        assert_eq!(describe(&locked), ("screen_locked", 0));

        let unlocked =
            screen_event_from_name(SCREEN_UNLOCKED_NOTIFICATION).expect("no unlock event");
        assert_eq!(describe(&unlocked), ("screen_unlocked", 0));

        assert!(screen_event_from_name("com.apple.somethingElse").is_none());
    }

    #[test]
    fn the_sleep_budget_is_two_seconds() {
        assert_eq!(SLEEP_FLUSH_BUDGET, Duration::from_secs(2));
    }

    #[test]
    fn the_sleep_handler_waits_for_the_acknowledgement() {
        let (inbox, mut receiver) = inbox();
        let flusher = std::thread::spawn(move || {
            let Some(MacEvent::WillSleep { acknowledgement }) = receiver.blocking_recv() else {
                return;
            };
            std::thread::sleep(Duration::from_millis(120));
            acknowledgement.acknowledge();
        });

        let started = Instant::now();
        let outcome = deliver_will_sleep(&inbox, SLEEP_FLUSH_BUDGET);
        let elapsed = started.elapsed();
        flusher.join().expect("the flusher thread panicked");

        assert_eq!(outcome, SleepFlush::Acknowledged);
        assert!(elapsed >= Duration::from_millis(100), "returned too early");
        assert!(elapsed < SLEEP_FLUSH_BUDGET, "waited past the budget");
    }

    #[test]
    fn the_sleep_handler_returns_once_the_budget_expires() {
        let (inbox, _receiver) = inbox();
        let started = Instant::now();
        let outcome = deliver_will_sleep(&inbox, SLEEP_FLUSH_BUDGET);
        let elapsed = started.elapsed();

        assert_eq!(outcome, SleepFlush::BudgetExpired);
        assert!(elapsed >= SLEEP_FLUSH_BUDGET, "returned before the budget");
        assert!(
            elapsed < SLEEP_FLUSH_BUDGET + Duration::from_secs(1),
            "waited well past the budget"
        );
    }

    #[test]
    fn a_gone_runtime_abandons_the_sleep_barrier_immediately() {
        let (inbox, receiver) = inbox();
        drop(receiver);
        let started = Instant::now();
        let outcome = deliver_will_sleep(&inbox, SLEEP_FLUSH_BUDGET);

        assert_eq!(outcome, SleepFlush::Abandoned);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn the_event_thread_delivers_an_injected_event_and_stops() {
        let (sender, mut receiver) = unbounded_channel();
        let thread = EventThread::spawn_without_system_sources(sender)
            .expect("the event thread could not start");

        thread.inject(MacEvent::TitleChanged { pid: 77 });
        let event = receiver.blocking_recv().expect("no event was delivered");
        assert_eq!(describe(&event), ("title_changed", 77));

        thread.inject(MacEvent::ScreenLocked);
        let event = receiver.blocking_recv().expect("no event was delivered");
        assert_eq!(describe(&event), ("screen_locked", 0));

        thread.stop();
        assert!(receiver.blocking_recv().is_none());
    }
}
