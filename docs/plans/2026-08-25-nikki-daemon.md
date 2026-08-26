# nikki daemon - macOS Activity Capture

## Overview

A Rust daemon, one per Mac, that records what was on screen and what was being done, and ships it to the nikki service. It is the client half of a self-hosted replacement for the capture half of Toggl Track. The service half is a Go service in a different repository; the only thing shared between them is the wire contract, reproduced verbatim below so this plan is self-contained.

Toggl's Activity view reports only which application was frontmost - `WezTerm 64%, Dia 20%` - and never what was being done in it. This daemon captures the missing layer: window titles, the open document path, the browser tab and profile, the terminal workspace and working directory, plus real activity signals (input volume, idle, lock, sleep, microphone).

**Acceptance scenario** (what everything here aims at):

> In the evening the agent calls the service API, gets the day, and produces a summary: what was worked on, how long on each thing, where the gaps are.

This daemon's job is that the data behind that summary is complete and faithful. Interpretation happens elsewhere.

## Execution environment

**Every task runs natively on macOS.** This is not an aside - it decides whether the gates mean anything.

From Task 2 onward the crate links Apple frameworks (`AXUIElementCreateApplication`, `CGWindowListCopyWindowInfo`, `CGGetActiveDisplayList`, `NSWorkspace`, `CGDisplayRegisterReconfigurationCallback`, CoreAudio). The crates providing them emit `#[link(kind = "framework", ...)]`, which rustc rejects on a non-Apple target, and both `cargo test` and `cargo clippy --all-targets` link binaries. So in a Linux container the crate does not build at all from Task 2 to the end, and a session that meets that wall either blocks or ticks a checkbox over a build it never saw pass - which silently disarms every later gate, including the unsafe-containment grep.

Cross-compiling is not an escape: linking against macOS frameworks needs the Apple SDK.

The alternative - `#[cfg(target_os = "macos")]` plus a stub backend - was considered and rejected. It would compile the whole `macos` module out on Linux, so the module holding every `unsafe` block and all the Core Foundation release discipline would never be compiled by the thing running the gates, while the gates reported success. That is more work for less verification exactly where verification matters most.

Running natively costs nothing here, because **nothing in this plan runs the daemon as a daemon during the build.** Gates compile the crate and run unit tests over committed fixtures - rectangles, captured JSON, a small history database. No Accessibility grant, no GUI, no permission prompt, no live observation. Task 10 briefly launches the binary against a local stub HTTP server; even that needs no permissions and ships nowhere.

### Non-goals

- **No interpretation.** The daemon never decides what an activity means.
- **No screen capture, no OCR, no screenshots.** This is what keeps the Screen Recording permission - and macOS's monthly re-consent dialog for it - out of the design entirely.
- **No keystroke content.** Only counts, from a counter that cannot see content.
- **No clipboard capture.**
- **No release pipeline.** Signing, notarisation, tap publication and the LaunchAgent are handled separately and appear only in Post-Completion. This plan produces a binary and an app bundle that run locally.
- **No dependency on any third-party window manager.** v1 uses only system APIs. The window source sits behind a trait so another source can be added later as its own plan.
- **No shell, git or agent-session providers.** The provider architecture lands now so they slot in later; only windows and browser history are implemented.
- **No profile filtering in the window extractor.** The configured profile governs which browser history is read; the tab extractor reports whatever profile is in front. A rarely-used profile therefore still leaves tab entries in window samples, which is accepted - this is a local tool over local data.

### Rejected alternatives

- **Swift.** Smaller in the FFI layer, but the Mac side also tails local SQLite databases, and one Rust binary beats a Swift daemon plus a Go sidecar. The usual argument against Rust here - that a bare binary holds TCC permissions poorly - does not apply: the deliverable is a signed `LSUIElement` app bundle, which holds Accessibility across upgrades regardless of the language inside it.
- **`CGWindowListCopyWindowInfo` as the source of window titles.** `kCGWindowName` is redacted without the Screen Recording permission. Titles come from Accessibility instead, which needs no such grant; CGWindowList is used only for the window list, geometry and z-order, all of which it returns without any permission.
- **`NSScreen` for display geometry.** `NSScreen.frame` is in Cocoa coordinates (origin bottom-left of the primary display, y upward) while `kCGWindowBounds` is flipped (origin top-left, y downward), so comparing them requires a conversion that is easy to get wrong and hard to notice when wrong. `CGDisplayBounds` returns display rectangles already in the same flipped space as window bounds, so the conversion disappears.
- **`AVCaptureDevice` for microphone state.** Opening a capture device requests the Microphone permission and lights the orange indicator. `kAudioDevicePropertyDeviceIsRunningSomewhere` answers the same question by querying a property, without opening the device, without the permission and without the indicator.
- **SQLite's online backup API for reading browser history.** Rejected on evidence, see the browser-history section: with the browser running, an external reader cannot acquire a shared lock on the active profile at all, and `sqlite3_backup_step` takes exactly the same lock an ordinary reader takes.

## Skills to invoke

None registered for Rust in this setup. The conventions that apply are in this repository's `CLAUDE.md`, written in Task 1.

## Context

### Permissions

| Permission | For | Indicator / prompts |
|---|---|---|
| Accessibility | window titles, focused window, title-change events | none |
| Automation -> Dia | tab URL, title, profile | one-time prompt on first use |
| (none needed) | window list, geometry, z-order, displays, idle, input counters, lock, sleep, mic-in-use | none |

Screen Recording is deliberately not requested. Holding it triggers a macOS re-consent dialog roughly monthly which cannot be disabled, in exchange for a field this design gets from Accessibility instead.

### What the system APIs give, verified on the target machine

**Window list** - `CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)` returns, without any permission, an array ordered front-to-back where each entry carries `kCGWindowOwnerPID`, `kCGWindowOwnerName`, `kCGWindowNumber`, `kCGWindowBounds` and `kCGWindowLayer`. Ordinary application windows are `kCGWindowLayer == 0`; everything else is chrome, overlays and system UI and is dropped. `kCGWindowName` is present only with Screen Recording, so it is never read.

There is **no minimised flag** to ask for and none is needed: `.optionOnScreenOnly` already excludes minimised windows by construction. Do not invent a key for it.

**Displays** - `CGGetActiveDisplayList` plus `CGDisplayBounds(displayID)` gives each display's rectangle in the same coordinate space as `kCGWindowBounds`.

**Titles** - `AXUIElementCreateApplication(pid)` then `kAXWindowsAttribute` then `kAXTitleAttribute`. Verified to return titles for 31 of 31 running applications with no failures. `AXUIElementSetMessagingTimeout` is mandatory - an unresponsive application otherwise blocks the call indefinitely.

**Open document** - `AXDocument` on a window returns a `file://` URL for real document applications. Verified: present on 5 of 18 applications with windows; meaningful on two, and they are the two that matter for this user - an editor returned `file:///Users/pavel.karpovich/Projects/DC/ask-dealcloud/agent-sdk-runtime/...` and a git client returned its repository root. Electron applications return an empty string and one media application returns its own `https://` internal URL, so the rule is: keep the value only when it is non-empty and its scheme is `file`.

**Activity** - `CGEventSourceSecondsSinceLastEventType(.combinedSessionState, .anyInputEventType)` for idle seconds, and `CGEventSourceCounterForEventType` for cumulative key and mouse event counts since boot, differenced between ticks. Neither needs a permission and neither can observe content.

Two conversions that are silent when wrong. The idle API returns a **double**, while the wire field is an integer the server types `*int64` - a fractional value fails to decode and the whole tick is rejected and then deleted, so it is truncated toward zero before it goes anywhere near a payload. And the counters are cumulative since **boot**, so the first difference after every daemon start has no previous sample to subtract: that first tick reports `keys_delta` and `mouse_delta` of 0 and starts the baseline, rather than shipping the entire since-boot total as if it happened in one 30-second interval.

**Microphone** - `kAudioDevicePropertyDeviceIsRunningSomewhere` on the default input device. Verified returning `status=0, isRunningSomewhere=1` while another application held the device. It means "the device is running", not "someone is listening" - some applications hold it open idle - so it is captured as a hint and labelled as one.

### The event thread

`AXObserver` callbacks, `NSWorkspace` notifications, distributed notifications and `CGDisplayRegisterReconfigurationCallback` are all delivered by a **CFRunLoop**, and none of them fire unless some thread is running one. A tokio worker is not a run loop, so registering sources without saying where they run produces a daemon whose event half is silently dead - ticks keep arriving, so it looks alive.

The daemon therefore owns exactly one dedicated OS thread whose job is the run loop:

- it calls `CFRunLoopGetCurrent()` once and keeps that reference;
- every observer source is added to **that** run loop (`CFRunLoopAddSource` with `kCFRunLoopDefaultMode`), including the `AXObserver` re-registered whenever the frontmost application changes;
- it then calls `CFRunLoopRun()` and stays there;
- each callback converts its payload into a plain Rust event value and sends it into a `tokio::sync::mpsc` channel; no callback touches provider state directly - **with exactly one exception, `willSleep`, described below**;
- a callback that carries transient state - which window was focused, which application was frontmost - captures that state **at callback time** and puts it in the event value. Resampling it when the event is drained reads the world after the transition it was meant to record, which for a rapid switch means recording the destination twice and the origin never;
- shutdown is `CFRunLoopStop` on the stored reference, then joining the thread.

Everything else in the daemon is ordinary async Rust reading that channel.

**The `willSleep` exception, and why it has to be one.** The service's liveness rule treats a `sleep` marker as expected quiet and alarms without one, so a marker that only reaches the server after wake inverts the check: the machine reads stale all night and healthy the moment it returns. Preventing that requires the record to be shipped *before* macOS suspends, and the only thing that can delay the suspension is blocking inside the notification callback itself - a handler that posts to a channel and returns has already let the machine sleep, whatever the code downstream does afterwards.

So `willSleepNotification` alone sends its event with a completion handle and **blocks on the runtime's flush acknowledgement for at most 2 seconds** before returning. macOS grants a short window before sleeping and blocking past it risks the process being killed, so the budget is a hard ceiling rather than a target: on expiry the callback returns anyway and the record ships on wake, which is no worse than not having tried. This lives in `events.rs`, whose thread owns the handler - placing it in the window provider would put it downstream of the very channel that makes the barrier impossible, and a test written against the provider's handler would pass without any barrier existing.

### Extractor contracts, captured live

These are the exact forms to implement against. Each is committed as a fixture in the task that parses it.

**Dia (browser tab).** The naive one-line form returns fields joined by `, `, which is ambiguous for any title containing a comma - and the ambiguity lands in the URL field. Use the unit separator instead, which cannot occur in a URL or a title. Verified working, including the not-running guard, which must come first because `tell application "Dia"` would otherwise launch the browser:

```applescript
if not (running of application "Dia") then return ""
tell application "Dia"
  set w to front window
  set t to active tab of w
  set u to (URL of t) as text
  set ti to (title of t) as text
  set pr to (name of active profile of w) as text
  set pn to (isPinned of t) as text
end tell
set AppleScript's text item delimiters to (ASCII character 31)
return {u, ti, pr, pn} as text
```

Captured output (`^_` is `0x1F`):

```
https://homeassistant.pkarpovich.space/home-v2/home^_Home – Home Assistant^_MBP_21^_false
```

An empty result means Dia is not running. **Errors arrive on stderr with a non-zero exit**, in this captured form:

```
220:224: execution error: Can't make {...} into type text. (-1700)
```

The stable part is the trailing parenthesised code, which is what the extractor matches: `-1743` Automation denied, `-1728` no front window, `-1712` Apple event timeout, and anything else unknown. All four branch identically - return empty - but `-1743` is logged at warn level once per process because it means the one-time Automation prompt was declined and no tab will ever be captured until a human fixes it, while the others are transient and logged at debug.

Note the limitation this form carries: it reads the browser's own front window, so when Dia is not the focused application the tab returned belongs to a window the user may not be looking at. The extractor is therefore called only when Dia is the focused application - see the extractor order.

**agterm (terminal workspace and session).** The response is not flat; the envelope is three levels deep. Verified shape:

```json
{"ok": true, "result": {"tree": {
  "sidebarVisible": true, "quickVisible": false, "idleMs": 44119, "workspaceFilter": false,
  "workspaces": [
    {"id": "839DBFAA-...", "name": "tuclaw", "active": false, "sessions": [
      {"id": "7320056A-...", "name": "new tuclaw desktop app", "title": "tuclaw: done",
       "cwd": "/Users/pavel.karpovich/Projects/tuclaw", "active": false, "flagged": true,
       "split": false, "scratch": false, "overlay": false, "realized": true, "fontSize": 18,
       "foreground": ["/Users/pavel.karpovich/.local/bin/claude", "--enable-auto-mode", "--resume"],
       "restoreCommand": "...", "surfaces": [{"kind": "left", "active": true, "visible": true}]}
    ]}
  ]}}}
```

Three facts that land in code. `foreground` is an argv array of absolute paths, and it is `null` for a session running nothing (2 of 19 sessions live) - deserialising it into a non-optional type panics, and a panic here is silent data loss because the extractor's error path returns empty. The value stored is the file name of `foreground[0]`, so the array above reduces to `claude`. Selecting the on-screen session is `workspaces[].active == true` then that workspace's `sessions[].active == true`; when no workspace or no session is active the extractor returns empty rather than guessing. The full tree is large (6 workspaces, 19 sessions on the live machine, nearly all off-screen), so everything except the active session is discarded.

The command is `agtermctl tree --json`. Its location is resolved at runtime: `agtermctl` on `PATH`, falling back to `/Applications/agterm.app/Contents/MacOS/agtermctl`. Absent, the extractor returns empty and logs once.

### Browser history

**Which profile.** Chromium stores profiles in numbered directories with display names held elsewhere, and the two do not match. Verified on the target machine, from `~/Library/Application Support/Dia/User Data/Local State`:

```
Default    -> MBP_21     (this Mac)
Profile 1  -> MBA_22
Profile 2  -> Intapp
last_used: Default
```

The directory name is an implementation detail; the display name is what the user knows, what the tab extractor already returns, and therefore what the `profile` field must carry - otherwise one column in one table means a directory in some rows and a profile in others.

The mapping lives in `~/Library/Application Support/Dia/User Data/Local State`, a plain JSON file the browser does not lock. Only one path in it is read:

```json
{"profile": {"info_cache": {
  "Default":   {"name": "MBP_21"},
  "Profile 1": {"name": "MBA_22"},
  "Profile 2": {"name": "Intapp"}
}, "last_used": "Default"}}
```

The daemon reads `profile.info_cache`, finds the entry whose `name` equals the configured value, and uses that entry's key as the directory. Every other field in the file is ignored, and unknown fields are not an error - it is a large browser-owned document that will grow. An entry with a missing or non-string `name` is skipped rather than failing the whole read. This file is committed as a fixture and parsed by a test, because it gates which database is read at all and a silent mismatch means capturing nothing.

So **config names the display name**, the daemon resolves it to a directory through `Local State`, and the payload always carries the display name. The resolution is redone on every poll rather than cached at startup, because a profile can be renamed in the browser and the daemon must not keep reading the directory that name used to point at. A configured name absent from `Local State` is a startup error listing the names that do exist; `last_used` is deliberately not a default, since it would silently change what is captured whenever the user opens another profile.

Exactly one profile is read. Multi-profile capture is not a goal.

**How the file is read.** The browser holds the active profile's `History` open, and this is stronger than a contention problem. Verified with the browser running:

```
$ sqlite3 "file:Default/History?mode=ro" "PRAGMA busy_timeout=8000; SELECT count(*) FROM visits;"
Error: in prepare, database is locked (5)
```

It waits out the full timeout every attempt, while the two idle profiles open instantly. `sqlite3_backup_step` takes the same shared lock an ordinary reader takes, so the online backup API fails identically - and since only the active profile is ever read, this is not an edge case but the normal one.

**Copying works where opening does not**, because SQLite's locks are advisory locks over a file that `read()` ignores. Verified: a copy of the locked database opens read-only and returns current data - 929 885 visits, the newest seconds old.

The copy is made with `clonefile` (`cp -c`), which on APFS snapshots the block map: measured at **0.00 s** for a 233 MB database, no I/O and no space until either file diverges. A plain byte copy also worked (0.26 s) but is not relied on. The copy is deleted immediately after the read, so the poll is effectively free and can stay frequent.

**Consistency.** The clone is a point-in-time snapshot of the file, but SQLite may be mid-transaction, so the copy can contain partially written pages. The `History-journal` sidecar is cloned alongside it (this database uses a rollback journal, not WAL - verified: `History-journal` present at 8 720 bytes beside a 233 MB `History`), and SQLite rolls the incomplete transaction back when the copy is opened. Two clones are still two instants and cannot be made atomic with respect to each other, so the rule is: **open the copy, run `PRAGMA quick_check`, and discard the copy and skip the poll unless it returns `ok`.** Failing to open is not the only bad outcome and not even the likely one - a torn snapshot frequently opens cleanly and reads as good data, which would ship wrong rows and advance the cursor past the right ones. Nothing is lost by skipping, because the cursor advances only over rows actually read and the next poll costs nothing.

**Schema and conversions**, verified against the live databases:

```sql
SELECT v.id, v.visit_time, v.transition, v.visit_duration, u.url, u.title
FROM visits v JOIN urls u ON u.id = v.url
WHERE v.id > ?1
ORDER BY v.id
LIMIT 5000;
```

`visit_time` is microseconds since 1601-01-01 UTC: `unix_seconds = visit_time / 1000000 - 11644473600`, verified to produce correct local wall-clock times.

`visit_duration` is **also microseconds**, and the wire field is milliseconds, so the daemon divides by 1000. Verified: raw values of `450404`, `1076461` and `22874541` correspond to 0.5 s, 1.1 s and 22.9 s of page time - plausible as microseconds, six hours as milliseconds. Shipping the raw value would make a 90-second read report as an hour and a half, with nothing erroring, because the only value pinned in the contract is `0` and zero looks identical in both units.

`transition` is a bitfield stored raw: the core type is the low byte (`transition & 0xFF`), the high bits are qualifiers - live values include `805306368` (`0x30000000`) and `822083584` (`0x31000000`). The daemon stores the integer and interprets nothing.

**Visit rows are mutable, and the cursor must account for it.** Chromium writes the row when the visit begins and fills `visit_duration` when it ends - an UPDATE to the same row under the same `visits.id`. A strictly forward cursor (`id > cursor`) can never see that update, so every page still open at poll time, and every page held longer than the poll interval, would ship `duration_ms: 0` and stay zero forever - which is exactly the long reads that matter most.

So each poll **reads** `id > cursor - revisit_window` (default 500 rows) rather than `id > cursor`, and the service accepts a correction: a repeated `browser_history/visit` updates `title`, `transition` and `duration_ms` guarded by `excluded.seq > web_visits.seq`.

**Reading a row is not shipping it.** A re-read row is emitted only when its `title`, `transition` or `visit_duration` differs from what was last shipped for that `visits.id`; the last-shipped triple is persisted per profile alongside the cursor and pruned to the same window. This is not an optimisation, it is the difference between a bounded daemon and an unbounded one. Shipping every re-read row would emit 288 polls x 500 rows = about 144 000 records a day on an idle browser, and none of them would be free on the server: the guard is `excluded.seq > web_visits.seq` and every shipment carries a freshly allocated, strictly higher `seq`, so the UPDATE fires every single time and drags the FTS delete-and-insert triggers with it. The buffer cap below is sized for roughly 3 000 records a day; at 147 000 it holds about a day and a half, and the first weekend outage would evict the window samples - the irreplaceable half - to make room for visit rows already stored.

**Identity, cursor and first run.** `visits.id` is a per-profile autoincrement, which makes it both the cursor and the identity: the cursor is the highest `id` shipped for that profile, and the dedup key is derived from `(device, profile, visit_id)` rather than from the URL - which also keeps the redacted-away path out of the key.

Two cases need stating because they are silent when wrong.

**First run** has no cursor, and backfilling 929 885 historical visits is not the intent - the daemon starts from the highest `id` present at first poll minus `revisit_window`, so it captures from installation forward.

**A database replaced or reset** (profile deleted and recreated, history cleared) restarts ids from a low value. Detecting it (`max(id) < cursor`) and resetting the cursor is necessary but **not sufficient**, and resetting the cursor alone is actively harmful: the new database's `visits.id` 1..N reuse identifiers the old database already spent, so the daemon would ship records whose `dedup_key` collides with unrelated stored visits - and the server now treats a colliding key as a *revision*, updating the old row's title and duration while keeping its timestamp and URL. Two unrelated visits merge under the older one's identity and the server reports success.

So the daemon persists a **generation** counter per profile beside the cursor, starting at 1 and incremented whenever a reset is detected, and the browser dedup key includes it:

```
device \x1F "browser_history" \x1F profile \x1F generation \x1F visit_id
```

A generation of 1 is not omitted - the field is always present - so the key shape never varies. The reset itself is logged at warn.

**Host-less URLs.** A browser history is not only http: the live database holds 127 `chrome-extension://` rows, 47 `file:///` rows and 4 `data:` rows. They do not all behave the same way, and conflating them is how the redaction rule ends up never firing for the majority:

- `file:///Users/...` and `data:...` genuinely parse into an **empty** `Host`. Redaction reduces them to their scheme alone (`file:///`), and the server stores them with `host` NULL rather than rejecting - rejection would be destructive, because it makes the daemon delete the record and the cursor has already passed it.
- `chrome-extension://<id>/page.html` has an authority, so it parses with `Host` set to the extension id and is **not** host-less. It goes through the ordinary host-only rule and ships as `chrome-extension://<id>/`. That is the desired outcome - the extension id is the useful part and the path is stripped like any other - but it must not be described as a host-less case, or an implementer writing the reduction will find it never applies to 127 of the 178 rows it was written for.

### Toolchain

Rust 1.98 (the latest mise offers) with `rustfmt` and `clippy`, pinned in `.mise.toml`. Crates are taken at their latest release at implementation time; verify each version rather than assuming one.

## Development Approach

- **Testing approach**: regular (code first, then tests), except the visibility resolver, which is written test-first because it is a pure function that is easy to specify and easy to get subtly wrong.
- Complete each task fully before starting the next.
- **Every task that adds Rust code must add or update tests for it**, listed as separate checklist items.
- **Tests live in inline `#[cfg(test)] mod tests` blocks in the file they cover.** A sibling `foo_test.rs` is not compiled unless something declares it with `mod foo_test;`, so such a file would sit unbuilt while its task's gate reported success.
- **Every new module file must be declared** - `mod x;` in its parent `mod.rs` or in `main.rs` - as an explicit checklist item in the task that creates it. An undeclared module is not compiled, and neither are its inline tests, so the gate goes green over code that does not exist as far as the compiler is concerned.
- **All tests must pass before the next task starts.**

**Carve-out.** Task 10 adds **test-support** Rust (a stub server and a test seam in `events.rs`) rather than production code, so it carries no unit-test checkbox of its own - the harness it builds *is* its verification - but it is still a Rust task and still runs the per-task gate. Task 11 (documentation) adds no Rust at all and runs no gate. **A review session must not fail either for missing tests**; what verifies them is stated inline.

## Code-Quality Rules

- **Every `unsafe` block lives in the `macos` module and nowhere else.** The rest of the daemon must never see a raw pointer. This includes the CGWindowList and CGDisplay wrappers, not only the Accessibility ones.
- Core Foundation memory discipline is the live bug class here. A value obtained from a `Copy` or `Create` function is owned and must be released on **every** path including error returns; a value obtained from a `Get` function is not. An Accessibility attribute is not guaranteed to be of the type its name suggests - check the type id before wrapping, and release before returning the error when it does not match.
- No comments; clear names instead.
- A provider must never panic the process. A failing provider restarts alone.
- Every subprocess call has a deadline and is killed when it expires.

**Per-task gate** (run before marking any Rust checkbox `[x]`):

```
mise run check      # cargo fmt --all -- --check; cargo clippy --all-targets -- -D warnings; cargo test
! grep -rn 'unsafe' src --include='*.rs' | grep -v '^src/macos/'
```

The grep enforces unsafe containment mechanically. Each Rust task's final checkbox is "run the per-task gate - must pass before Task N+1"; Task 11 adds no Rust and is the one task without it.

## Testing Strategy

- **Unit tests** on every Rust task.
- **Visibility resolution is tested exhaustively and first**, from fixture window and display rectangles: a window wholly on one display, wholly off every display, straddling two, occupying a single pixel of a display, on a display absent from the list.
- **Parser fixtures**: the captured Dia output, the captured agterm JSON and a small anonymised Chromium history database are committed and parsed by tests.
- **Durability**: the buffer survives a simulated restart; a cursor never advances past an uncommitted enqueue; a full buffer drops oldest and emits an overflow record.
- **Degradation**: a hung extractor yields an empty enrichment while capture continues; a missing extractor binary yields empty; the service unreachable leaves records buffered and they drain on recovery.
- **Redaction**: no redacted path or query appears anywhere in a buffered or shipped envelope, including inside `dedup_key`.

## Progress Tracking

- Mark completed items `[x]` immediately.
- New tasks get a `+` prefix, blockers a `!` prefix.

## Solution Overview

One binary inside an `LSUIElement` app bundle. A runtime hosts independent **providers**: the window provider is event-driven with a heartbeat tick, and the browser-history provider runs on a timer and ships deltas from a persisted cursor. The runtime owns a durable SQLite buffer, a monotonic sequence counter, redaction, batching, retry and cursor advancement. A crashing provider restarts alone and never stops the others.

## Technical Details

### The wire contract

The boundary between two repositories, pinned with captured bodies. The service repository carries the identical section, and any change here must be mirrored there in the same pass.

**Request** - `POST {service}/api/v1/records`, `Content-Type: application/json`:

```json
{
  "records": [
    {
      "provider": "windows",
      "device": "mbp-21",
      "ts": "2026-08-25T13:55:52.481Z",
      "seq": 41207,
      "kind": "tick",
      "dedup_key": "b6f1c0e4a37d92f5",
      "degraded": false,
      "payload": {
        "app": "Zed",
        "bundle_id": "dev.zed.Zed",
        "title": "environment, home-environment, workspaces — settings.json",
        "path": "file:///Users/pavel.karpovich/Projects/environment/dotfiles/mise/config.toml",
        "details": {"workspace": null},
        "display": 1,
        "tick_interval_sec": 30,
        "idle_sec": 3,
        "keys_delta": 184,
        "mouse_delta": 22,
        "mic_active": false,
        "visible": [
          {"app": "Dia", "bundle_id": "company.thebrowser.dia", "title": "Home – Home Assistant", "title_reason": null, "display": 2, "z": 1},
          {"app": "Telegram", "bundle_id": "ru.keepcoder.Telegram", "title": null, "title_reason": "ambiguous", "display": 2, "z": 4}
        ]
      }
    },
    {
      "provider": "browser_history",
      "device": "mbp-21",
      "ts": "2026-08-25T13:55:56.000Z",
      "seq": 41208,
      "kind": "visit",
      "dedup_key": "9c02aa17be44d310",
      "degraded": false,
      "payload": {
        "url": "https://homeassistant.pkarpovich.space/",
        "title": "Home – Home Assistant",
        "profile": "MBP_21",
        "transition": 805306368,
        "visit_id": 929269,
        "duration_ms": 0
      }
    }
  ]
}
```

The `url` above is in its **post-redaction** form, which is what the wire actually carries: the default rule keeps the host only. A host-only value ships as a **scheme-bearing URL with an empty path** - `https://host/` - and never as a bare hostname, because a bare hostname parses without error into an empty `Host` and the server would silently store an empty `host` column on a field it indexes. A URL with no host at all reduces to its scheme (`file:///`).

`profile` carries the **display name** (`MBP_21`), never the directory name.

`display` is the **zero-based index into `CGGetActiveDisplayList`**, not a `CGDirectDisplayID`. The trade-off was weighed and accepted: an index shifts when a monitor is attached or detached, so a value recorded before a reconfiguration stops meaning the same physical screen afterwards and the server's run merge key splits a run across the change. This machine's monitors are permanently attached, which makes the stable-identifier alternatives - an opaque `CGDirectDisplayID`, or a display UUID that would force the field to a string and change the server schema - cost more than they buy. The server never branches on the value; `coalesce.go` only compares it for equality.

**Every `(provider, kind)` pair the daemon emits, with its payload contract.** The server validates per pair, not per provider, and a rejected record is deleted rather than retried - so a kind missing from this table is destroyed permanently the first time it is sent.

| provider | kind | required payload fields | optional |
|---|---|---|---|
| `windows` | `tick` | `app`, `bundle_id`, `display`, `tick_interval_sec`, `idle_sec`, `keys_delta`, `mouse_delta`, `mic_active`, `visible` | `title`, `path`, `details` |
| `windows` | `focus` | `app`, `bundle_id`, `display` | `title`, `path`, `details`, `visible` |
| `windows` | `state_change` | `app`, `bundle_id`, `display` | `title`, `path`, `details`, `visible` |
| `windows` | `lock`, `unlock`, `sleep`, `wake` | none | none |
| `windows` | `buffer_overflow` | `details` carrying `dropped`, `dropped_from`, `dropped_to` | none |
| `browser_history` | `visit` | `url`, `profile`, `visit_id` | `title`, `transition`, `duration_ms` |

Unknown payload fields are never rejected - the server preserves them in its `raw` column and ignores them.

**`tick_interval_sec` must ride on every tick, and the server bounds it to `[1, 3600]`.** A tick whose interval is absent, `<= 0` or `> 3600` is rejected per-record, the response is still `200`, and the daemon's own rule then deletes it - so a misconfigured interval destroys the entire window stream while `lock`, `wake` and browser visits keep succeeding, leaving the device reading `ok` on a timeline empty of work. The daemon therefore validates `tick_interval` against the same bound **at startup** and refuses to run outside it, rather than shipping records that will be thrown away.

Carrying the interval per record rather than assuming a server-side global is what keeps a second Mac on a 60-second interval from halving that day's reported totals, and keeps historical rows meaning what they meant when recorded.

**`state_change`** reports that something about the focused window changed without the focus itself moving: a title rewrite, a window created or destroyed, a display reconfiguration. It exists because none of those fire an application activation or a focus change, and a title rewrite is the only signal for switching files inside an editor, a build finishing and rewriting a terminal title, or switching between two already-loaded tabs in one browser window - which writes no history row either. The server breaks a coalesced run on it, exactly as it does on `lock` and `sleep`, and gives it no duration: a state change is an instant, and the tick that follows opens the run where the duration lives.

**Caveat, and it is the server's to fix rather than this plan's.** The shipped coalescer ends a broken run at *the last tick plus its full interval*, not at the event's own timestamp, and `focus` is not in its breaking set at all. So with an A tick at 12:00, a switch at 12:00:05 and a B tick at 12:00:30, A is credited through 12:00:30 whether or not a `state_change` was sent. Emitting the record is necessary and is this daemon's whole responsibility here, but the duration only becomes correct once the service closes a run at the event timestamp and starts the next one from there. Nothing in this plan can compensate for that, and it should not try.

Captured bodies for the kinds not shown in the batch above:

```json
{"provider":"windows","device":"mbp-21","ts":"2026-08-25T12:04:11.002Z","seq":41190,
 "kind":"lock","dedup_key":"31aa90b2c7e05f18","degraded":false,"payload":{}}

{"provider":"windows","device":"mbp-21","ts":"2026-08-25T12:41:07.884Z","seq":41191,
 "kind":"wake","dedup_key":"7d51e8039ab2c460","degraded":false,"payload":{}}

{"provider":"windows","device":"mbp-21","ts":"2026-08-25T13:02:55.120Z","seq":41192,
 "kind":"focus","dedup_key":"c0934be7712af8d5","degraded":false,
 "payload":{"app":"Dia","bundle_id":"company.thebrowser.dia","display":2,
            "title":"Home – Home Assistant","details":{"url":"https://homeassistant.pkarpovich.space/","profile":"MBP_21"}}}

{"provider":"windows","device":"mbp-21","ts":"2026-08-26T09:14:03.556Z","seq":52118,
 "kind":"state_change","dedup_key":"4f83a0c15d29e7b6","degraded":false,
 "payload":{"app":"Zed","bundle_id":"dev.zed.Zed","display":1,
            "title":"nikki, turtle-hub — coalesce.go","path":"file:///p/coalesce.go"}}

{"provider":"windows","device":"mbp-21","ts":"2026-08-25T13:10:00.000Z","seq":41193,
 "kind":"buffer_overflow","dedup_key":"5e2b71c8804fda93","degraded":false,
 "payload":{"details":{"dropped":20000,"dropped_from":"2026-06-14T08:00:00.000Z","dropped_to":"2026-06-21T19:30:00.000Z"}}}

{"provider":"browser_history","device":"mbp-21","ts":"2026-08-25T14:02:19.310Z","seq":41204,
 "kind":"visit","dedup_key":"a70f31d9b8c2e546","degraded":false,
 "payload":{"url":"file:///","title":"Coverage report","profile":"MBP_21",
            "transition":805306368,"visit_id":929402,"duration_ms":0}}
```

**Response** - always `200` when the request parsed:

```json
{"accepted": 1, "duplicates": 0, "rejected": [{"index": 1, "reason": "unknown provider \"shell\""}]}
```

**How the daemon branches on the outcome**, which is the whole reason the contract is pinned:

- `200` - delete every record of the batch from the buffer, including those in `rejected`. A rejected record will never be accepted by retrying it, so keeping it would block the queue forever. Log each rejection at warn level with its reason.
- `401`, `403`, `404`, `405` - these are **configuration**, not malformed data: a wrong `service_url`, a proxy in front, a service not deployed yet. Dead-lettering here would feed the entire capture into a bin one batch at a time while the daemon looked healthy. Keep the batch, back off, and log at error every time - the operator has something to fix and the records must survive until they do.
- other `4xx` (400, 409, 422) - the batch is malformed in a way retrying cannot fix. Move it to `dead_letter`, log at error, continue with the next batch.
- `413` - halve the batch size and retry, down to a floor of 10 records; below that, dead-letter.
- `5xx`, timeout, connection failure - keep the batch and back off. This is the only path that retries the same bytes.

A `200` whose body does not parse as the documented shape, or whose counts do not add up to the batch size, is treated as `5xx`: the batch is kept and retried. Deleting records on the strength of a status line alone would discard them on any proxy or misconfiguration that answers 200 with something else.

Without this split one permanently unacceptable record stalls shipping forever while `last_seen_at` freezes, which the service reports as a dead daemon - sending the operator to look at a daemon that is running perfectly.

`ts` is RFC 3339 with millisecond precision in UTC and is when the event happened. `seq` is a per-device monotonic counter persisted in the buffer database, so it survives restart and never restarts at zero - a counter that resets would collide with keys already stored earlier the same day, and the collisions would be reported as duplicates, which looks like success.

`dedup_key` is `sha256` truncated to 16 hex characters over unit-separator-joined fields:

- windows: `device \x1F "windows" \x1F kind \x1F ts_millis \x1F seq`
- browser: `device \x1F "browser_history" \x1F profile \x1F generation \x1F visit_id`

No URL, path or title ever enters a key, so redaction cannot be defeated through it. The browser key deliberately excludes `seq`, so a revision of the same visit carries the same key and the server can recognise it as a correction - and includes `generation` so a replaced history database, whose ids restart from 1, cannot collide with visits already stored under the old one.

**The key is derived inside the enqueue transaction, after `seq` is allocated.** The window key hashes `seq`, so it cannot be computed before the counter that produces it; a pipeline of "redact, then key, then enqueue" is unimplementable, and the tempting resolution - a placeholder `seq` of 0 - degenerates the key to `device|windows|kind|ts_millis|0`, which collides for any two records of the same kind in the same millisecond and is reported by the server as a duplicate, which looks like success. Redaction runs first and separately, because it must happen before anything is written to disk; keying is part of enqueue.

### Providers

```rust
#[async_trait]
trait Provider {
    fn name(&self) -> &'static str;
    async fn run(&mut self, ctx: Ctx, out: Sender<Emission>) -> Result<()>;
}

struct Emission {
    records: Vec<RecordDraft>,
    cursor: Option<Cursor>,
}
```

A provider emits records and, when it has one, the cursor those records advance it to - **together, in one message**. The runtime writes the pending envelopes and the cursor in a single SQLite transaction before acknowledging. This is not a style choice: with two independent operations, a provider that reads visits up to `T`, sends them, saves its cursor and then dies before the runtime commits will resume after `T` on restart, and those visits are lost permanently, silently, with no gap marker. Saving the cursor first loses them the same way; saving after the send still loses them if the process dies while the records are only queued.

One tokio task per provider, supervised with restart and exponential backoff. **The supervisor lives beside the `Provider` trait in `providers/mod.rs`** - it is provider lifecycle, not buffer or transport concern - and the runtime starts it and does not reimplement it. Exactly one module owns this; a second claim anywhere is a defect.

### Window provider

**Event sources.** Every one is a system API; there is no third-party dependency. All of them are registered on the event thread's run loop described in Context.

- `NSWorkspace.didActivateApplicationNotification` - the frontmost application changed. Emits `focus`.
- An `AXObserver` on the frontmost application, re-registered whenever that application changes, watching `kAXFocusedWindowChangedNotification`, `kAXTitleChangedNotification`, `kAXWindowCreatedNotification` and `kAXUIElementDestroyedNotification`. All four emit `state_change`.
- `CGDisplayRegisterReconfigurationCallback` - a display was attached, detached or rearranged. Emits `state_change`.
- `NSWorkspace.willSleepNotification` and `didWakeNotification`. Emit `sleep` and `wake`.
- Distributed notifications `com.apple.screenIsLocked` and `com.apple.screenIsUnlocked`. Emit `lock` and `unlock`.
- A heartbeat tick every `tick_interval` (default 30s) regardless of events. Emits `tick`.

`kAXTitleChangedNotification` is load-bearing and not optional. Neither application activation nor window focus fires when the title changes underneath a window that keeps focus - switching files inside an editor, a build finishing and rewriting a terminal title, or switching between two already-loaded tabs in one browser window, which also writes no history row. Without the title observer every one of those is invisible until the next tick, and anything shorter than a tick vanishes entirely, because coalescing merges consecutive samples sharing the same title.

**`sleep` must reach the server before the Mac sleeps.** The service's liveness rule treats a `sleep` marker as expected quiet and alarms without one, so a marker that sits in the buffer until wake inverts the check: the machine is reported stale all night and healthy the moment it comes back. On `willSleepNotification` the provider therefore enqueues the `sleep` record and the runtime flushes the buffer synchronously before the handler returns, with a hard 2s budget - macOS grants a short window before sleeping and blocking longer risks being killed. A flush that does not complete in the budget is abandoned; the record is durable in the buffer either way and ships on wake, which is the previous behaviour rather than a new failure.

**Sample assembly**, in this order:

1. Read the window list and display list, and compute the visible set (below).
2. Resolve the frontmost application (`NSWorkspace.frontmostApplication`) and, from the window list, its front-most on-screen window - the entry whose owner pid matches and whose front-to-back index is lowest. `display` comes from **that** entry, through `CGDisplayBounds`, with no Accessibility involved. When the frontmost application has no on-screen window, `display` is the display containing the mouse cursor, which always exists.
3. Read the focused window's `kAXTitle` and `AXDocument` via `kAXFocusedWindow`.
4. Read titles for the other visible windows, subject to the join rule below.
5. Run the extractor registered for the focused application's bundle id, if any.
6. Read idle seconds, difference the input counters against the previous tick, read microphone state.

Step 2 comes before step 3 deliberately. `display` is a required field on every `tick` and `focus`, and a null one is rejected and then deleted - so sourcing it from Accessibility would mean the entire stream is destroyed on day one, before the human has granted the permission, which is precisely the state the degraded path exists to survive.

**Visibility.** A window from `CGWindowListCopyWindowInfo` counts as visible when its layer is 0 and its rectangle intersects the rectangle of some active display by at least 20% of the window's own area. Both rectangles come from the same flipped coordinate space - window bounds from `kCGWindowBounds`, display bounds from `CGDisplayBounds` - so no conversion is involved and there is no axis to get backwards. Minimised windows never appear, because `.optionOnScreenOnly` excludes them.

The area threshold is not cosmetic. Some window managers hide a window by moving it off-screen rather than unmapping it, leaving it reported as on-screen while only a pixel or two intersects a display; a window that is 99% off-screen is also not being read by anyone. A window intersecting no display at all is not visible.

A window straddling two displays can pass the threshold on both. `display` is a single value, so the rule is: **the display with the largest intersection area wins**, and an exact tie is broken by the lower display index so the result is deterministic. Without this the straddling test the plan mandates has no expected answer and two implementations disagree.

Occlusion is **not** computed in v1: a window fully covered by another still counts as visible. The window list is front-to-back, so each entry's index is recorded as `z` in the `visible` payload and occlusion can be derived later. This is a stated limitation with a stated mitigation, not an oversight.

**Titles for non-focused visible windows.** The focused window is resolved through `kAXFocusedWindow` and is unambiguous. For the rest there is no supported way to map a `kCGWindowNumber` to an `AXUIElement`: the Accessibility window array is unordered and carries no window number, two windows of one application can have byte-identical geometry (verified live: two windows at `2544x1394 @ (-5103,1027)`), and matching by title is circular because the title is what is being fetched. So the rule is: when the owning application has **exactly one** Accessibility window, its title is unambiguous and is used, with `title_reason` null; otherwise `title` is null and `title_reason` is `"ambiguous"`. A private Accessibility symbol exists that would close this, and adopting an undocumented API is deliberately not a decision made silently here - if the null rate proves high in practice it becomes its own plan.

**Degraded capture.** When Accessibility is not granted, or the Accessibility calls fail, the daemon keeps capturing: application, bundle id, geometry, `display` and activity signals all still work - none of them touch Accessibility - and only titles, paths and extractor details go null. Such records carry `degraded: true`, which reaches the service and the API. Announcing degradation only in a local log would make a degraded day indistinguishable from a quiet one to anyone reading the timeline afterwards, and by then the day is gone.

### Extractor registry

Three mechanisms, one registry keyed by bundle id, all invoked only for the **focused** application:

1. **`AXDocument`** - a universal probe applied to every focused window regardless of application. Keep the value only when non-empty and its scheme is `file`. No per-application code.
2. **AppleScript** - `company.thebrowser.dia`, the form captured in Context. Produces `{url, tab, profile, pinned}`.
3. **The application's own CLI** - `com.umputun.agterm`, via `agtermctl tree --json`. Produces `{workspace, session, cwd, foreground}`.

**Extractors are debounced.** A burst of AX notifications - a window opening, a title settling in two steps, a display waking - would otherwise spawn an `osascript` and an `agtermctl` per notification, several subprocesses a second, and each `state_change` also breaks a run on the server. So event-driven sample assembly coalesces: an event schedules assembly 300 ms out, further events inside that window reset the timer without adding work, and one sample is emitted when it expires. The heartbeat tick is not debounced. A burst that lasts longer than the debounce still emits, so nothing is lost - only the duplicates within it are.

Every extractor is fallible and returns empty rather than failing the tick - and every extractor that spawns a subprocess carries a hard deadline (2s) after which the child is killed. "Fallible" covers an error return, not a hang: `osascript` blocking on a wedged browser would otherwise leave the provider awaiting it forever, with no error for the supervisor to act on, so ticks and events simply stop while the daemon appears healthy.

A new extractor is added only when it answers a question the window title cannot.

### Runtime

**Buffer** - SQLite at `~/Library/Application Support/nikki/buffer.db`:

```
pending(id INTEGER PRIMARY KEY, envelope TEXT NOT NULL, bytes INTEGER NOT NULL, created_at TEXT NOT NULL)
cursors(provider TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(provider, key))
dead_letter(id INTEGER PRIMARY KEY, envelope TEXT NOT NULL, bytes INTEGER NOT NULL, reason TEXT NOT NULL, at TEXT NOT NULL)
meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)     -- holds the seq counter
```

**Concurrency.** Two providers and the shipper touch this file. It is opened once with `journal_mode = WAL` and `busy_timeout = 5000`, and every writer goes through a single owner task rather than a shared connection pool - the buffer is not a hot path and serialising writes removes a class of `SQLITE_BUSY` bugs that would otherwise appear only under load. A test exercises concurrent enqueue from both providers while the shipper drains.

**Bounded, with a stated policy.** `pending` is capped at `max_rows` (default 200_000) and `max_bytes` (default 500 MB), whichever is reached first. `dead_letter` has its **own, separate** cap of 5 000 rows and 50 MB, evicting its own oldest rows when full.

The two caps are separate deliberately, and the obvious alternative is a trap: counting `dead_letter` toward the shared cap while evicting only from `pending` produces a buffer that cannot stay bounded at all. Once permanent failures fill the shared limit, every enqueue trips overflow, live records are destroyed to make room for records that will never ship, and the overflow marker enqueued to report the loss pushes the total up again - and is itself dead-lettered if the failure is the permanent kind.

On `pending` overflow, evict oldest-first **until both the row and the byte total are under their limits** - a flat "delete 10%" does not necessarily satisfy `max_bytes`, which a handful of large records can dominate on their own - then enqueue one record with `provider: "windows"`, `kind: "buffer_overflow"` and the payload pinned in the wire contract, so the loss appears in the timeline instead of being invisible. The `details` nesting is not incidental: the server routes this into `window_samples`, whose only free-form column is `details`, and that column is carried through to the coalesced timeline.

Calling a buffer "sized for outages" without a number is how a laptop away for a week fills its disk. The real record rate is roughly 2 880 window ticks a day, plus a few hundred window events, plus browser visits that only ship when something about them changed - call it 3 000 to 4 000 records a day. The default `max_rows` therefore holds around seven weeks. That figure depends entirely on re-read visits **not** being shipped unchanged; ship them all and the same cap holds a day and a half.

**Shipping** - batches of up to 500, outcome handling exactly as specified in the wire contract, exponential backoff from 1s to a 5-minute ceiling on the retry path only. Every request carries an explicit deadline: 10s to connect, 30s total. Without one a half-open connection - the normal result of a laptop changing networks - hangs the shipper indefinitely while the buffer grows behind it, and no backoff branch is ever reached because nothing has failed yet.

**Redaction** - applied to the envelope before it reaches the buffer, so every provider inherits it and nothing unredacted is ever written to disk:

```toml
[[redact]]
url_host = "*"
keep = "host"              # default: host only, path and query dropped

[[redact]]
url_host = "linear.app"
keep = "full"              # opt in per host

[[redact]]
bundle_id = "com.tinyspeck.slackmacgap"
drop = ["title"]
```

Three rules that are easy to get wrong and silent when wrong:

- **`payload.path` is not a URL rule.** It is a `file://` document path and survives whole; the URL rules apply only to `payload.url` and to a `url` inside `details`. Applying host-only redaction to `path` would reduce every editor sample to `file:///`, deleting the single most valuable field in the window stream.
- **A host-less URL** (`file://`, `chrome-extension://`, `data:`) is reduced to its scheme by the `url_host = "*"` rule rather than falling through it. Falling through would ship a full local filesystem path unredacted.
- **`drop = ["title"]` applies to `visible[]` entries too.** Those carry other applications' titles, so a rule that only covered the top-level title would drop a bundle's title when it was focused and keep it when it was merely visible.

Default posture for URLs is host-only with per-host opt-in; **the port is part of the host** and is kept (`http://localhost:3000/`), because a bare hostname loses which of several local services was being used. Titles are kept by default because they usually carry the meaning, and dropped per bundle id where they do not.

**Config** - `~/.config/nikki/config.toml`. The redaction rules above live in this same file, under `[[redact]]`; they are not a separate document.

`service_url`, `device` and `browser.profile` have **no defaults and are required**. An empty `device` is the dangerous one: it is a component of every `dedup_key` and a column the server indexes, so an empty value produces records that store but can never be attributed to a machine, and no error anywhere. All three are validated at startup - `service_url` must parse as an absolute http or https URL, `device` and `browser.profile` must be non-empty - and a failure names the field rather than the file.

Both paths the daemon owns are overridable so a test harness never touches the operator's real state: `NIKKI_CONFIG` selects the config file and `NIKKI_STATE_DIR` the directory holding `buffer.db`. Task 10's harness sets both.

```toml
service_url = "http://alpha:8080"
device = "mbp-21"
tick_interval = 30            # seconds, must be within [1, 3600]
history_poll_interval = 300   # seconds
revisit_window = 500          # visit rows re-read on each poll

[browser]
profile = "MBP_21"            # display name, not the directory name

[buffer]
max_rows = 200000
max_bytes = 536870912
```

## What Goes Where

- **Implementation Steps** cover everything achievable inside this repository, from its root as the working directory, running natively on macOS.
- **Post-Completion** covers signing, distribution, permissions and anything needing a human.

## Implementation Steps

### Task 1: Workspace, config, bundle

**Files:**
- Create: `Cargo.toml`, `src/main.rs`, `src/config.rs`
- Create: `.mise.toml`, `Info.plist.template`, `scripts/bundle.sh`, `CLAUDE.md`, `README.md`

- [x] `.mise.toml` pinning rust 1.98 with `rustfmt` and `clippy`, and tasks `build`, `test`, `lint` (`cargo clippy --all-targets -- -D warnings`), `fmt`, and `check` running fmt-check, clippy and tests in sequence
- [x] cargo project with tokio, tracing, serde, argh; crates at their latest releases, verified at add time
- [x] declare `mod config;` in `main.rs`
- [x] config from `~/.config/nikki/config.toml` with every field from Technical Details and its default; an unparseable file is a startup error naming the field
- [x] reject `tick_interval` outside `[1, 3600]` at startup with a named error - the server rejects such ticks per-record and the daemon then deletes them, so shipping them destroys the whole window stream while looking healthy
- [x] require `service_url` (absolute http/https), `device` (non-empty) and `browser.profile` (non-empty) at startup, each failure naming the field; honour `NIKKI_CONFIG` and `NIKKI_STATE_DIR` overrides
- [x] `Info.plist.template` with `LSUIElement` true, a stable `CFBundleIdentifier`, `CFBundleExecutable`, `CFBundleName`, `CFBundleVersion`, `CFBundleShortVersionString`, `CFBundlePackageType` `APPL`, `LSMinimumSystemVersion`, and **`NSAppleEventsUsageDescription`** - without that string macOS never shows the Automation prompt, the Dia extractor is denied for the life of the bundle, and the only symptom is a `-1743` in the log
- [x] `scripts/bundle.sh` assembling `nikki.app` from the built binary and the plist template; signing is out of scope here
- [x] `CLAUDE.md` recording the conventions, above all the unsafe-containment rule, the inline-test rule and the declare-every-module rule
- [x] write tests for config defaults, full parse, each error case, both ends of the `tick_interval` bound, each missing required field, and that `NIKKI_CONFIG`/`NIKKI_STATE_DIR` are honoured
- [x] run the per-task gate - must pass before Task 2

### Task 2: The macos module - all unsafe lives here

**Files:**
- Create: `src/macos/mod.rs`, `src/macos/ax.rs`, `src/macos/window_list.rs`, `src/macos/activity.rs`, `src/macos/events.rs`
- Modify: `src/main.rs`, `Cargo.toml`

- [ ] add the FFI crates to `Cargo.toml`; declare `mod macos;` in `main.rs` and every file as `mod ax; mod window_list; mod activity; mod events;` in `src/macos/mod.rs`
- [ ] `ax.rs`: application element, window list, focused window, attribute read with a type-id check before wrapping, `AXDocument`, and `AXUIElementSetMessagingTimeout` applied to every element (0.4s)
- [ ] release every Copy/Create value on every path including error returns; the attribute reader must release before returning a type mismatch
- [ ] `window_list.rs`: `CGWindowListCopyWindowInfo` and `CGGetActiveDisplayList` plus `CGDisplayBounds`, returning plain Rust structs with owner pid, owner name, window number, bounds, layer and front-to-back index - **no minimised flag**, since `.optionOnScreenOnly` already excludes minimised windows
- [ ] `activity.rs`: idle seconds truncated to an integer, key and mouse counters, microphone running state, and the display containing the mouse cursor
- [ ] `window_list.rs` also exposes `frontmost_application()` and `bundle_id_for_pid(pid) -> Option<String>` over `NSRunningApplication`, with the same release discipline as the rest of the module - `kCGWindowOwnerName` is the display name (`Zed`), not the bundle identifier (`dev.zed.Zed`), and `bundle_id` is a required field on three kinds and rides every `visible[]` entry, so without this the provider must either put `unsafe` outside this module or silently ship the display name, which the server accepts as opaque text while the bundle-id-keyed extractor registry then never matches anything
- [ ] `events.rs`: the dedicated event thread from Context - own `CFRunLoopGetCurrent()`, add every source to it in `kCFRunLoopDefaultMode`, run `CFRunLoopRun()`, convert each callback into a plain Rust event value onto an mpsc channel, and stop via `CFRunLoopStop` on shutdown
- [ ] register on that run loop: `NSWorkspace` activation, sleep and wake; distributed lock and unlock; `AXObserver` for focused-window, title, created and destroyed, re-registered when the frontmost application changes; display reconfiguration
- [ ] the module's public API exposes only safe types; no raw pointer crosses its boundary
- [ ] write tests for the pure parts: counter differencing including wraparound, event mapping, bounds arithmetic
- [ ] the `willSleep` handler sends its event with a completion handle and blocks on the flush acknowledgement for at most 2s before returning - the one exception to post-and-return, and the only thing that can delay the sleep transition
- [ ] callbacks carrying transient state capture it at callback time rather than leaving it to be resampled when the event is drained
- [ ] write a test that the event thread starts, delivers a synthesised event onto the channel, and stops cleanly on shutdown
- [ ] write a test that the `willSleep` handler does not return until the acknowledgement arrives, and does return once the 2s budget expires without one
- [ ] run the per-task gate - must pass before Task 3

### Task 3: Visibility resolution (test-first)

**Files:**
- Create: `src/window/mod.rs`, `src/window/visibility.rs`
- Modify: `src/main.rs`

- [ ] declare `mod window;` in `main.rs` and `mod visibility;` in `src/window/mod.rs`
- [ ] write the tests first from fixture rectangles: window wholly inside one display; wholly outside every display; straddling two displays with unequal overlap, asserting the larger one wins; straddling with an exact tie, asserting the lower index wins; intersecting exactly one pixel; intersecting exactly at the 20% threshold from both sides; layer non-zero; a window whose display is absent from the display list
- [ ] implement the pure resolver over window and display rectangles, returning the visible set with each window's display and front-to-back index `z`
- [ ] a window intersecting no listed display is not visible, and the case is logged rather than silently dropped
- [ ] run the per-task gate - must pass before Task 4

### Task 4: Buffer, sequence and cursors

**Files:**
- Create: `src/runtime/mod.rs`, `src/runtime/buffer.rs`, `src/runtime/dedup.rs`
- Modify: `src/main.rs`, `Cargo.toml`

*Deliberately before the providers: providers persist cursors through the runtime, so building them first would force a stub that the buffer task then rewrites, leaving the provider's only tested behaviour unproven.*

- [ ] add the SQLite crate to `Cargo.toml`; declare `mod runtime;` in `main.rs` and `mod buffer; mod dedup;` in `src/runtime/mod.rs`
- [ ] `dedup.rs`: the two key formulas from the wire contract, over unit-separator-joined fields, `sha256` truncated to 16 hex characters - it lives here, not with shipping, because the window key hashes `seq` and only this task can produce one
- [ ] SQLite buffer with the four tables from Technical Details, opened with `journal_mode = WAL` and `busy_timeout = 5000`, all writes serialised through one owner task
- [ ] `enqueue(records, cursor)` allocates the monotonic `seq` from `meta`, derives each record's `dedup_key` from it, writes the pending envelopes and advances the cursor - **all in one transaction**, returning only after commit
- [ ] `seq` survives restart and never repeats
- [ ] `pending` enforces `max_rows` and `max_bytes`, evicting oldest-first until **both** totals are under their limits, then enqueues a `buffer_overflow` record whose payload is exactly the shape pinned in the wire contract - and which is keyed through the same path as any other record, so it carries a real `seq` and `dedup_key`
- [ ] `dead_letter` has its own separate cap (5 000 rows, 50 MB) evicting its own oldest rows, so unshippable records can never consume the live buffer's budget
- [ ] `take_batch`, `delete_batch`, `dead_letter` and a synchronous `flush_now` the sleep handler awaits
- [ ] write tests for: enqueue atomicity (a cursor never advances when the envelope insert fails); seq monotonic across a simulated restart; overflow evicting until both limits are satisfied and emitting exactly one overflow record; the overflow record carrying a non-empty `dedup_key` and a non-zero `seq`; `dead_letter` filling its own cap without evicting a single `pending` row; take and delete round-trip
- [ ] write a test that two records of the same kind in the same millisecond get different keys
- [ ] write a concurrency test: both providers enqueueing while the shipper drains, with no `SQLITE_BUSY` surfacing
- [ ] run the per-task gate - must pass before Task 5

### Task 5: Shipping and redaction

**Files:**
- Create: `src/runtime/ship.rs`, `src/runtime/redact.rs`
- Modify: `src/runtime/mod.rs`, `src/runtime/buffer.rs`, `Cargo.toml`

- [ ] add the HTTP client crate to `Cargo.toml`; declare `mod ship; mod redact;` in `src/runtime/mod.rs`
- [ ] `redact.rs`: rules applied to the envelope **before it is buffered**; URL default host-only **keeping the port**, per-host opt-in, genuinely host-less URLs (`file://`, `data:`) reduced to their scheme while `chrome-extension://` goes through the ordinary host rule, `payload.path` never treated as a URL, and `drop = ["title"]` applied to `visible[]` entries as well as the top-level title
- [ ] `ship.rs`: batches of up to 500, a 10s connect and 30s total deadline on every request, and the outcome split from the wire contract - 200 with a well-formed body deletes the whole batch and logs each rejection; a 200 whose body does not parse or whose counts do not add up is treated as 5xx; 401/403/404/405 keep the batch and back off, logging at error, because those are configuration rather than bad data; other 4xx dead-letter; 413 halves down to a floor of 10 then dead-letters; 5xx and transport errors back off from 1s to 5m
- [ ] wire the pipeline as redact -> enqueue (which keys) -> ship, so redaction is on the live path and nothing unredacted is written to disk
- [ ] write tests for each redaction rule, including a `file:///` reduced to scheme, a `chrome-extension://` keeping its extension id, a `localhost:3000` keeping its port, and `payload.path` surviving whole
- [ ] write a test asserting no redacted path or query appears in the buffered envelope - constructed so it can fail, by redacting a URL whose path is a distinctive token and grepping the whole serialised envelope for that token
- [ ] write tests for every branch of the outcome split, including a 200 carrying rejections, a 200 with a malformed body, a 404 that keeps the batch, a 400 that dead-letters, and a 413 halving to the floor
- [ ] run the per-task gate - must pass before Task 6

### Task 6: Extractors

**Files:**
- Create: `src/extract/mod.rs`, `src/extract/document.rs`, `src/extract/dia.rs`, `src/extract/agterm.rs`
- Create: `fixtures/dia_active_tab.txt`, `fixtures/agterm_tree.json`
- Modify: `src/main.rs`

- [ ] declare `mod extract;` in `main.rs` and every file in `src/extract/mod.rs`
- [ ] registry keyed by bundle id, invoked only for the focused application
- [ ] `document.rs`: the universal `AXDocument` probe keeping only non-empty `file://` values
- [ ] `dia.rs`: the captured AppleScript verbatim, splitting on `0x1F` into four fields; empty result means not running; parse the trailing `(-NNNN)` from stderr and branch on `-1743` (warn once per process), `-1728`, `-1712` and unknown - all returning empty
- [ ] `agterm.rs`: resolve `agtermctl` on `PATH` then the bundle path; walk `result.tree.workspaces[].active` then `sessions[].active`; `foreground` is optional and may be `null`; store the file name of `foreground[0]`; no active workspace or session yields empty
- [ ] every subprocess carries a 2s deadline and is killed when it expires
- [ ] commit the captured Dia output and agterm JSON from Context as fixtures
- [ ] write parser tests against both fixtures, including a title containing a comma, a `null` foreground, a tree with no active session, and each AppleScript error code
- [ ] write a test asserting a hung subprocess yields empty within the deadline rather than blocking
- [ ] run the per-task gate - must pass before Task 7

### Task 7: Window provider

**Files:**
- Create: `src/providers/mod.rs`, `src/providers/windows.rs`
- Modify: `src/main.rs`

- [ ] declare `mod providers;` in `main.rs` and `mod windows;` in `src/providers/mod.rs`
- [ ] the `Provider` trait, `Emission`, `Ctx`, and the supervision harness - restart with backoff, owned here and nowhere else
- [ ] window provider driven by every event source in Technical Details plus the heartbeat tick, with each source emitting the kind stated there
- [ ] sample assembly in the specified order, with `display` sourced from the window list rather than Accessibility, and the single-Accessibility-window join rule producing `title_reason` when ambiguous
- [ ] stamp `tick_interval_sec` from config onto every `tick` payload
- [ ] `visible[]` entries carry `app`, `bundle_id`, `title`, `title_reason`, `display` and `z`, with `bundle_id` from `bundle_id_for_pid` and `app` from the owner name
- [ ] event-driven assembly is debounced at 300 ms - an event schedules assembly, further events reset the timer, one sample is emitted when it expires; the heartbeat tick is not debounced
- [ ] set `degraded: true` when Accessibility is unavailable or its calls fail, keeping everything that still works
- [ ] write tests for sample assembly from mocked sources: tick path, each event path and the kind it emits, the ambiguous-title rule, `display` resolved with Accessibility unavailable, and the degraded path
- [ ] write a test that a burst of five events inside the debounce window produces one sample and spawns one extractor invocation, not five
- [ ] run the per-task gate - must pass before Task 8

### Task 8: Browser history provider

**Files:**
- Create: `src/providers/browser_history.rs`
- Create: `fixtures/history_sample.db`
- Modify: `src/providers/mod.rs`

- [ ] declare `mod browser_history;` in `src/providers/mod.rs`
- [ ] resolve the configured display name to a directory through `Local State` on **every** poll, reading only `profile.info_cache` and tolerating unknown fields; a name absent at **startup** is a fatal error listing the names that exist, while a name that disappears **at poll time** logs a warn and skips that poll rather than killing a running daemon
- [ ] read by cloning `History` and `History-journal` with `clonefile` into a temporary directory, opening the copy read-only, running `PRAGMA quick_check`, and deleting the copy after the read; anything other than `ok` discards the copy and skips the poll without advancing the cursor
- [ ] the captured query from Context, paged by `visits.id`, 5000 rows at a time, reading `id > cursor - revisit_window` so a filled-in `visit_duration` is picked up
- [ ] **emit a re-read row only when its `title`, `transition` or `visit_duration` differs from the last shipped values for that `visits.id`**; persist those values per profile beside the cursor, pruned to the same window
- [ ] convert `visit_time` with `visit_time / 1000000 - 11644473600` and `visit_duration` with `visit_duration / 1000`; store `transition` raw
- [ ] first run starts from `max(id) - revisit_window` rather than backfilling the whole history
- [ ] `max(id) < cursor` means the database was replaced: log at warn, increment the per-profile `generation`, reset the cursor and clear the last-shipped map - the generation is part of the dedup key, so without incrementing it the reused ids would collide with stored visits and the server would silently merge two unrelated ones
- [ ] cursor and generation per profile, emitted together with the records they cover
- [ ] poll on `history_poll_interval` (default 5m)
- [ ] commit a small anonymised history database and a `Local State` fixture
- [ ] write tests for both conversions against the captured live values (`22874541` microseconds becomes `22874` ms), paging across the limit, cursor advance, first-run cursor selection, and `Local State` parsing including an entry with a missing `name` and an unknown top-level field
- [ ] write a test that an unchanged re-read row is **not** emitted while one whose duration changed **is**
- [ ] write a test that a reset increments the generation and that keys before and after the reset differ for the same `visits.id`
- [ ] write a test that a `file:///` row ships with `url` reduced to `file:///` and is not dropped
- [ ] write a clone test that actually exercises the lock: hold an **exclusive** lock on the source (`BEGIN EXCLUSIVE`), assert a direct read fails, then assert the clone path succeeds - a test using an ordinary reader proves nothing, since SQLite readers do not block readers
- [ ] write a test that a copy failing `quick_check` is discarded and the cursor does not move
- [ ] run the per-task gate - must pass before Task 9

### Task 9: Wire it together

**Files:**
- Modify: `src/main.rs`, `src/runtime/mod.rs`

- [ ] main starts the event thread and the runtime, registers both providers, and runs until SIGTERM
- [ ] graceful shutdown: stop providers, `CFRunLoopStop` the event thread and join it, flush one final batch, close the buffer
- [ ] structured logging with a one-line startup summary naming the device, service URL, granted permissions, resolved browser profile and enabled providers
- [ ] a provider that panics is caught, logged and restarted without taking down the process
- [ ] write a test asserting a panicking provider restarts while the other keeps emitting
- [ ] run the per-task gate - must pass before Task 10

### Task 10: Verify acceptance criteria

*This task adds **test-support** Rust rather than production code, so it carries no unit-test checkbox of its own; its gate is the harness below passing end to end. It is still a Rust task and still runs the per-task gate.*

**Files:**
- Create: `scripts/acceptance.sh`, `tests/stub_server.rs`
- Modify: `src/macos/events.rs`

- [ ] `tests/stub_server.rs` is a test-only HTTP server recording requests and returning a scripted sequence of responses; `scripts/acceptance.sh` runs the daemon against it and asserts the checks below
- [ ] the harness sets `NIKKI_CONFIG` and `NIKKI_STATE_DIR` into a temporary directory, so it can never read or write the operator's real config or buffer
- [ ] `events.rs` honours `NIKKI_TEST_EVENTS=<path>`: when set, the event thread reads newline-delimited event values from that file instead of registering OS sources. Without this seam the entire event half of the daemon - every `state_change`, `lock`, `sleep` and `wake` - can only be exercised by a human doing things on a Mac, so an unattended run has no way to assert the kinds the server was just changed to accept
- [ ] run the per-task gate over the whole crate
- [ ] `scripts/bundle.sh` produces `nikki.app`, and its `Info.plist` contains `LSUIElement` true and a non-empty `NSAppleEventsUsageDescription`
- [ ] envelopes recorded by the stub match the wire contract exactly, including `tick_interval_sec` on every tick
- [ ] with a synthesised title-change event injected through `NIKKI_TEST_EVENTS`, the stub records a `state_change` with `app`, `bundle_id` and `display` present
- [ ] `seq` is monotonic across a daemon restart
- [ ] `dedup_key` contains no path or query from a redacted URL
- [ ] with the stub returning 500, records accumulate in the buffer and drain once it returns 200
- [ ] with the stub returning 200 carrying a rejection for one record, the whole batch is deleted and the rejection logged
- [ ] with the stub returning 200 and a malformed body, the batch is kept and retried
- [ ] with the stub returning 400, the batch lands in `dead_letter` and shipping continues
- [ ] with the stub returning 404, the batch is **kept** and retried rather than dead-lettered
- [ ] filling the buffer past `max_rows` evicts oldest-first until both limits are satisfied and enqueues exactly one `buffer_overflow` record, itself carrying a real `seq` and `dedup_key`
- [ ] running with Accessibility unavailable still ships samples with `degraded: true`, a non-null `display` and null titles

### Task 11: Documentation

*No test checkbox.*

**Files:**
- Modify: `README.md`, `CLAUDE.md`

- [ ] README: the permissions and why each is needed, the config file with every field, the provider model and how to add one, and the wire contract
- [ ] CLAUDE.md: the unsafe-containment rule, the inline-test rule, the declare-every-module rule, the per-task gate
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

*No checkboxes - these need a human or an external system.*

**Permissions on each Mac:**
- Grant Accessibility to `nikki.app` and approve the one-time Automation prompt for Dia.
- Confirm no Screen Recording or Microphone prompt ever appears and no menu-bar indicator lights.

**Distribution** (handled separately, deliberately outside this plan):
- signing identity, notarisation, the release workflow and tap publication
- the LaunchAgent that starts the bundle at login
- installing on the second MacBook once the first has run for a week, with its own `device` and `browser.profile` in config

**Verification against a real day** (needs a human working one):
- Capture a full day, then spot-check the timeline against memory: a known editing session shows the right repository path, a known browsing session shows the right host and profile, a known terminal session shows the right workspace and cwd, and lunch appears as idle rather than as work.
- Confirm a file switch inside the editor appears as a `state_change`, and that a page held open across a poll boundary eventually reports a non-zero `duration_ms`. Whether that `state_change` also *shortens* the preceding run depends on the service closing runs at the event timestamp, which is tracked separately.

**Deferred providers** (the architecture admits them; each is roughly one file):
- shell history from the local atuin database, which already records every command with its working directory, duration and exit code - the richest single addition
- git commits across the projects directory
- agent session transcripts

**Known limitations to revisit:**
- Occlusion is not computed, so a fully covered window still counts as visible; `z` is recorded so it can be derived later.
- A non-focused visible window belonging to a multi-window application has no title, flagged as `title_reason: "ambiguous"`.
- The browser tab is captured only when the browser is the focused application, and only one profile's history is read.
- A rarely-used browser profile still leaves tab entries in window samples, by explicit decision.
- `mic_active` means the input device is running, not that anyone is listening.
- Windows applications running under a virtualiser in coherence mode appear as ordinary host windows with their own titles.
- No third-party window manager is used, so there is no tag or workspace grouping in the window layer; adding one is its own plan.
