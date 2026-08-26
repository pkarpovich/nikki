use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
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

use super::ax::MESSAGING_TIMEOUT_SECONDS;
use super::window_list::{RunningApplication, frontmost_application, running_application};

pub const SLEEP_FLUSH_BUDGET: Duration = Duration::from_secs(2);

const SCREEN_LOCKED_NOTIFICATION: &str = "com.apple.screenIsLocked";
const SCREEN_UNLOCKED_NOTIFICATION: &str = "com.apple.screenIsUnlocked";

const AX_NOTIFICATIONS: [(&str, AxNotification); 4] = [
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
        pid: i32,
    },
    TitleChanged {
        pid: i32,
    },
    WindowCreated {
        pid: i32,
    },
    WindowDestroyed {
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

struct Inbox {
    sender: UnboundedSender<MacEvent>,
    injected: Mutex<VecDeque<MacEvent>>,
    focus_target: Mutex<Option<i32>>,
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

struct SourceState {
    inbox: Arc<Inbox>,
    run_loop: CFRetained<CFRunLoop>,
    observer: Option<ObserverRegistration>,
}

struct ObserverRegistration {
    observer: CFRetained<AXObserver>,
    element: CFRetained<AXUIElement>,
    run_loop: CFRetained<CFRunLoop>,
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
        Self::start(sender, RegisterSystemSources::Yes)
    }

    pub fn spawn_without_system_sources(
        sender: UnboundedSender<MacEvent>,
    ) -> std::io::Result<Self> {
        Self::start(sender, RegisterSystemSources::No)
    }

    fn start(
        sender: UnboundedSender<MacEvent>,
        register: RegisterSystemSources,
    ) -> std::io::Result<Self> {
        let inbox = Arc::new(Inbox {
            sender,
            injected: Mutex::new(VecDeque::new()),
            focus_target: Mutex::new(None),
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

    pub fn inject(&self, event: MacEvent) {
        let Ok(mut injected) = self.inbox.injected.lock() else {
            tracing::error!("the injection queue is poisoned");
            return;
        };
        injected.push_back(event);
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
        observer: None,
    }));
    let source = create_source(state.cast::<c_void>());
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });

    let observers = match register {
        RegisterSystemSources::Yes => register_system_sources(&inbox, &source, &run_loop),
        RegisterSystemSources::No => SystemSources::default(),
    };

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
        let RunningApplication { pid, .. } = application;
        if let Ok(mut target) = activation_inbox.focus_target.lock() {
            *target = Some(pid);
        }
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

    seed_focus_target(inbox, source, run_loop);
    sources
}

fn seed_focus_target(
    inbox: &Arc<Inbox>,
    source: &CFRetained<CFRunLoopSource>,
    run_loop: &CFRetained<CFRunLoop>,
) {
    let Some(RunningApplication { pid, .. }) = frontmost_application() else {
        return;
    };
    let Ok(mut target) = inbox.focus_target.lock() else {
        return;
    };
    *target = Some(pid);
    drop(target);
    source.signal();
    run_loop.wake_up();
}

fn activated_application(notification: &NSNotification) -> Option<RunningApplication> {
    let user_info = notification.userInfo()?;
    let key: &AnyObject = unsafe { NSWorkspaceApplicationKey }.as_ref();
    let application = user_info.objectForKey(key)?;
    let application = application.downcast_ref::<NSRunningApplication>()?;
    Some(running_application(application))
}

fn deliver_will_sleep(inbox: &Inbox, budget: Duration) -> SleepFlush {
    let (sender, receiver) = sync_channel::<()>(1);
    let event = MacEvent::WillSleep {
        acknowledgement: SleepAcknowledgement { sender },
    };
    if inbox.sender.send(event).is_err() {
        return SleepFlush::Abandoned;
    }
    match receiver.recv_timeout(budget) {
        Ok(()) => SleepFlush::Acknowledged,
        Err(RecvTimeoutError::Timeout) => SleepFlush::BudgetExpired,
        Err(RecvTimeoutError::Disconnected) => SleepFlush::Abandoned,
    }
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

fn ax_event(notification: AxNotification, pid: i32) -> MacEvent {
    match notification {
        AxNotification::FocusedWindowChanged => MacEvent::FocusedWindowChanged { pid },
        AxNotification::TitleChanged => MacEvent::TitleChanged { pid },
        AxNotification::WindowCreated => MacEvent::WindowCreated { pid },
        AxNotification::UiElementDestroyed => MacEvent::WindowDestroyed { pid },
    }
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
    for event in drained {
        state.inbox.emit(event);
    }

    let mut target = None;
    if let Ok(mut focus_target) = state.inbox.focus_target.lock() {
        target = focus_target.take();
    }
    let Some(pid) = target else {
        return;
    };
    rebind_observer(state, pid);
}

fn rebind_observer(state: &mut SourceState, pid: i32) {
    state.observer = None;
    let Some(registration) =
        ObserverRegistration::create(pid, Arc::clone(&state.inbox), state.run_loop.clone())
    else {
        return;
    };
    state.observer = Some(registration);
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
    inbox.emit(ax_event(notification, pid));
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

    fn inbox() -> (Arc<Inbox>, tokio::sync::mpsc::UnboundedReceiver<MacEvent>) {
        let (sender, receiver) = unbounded_channel();
        let inbox = Arc::new(Inbox {
            sender,
            injected: Mutex::new(VecDeque::new()),
            focus_target: Mutex::new(None),
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

    #[test]
    fn accessibility_notification_names_map_to_kinds() {
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
            let event = ax_event(notification, 4242);
            assert_eq!(describe(&event), (expected, 4242));
        }
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
