# Ask Accessibility who is focused, and observe the changes

## Overview

The daemon infers focus from paint order: `frontmost_application` takes the owner of the topmost
layer-0 window. That was the right call when it was written - `NSWorkspace.frontmostApplication` was
frozen, and the window list was the only source that moved. It is wrong in two ways that show up in
real days:

- **A window that cannot hold focus wins.** `borders`, a utility that draws a frame around the active
  window, was recorded as the focused application for 32 minutes on 28 August. It is not even a
  registered application - `lsappinfo` does not list it, only `pgrep` finds it, and its `bundle_id`
  ships as an empty string. Any overlay - a notch bar, an HUD, a screenshot tool - belongs to the same
  class, and a list of names to ignore would have to grow forever.
- **Nothing distinguishes focus from what is merely in front.** During one 71-minute lock on 29 August
  the daemon shipped 113 ticks reading `app: Agterm` - which is wrong about focus and right about the
  machine, since `foreground: claude` and the command line rode along with every one of them. The
  record needs both readings kept apart deliberately rather than by accident, which is what Task 3
  settles.

Accessibility answers the actual question. Two spikes ran against this machine before this plan:

**Polled `AXFocusedApplication`** - 2123 samples over 71 minutes, `AXIsProcessTrusted` true:

```
ax=loginwindow          layer0=Agterm     1683   the whole lock, correctly named
ax=Notification Center  layer0=Dia           3   AX follows a banner
ax=(none)               layer0=various       9   transient, 0.4%
agreement otherwise                        428
```

Cost: p50 0.38 ms, p90 1.8 ms, p99 10 ms - and one call at **707 ms**, which is why the messaging
timeout `ax.rs` already applies elsewhere has to apply here too.

**AX observers on a background CFRunLoop thread**, main thread deliberately pumping nothing - the
question being whether notifications need the main run loop the way `NSWorkspace` does. They do not.
19 applications attached, and five minutes of ordinary work produced real events:

```
15:34:54.056  pid 83627  AXApplicationDeactivated  -> focused 1538
15:34:54.076  pid 1538   AXApplicationActivated    -> focused 1538
```

Three switches as deactivate/activate pairs 20 ms apart, plus 10 `AXTitleChanged`. So the daemon's
existing event thread can carry focus natively, and the `focus` record kind - in the wire contract
since day one and **never emitted once** - can finally exist, with millisecond timestamps rather than
tick granularity.

Reading yashiki's source settled the design: it takes focus from `AXObserver` per application, filters
the same `CGWindowListCopyWindowInfo` output by `layer == 0` **and** a per-window AX cross-check, and
knows nothing about the lock screen. The rule is worth copying; the dependency is not - see non-goals.

## Context (from discovery)

- `src/macos/window_list.rs` - `frontmost_application()` (the layer-0 heuristic plus a fallback to the
  frozen `NSWorkspace.frontmostApplication`), `focused_owner()`, `MAX_DISPLAYS`.
- `src/macos/ax.rs` - `accessibility_is_trusted()`, `AxApplication::for_pid`, `focused_window`,
  `title`, `document`, and the messaging timeout already applied to AX calls. No system-wide element
  and no observers yet.
- `src/macos/events.rs` - the CFRunLoop thread that already exists, with `NSWorkspace` activation,
  sleep and wake observers and the distributed `com.apple.screenIsLocked`/`Unlocked` pair (line 37).
  Those `NSWorkspace` observers are the dead ones; the thread itself is sound and is where AX
  observers belong.
- `src/providers/windows.rs` - the `Sources` trait and `MacSources`, `Activity`, the `MacEvent`
  receiver with its 300 ms `DEBOUNCE`/1 s `DEBOUNCE_CEILING`, `tick_record`, and the `degraded` flag
  set from the focused-window read.
- `README.md` - the wire contract; the `focus` row of the `(provider, kind)` table describes a record
  that has never been emitted.

## Development Approach

- **Testing approach**: regular - code, then tests, in the same task.
- Every task ends with `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and
  `cargo test` green before the next begins.
- FFI cannot be unit-tested, so each task separates a pure decision function from the calls around it
  and tests that; the live behaviour is covered by `scripts/acceptance.sh` and by the manual pass in
  Post-Completion.
- The record stream must stay backward compatible: `tick` keeps its shape, and `focus` records start
  flowing into a kind the server already accepts and stores.

## Implementation Steps

### Task 1: Ask Accessibility which application is focused
- [x] add `focused_application() -> Option<i32>` to `src/macos/ax.rs`, reading
      `AXFocusedApplication` from `AXUIElementCreateSystemWide()` and returning its pid
- [x] apply the same messaging timeout the module already uses for AX calls - one observed call took
      707 ms and a tick must never block on it
- [x] treat every AX error, including a null result, as "no answer" rather than an error to propagate;
      the caller decides what to do with silence
- [x] write tests for the pure part: an error code maps to `None`, a null element maps to `None`
- [x] add an `#[ignore]`d live test asserting the machine answers with some pid, in the shape
      `screen.rs` already uses
- [x] run `cargo test` - must pass before task 2

### Task 2: Make Accessibility the focus source, with a filtered fallback
- [x] rewrite `frontmost_application()` as: AX first; on no answer, the window list; and **drop** the
      `NSWorkspace.frontmostApplication` fallback, which has been frozen since it was written
- [x] filter the window-list path to owners that are real applications - an owner whose bundle id is
      absent or empty is an overlay, not something a person can be "in"
- [x] keep the pid-only degraded result when `NSRunningApplication` cannot name a pid, as today
- [x] factor the choice into a pure function over (ax answer, window list) so it is testable without
      a window server
- [x] write tests: AX wins when it answers; the window list is used when AX is silent; an
      overlay-shaped entry (empty bundle id) is skipped in favour of the next real one; all-overlay
      input yields nothing rather than an overlay
- [x] run `cargo test` - must pass before task 3

### Task 3: Keep reporting what is running while the screen is locked
- [ ] treat an AX answer of `loginwindow` as no answer, and fall through to the filtered window list,
      which keeps naming the last real window for the whole locked span
- [ ] leave `screen_locked` as the sole marker of the lock; nothing else in the record changes because
      the session locked
- [ ] write a test that a `loginwindow` answer yields the window list's application, not `loginwindow`
- [ ] write a test that a real application from AX is never overridden by the window list
- [ ] document the rule in `README.md`: `app` means what was in front on screen, not what held the
      keyboard focus, and during a lock the two differ
- [ ] run `cargo test` - must pass before task 4

### Task 4: Observe applications from the existing run loop thread
- [ ] add an observer registry to `src/macos/events.rs`: one `AXObserver` per application pid,
      registered on the thread's own run loop, for `AXApplicationActivated`,
      `AXApplicationDeactivated`, `AXFocusedWindowChanged` and `AXTitleChanged`
- [ ] attach on the pids currently owning windows, and re-scan on each tick so an application launched
      later is picked up - the spike missed a switch to a pid it had never attached to
- [ ] detach and release when a pid disappears, so a long-running daemon does not accumulate observers
- [ ] tolerate an application that refuses AX (two of nineteen did): warn once, skip, do not retry in a
      tight loop
- [ ] write tests for the pure registry logic: a new pid is attached, a departed pid is detached, a
      refusing pid is not retried every pass
- [ ] run `cargo test` - must pass before task 5

### Task 5: Turn the notifications into records
- [ ] map the observer callbacks into the existing `MacEvent` channel so they pass through the current
      300 ms debounce rather than emitting a record per notification
- [ ] emit `focus` records on an application activation, with the payload the contract already
      specifies - the kind exists and has never been used
- [ ] let `AXTitleChanged` feed the existing `state_change` path instead of waiting for a tick to
      notice a rewritten title
- [ ] write a test that a deactivate/activate pair 20 ms apart produces one focus record, not two
- [ ] write a test that a title change on the focused application produces a `state_change` and no
      `focus`
- [ ] run `cargo test` - must pass before task 6

### Task 6: Documentation
- [ ] `README.md`: name Accessibility as the focus source, the window list as the filtered fallback,
      and record why the `NSWorkspace` path was removed rather than fixed - so nobody restores it
- [ ] describe the observer lifecycle and the per-tick re-scan, including that an application which
      refuses AX is invisible to focus events
- [ ] state that `focus` records now exist, what they carry, and that their timestamps are the moment
      of the switch rather than the tick that noticed it
- [ ] update the `src/macos/` file table with the new responsibilities

### Task 7: Verify acceptance criteria
- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` green
- [ ] `scripts/acceptance.sh` passes, extended with a live check that AX names a focused application
- [ ] grep the crate for `frontmostApplication` and confirm the frozen call is gone
- [ ] confirm no new dependency was added to `Cargo.toml` beyond features of crates already present

## Technical Details

**An overlay is defined by what it is, not by its name.** The filter is "no bundle id", not a list of
processes. `borders` ships `bundle_id: ""` today, which is what makes the rule checkable against the
data already stored.

**A locked screen must not erase what the machine was doing.** Measured on the 71-minute lock of
29 August: all 113 ticks carried `title: Agterm`, `foreground: claude`, the working directory and the
full command line. That is the only evidence the daemon produces for "the machine worked while nobody
was there" - the third attendance state - and the agterm extractor only runs because the focused
application is Agterm. Reporting `loginwindow` would replace that with a fact `screen_locked` already
states, and blank out the hour. Hence AX is authoritative for who holds focus **while somebody is
there**; during a lock the useful question changes to what is running, and the window list answers it.

**The system-wide focus read needs a warm connection to the application it names** (measured while
building task 1, reproducible on this machine). In a process that has never messaged the focused
application, `AXFocusedApplication` on the system-wide element returns `kAXErrorCannotComplete`
immediately and keeps returning it - six attempts over two seconds, at 0.4 s, 2 s and 5 s messaging
timeouts, all failed. One `AXUIElementCreateApplication` read against the pid currently in front makes
every later system-wide read succeed; the same read against some other application does not. The
daemon warms it by accident - each tick already builds an `AxApplication` for the front pid to read
its title - so the practical cost is that the first tick after a switch to an application never
messaged before falls through to the window list. The live test warms it deliberately for the same
reason.

**AX silence is normal.** Nine samples in 2123 returned nothing, and two of sixteen observer callbacks
could not resolve the focused application from inside the callback. Silence must fall through to the
window list, never to an error and never to a fabricated answer.

**The observers are attached from the run loop thread, and only from there.** `AXObserverGetRunLoopSource`
must be added to the run loop that will actually run; adding it from another thread attaches it to a
loop nobody pumps, which is the same class of mistake that left the `NSWorkspace` observers dead.

## Post-Completion

**Manual verification**:
- switch applications a few times and confirm `focus` records appear with the right application and
  sub-second timestamps
- lock the screen with something running in an agterm pane, confirm the span keeps the application,
  title, cwd, command and foreground it had before the lock, carries `screen_locked: true`, and that
  `borders` never appears as a focused application
- leave it running a full day and compare the application histogram against 28 August, where `borders`
  held 32 minutes

**External systems**:
- no server change is required: `focus` is already in the contract, validated and stored
- the day screen will start seeing `focus` event rows it currently ignores; deciding whether to render
  them is a design question, not part of this plan

## Non-goals

- **No dependency on yashiki.** Its focus comes from the same AX observers this plan adds, it knows
  nothing about the lock screen, and making it the source of record would mean a daemon that cannot
  answer the basic question on a plain Mac - plus a fallback path implemented anyway.
- **No tags or spaces.** Which tag set is visible is the one thing yashiki knows and Accessibility does
  not, and it is worth its own plan as an optional enrichment in the shape of the agterm extractor.
- **No fix for the dead `NSWorkspace` sleep and wake notifications.** They are replaced for the
  unattended-span purpose by the polled screen state shipped in 0.3.0; reviving them is separate.
- **No change to the tick.** Duration, idle, input counters, microphone and screen state stay polled;
  events add precision to transitions, they do not replace the sampling that measures time.
