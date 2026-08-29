# nikki

A macOS daemon that records what was on screen and what was being done, and ships it to the nikki
service. One signed binary per Mac, installed from a Homebrew tap and run by launchd.

Toggl's Activity view reports only which application was frontmost and never what was being done in
it. This daemon captures the missing layer: window titles, the open document path, the browser tab
and profile, the terminal workspace and working directory, plus real activity signals - input
volume, idle, lock, sleep and microphone.

The daemon never interprets. It captures faithfully and ships; a summary of the day is produced
elsewhere, from the service API.

## Install

```
brew install pkarpovich/apps/nikki
```

Write `~/.config/nikki/config.toml` **before** starting the service - `service_url` and `device` are
required and the daemon exits without them, which under Homebrew's `keep_alive` is a restart loop.
Then `nikki --check-config`, `brew services start nikki`, and grant Accessibility when macOS asks.

Releases are cut by tagging; see `docs/releasing.md`.

## Build

```
mise run build            # debug binary
mise run check            # fmt check, clippy with warnings denied, tests
./scripts/build-signed.sh <team-id>   # signed release binary, same identity CI ships
./scripts/acceptance.sh   # daemon against the stub server in tests/stub_server.rs
```

The crate links Apple frameworks and builds only on macOS. Cross-compiling needs the Apple SDK.

`build.rs` renders `Info.plist.template` with the crate version and embeds it into the binary's
`__TEXT,__info_plist` section. That section is not decoration: it is where macOS reads
`NSAppleEventsUsageDescription` from for a program that is not an app bundle, and the release
workflow refuses to ship a binary without it.

## Permissions

| Permission | What it buys | Indicator / prompts |
|---|---|---|
| Accessibility | which application is focused, window titles, the focused window, `AXDocument` paths, activation and title-change events | none |
| Automation -> Dia | the active tab's URL, title and profile | one-time prompt on first use |
| (none needed) | window list, geometry, z-order, displays, idle seconds, input counters, lock, sleep, microphone state, the process table | none |

Both grants are keyed by the code signature - the Developer ID team plus the `dev.pkarpovich.nikki`
identifier that `codesign --identifier` pins - so they survive every upgrade that keeps signing with
the same pair. Changing the identifier loses both grants silently: capture keeps running, titles go
null and no tab is ever read again.

**The process table needs no permission, and only agterm's own variables are consulted.**
`KERN_PROCARGS2` returns the argv and the environment of a process owned by the same user with no
prompt and no entitlement, which is how the agterm extractor knows what is running in the pane on
screen. The buffer it returns carries the whole environment and is parsed whole, but only
`AGTERM_ENABLED`, `AGTERM_SESSION_ID`, `AGTERM_PANE` and `AGTERM_PANE_ID` are ever looked at: no
environment variable is recorded, logged or shipped, and an argv reaches a record only for the pane
the tree calls active and visible. The call fails for setuid and hardened binaries (`sudo`, `top`), which
is a normal outcome and never logged per tick.

Run the daemon under launchd (`brew services start nikki`), not from a terminal. macOS attributes a
TCC grant to the *responsible* process, and a binary launched from an already-trusted terminal
inherits that terminal's trust instead of asking for its own - so the grant lands on the terminal,
and the daemon loses it the moment launchd starts it for real.

**Restart the service after granting Accessibility.** No prompt is raised - the daemon asks
`AXIsProcessTrusted` without the prompting option, because a background agent that pops a dialog on
every start is worse than one that logs what it is missing - so the checkbox is ticked by hand, and
macOS caches the denial for the life of the process. Until `brew services restart nikki` the daemon
keeps running as if nothing was granted, and the only place that says so is the `accessibility=false`
field on the startup line.

**Screen Recording is deliberately never requested.** Holding it triggers a macOS re-consent dialog
roughly monthly which cannot be disabled, in exchange for `kCGWindowName` - a field this design gets
from Accessibility instead. `kCGWindowName` is therefore never read.

**The microphone is never opened.** `kAudioDevicePropertyDeviceIsRunningSomewhere` answers whether
the default input device is running without opening it, so no Microphone permission is requested and
no orange indicator lights. It means "the device is running", not "someone is listening" - some
applications hold it open idle - so `mic_active` is a hint and nothing more.

**Automation is what the Dia extractor needs.** The embedded `Info.plist` must carry a non-empty
`NSAppleEventsUsageDescription`; without that string macOS never shows the prompt, the extractor is
denied for the life of the binary, and the only symptom is a `-1743` in the log. A declined prompt is
logged once at warn level, because no tab will ever be captured until a human fixes it in System
Settings.

### Degraded capture

When Accessibility is not granted, or its calls fail, capture continues: application, bundle id,
geometry, `display` and every activity signal still work. Geometry and the activity signals never
touch Accessibility at all, and the application is still named because the focus read falls through
to the filtered window list, which needs no permission - what is lost is focus *events*, since no
observer can attach, so a switch is noticed by the next tick rather than at the moment it happened.
The window title and the `AXDocument` path go null, `visible[]` entries lose their titles, and the
record carries `degraded: true`, which reaches the service and the API - a degraded day must not be
indistinguishable from a quiet one to whoever reads the timeline later. Extractor `details` survive,
because neither extractor touches Accessibility: Dia needs Automation, and agterm needs nothing at
all - it reads `agtermctl` and the process table.

## Configuration

`~/.config/nikki/config.toml`, or the path in `NIKKI_CONFIG`. `NIKKI_STATE_DIR` overrides the
directory holding `buffer.db` and the per-poll `history-snapshot/` clone (default
`~/Library/Application Support/nikki`). Both overrides exist so a test harness never touches the
operator's real state. `HOME` is still required with both of them set: the browser profile is
resolved under `$HOME/Library/Application Support/Dia/User Data`, which no variable overrides, and an
unset `HOME` is a startup failure.

`nikki --check-config` loads and validates the configuration, logs the summary and exits.

```toml
service_url = "http://alpha:8080"   # required, absolute http or https with a host
device = "mbp-21"                   # required, non-empty
tick_interval = 30                  # seconds, must be within [1, 3600]
history_poll_interval = 300         # seconds between browser history polls
revisit_window = 500                # visit rows re-read on each poll

[browser]
profile = "MBP_21"                  # required, the display name, not the directory name

[buffer]
max_rows = 200000
max_bytes = 536870912

[[redact]]
url_host = "*"
keep = "host"                       # default: host only, path and query dropped

[[redact]]
url_host = "linear.app"
keep = "full"                       # opt in per host

[[redact]]
bundle_id = "com.tinyspeck.slackmacgap"
drop = ["title"]
```

| Field | Default | Notes |
|---|---|---|
| `service_url` | none, required | must parse as an absolute http or https URL with a non-empty host, and carry no query or fragment, because `/api/v1/records` is appended to it, nor a username or password, because the whole URL is logged at startup |
| `device` | none, required | a component of every `dedup_key` and an indexed server column, so an empty one produces records that store but can never be attributed to a machine |
| `tick_interval` | 30 | validated at startup against the same `[1, 3600]` bound the server enforces per record |
| `history_poll_interval` | 300 | at least 1 second: a poll timer of zero panics the browser history provider on every supervised restart |
| `revisit_window` | 500 | rows re-read below the cursor so a filled-in `visit_duration` is picked up |
| `browser.profile` | none, required | resolved to a directory through `Local State` on every poll; a name absent at startup is fatal and lists the names that do exist. The browser itself is not configurable - the read is always Dia's user data |
| `buffer.max_rows` | 200000 | roughly seven weeks at the real record rate; at least 1, because a cap of zero evicts every record as it arrives |
| `buffer.max_bytes` | 536870912 | 500 MB; must exceed the 512 bytes held back for the overflow record, for the same reason |
| `[[redact]]` | one `url_host = "*"`, `keep = "host"` rule | replaced wholesale when the key is present |

An unknown key is a startup error rather than silence, and every validation failure names the field
rather than the file.

`tick_interval` is validated at startup because the server bounds it per record: a tick whose
interval is absent, `<= 0` or `> 3600` is rejected, the response is still `200`, and the daemon's own
rule then deletes it - so a misconfigured interval would destroy the whole window stream while
`lock`, `wake` and browser visits kept succeeding, leaving the device reading `ok` on a timeline
empty of work.

### Redaction

Rules are applied to the envelope **before it is buffered**, so every provider inherits them and
nothing unredacted is ever written to disk. Three rules that are silent when wrong:

- **`payload.path` is not a URL.** It is a `file://` document path and survives whole. The URL rules
  apply only to `payload.url` and to a `url` inside `details`; applying host-only redaction to `path`
  would reduce every editor sample to `file:///`.
- **A host-less URL** (`file://`, `data:`) is reduced to its scheme by the `url_host = "*"` rule
  rather than falling through it. `chrome-extension://<id>/page.html` has an authority, so it is not
  host-less and goes through the ordinary host rule, shipping as `chrome-extension://<id>/`.
- **`drop = ["title"]` applies to `visible[]` entries too**, not only the top-level title, and to the
  browser tab title in `details.tab`, which for a browser window is the same string the rule just
  nulled. It applies to `details.command` for the same reason: a terminal's command line is what its
  title summarises, it is the field most likely to carry a credential passed as an argument, and a
  rule that nulled the title while shipping the whole argv would be an escape hatch that does not
  close.
- **Host-only is the floor, not a rule that can be deleted.** Replacing the `[[redact]]` list replaces
  the rules, not the default posture: a host with no matching rule is still reduced to its host.
  Shipping whole URLs takes an explicit `url_host = "*"`, `keep = "full"`.
- **A value that does not parse as a URL ships as an empty string** rather than passing through
  unredacted.

A host-only value ships as a scheme-bearing URL with an empty path (`https://host/`), never as a bare
hostname, and **the port is part of the host** (`http://localhost:3000/`). No URL, path or title ever
enters a `dedup_key`, so redaction cannot be defeated through it.

## The provider model

A **provider** captures one kind of thing and emits records. The runtime owns everything else: the
durable buffer, the monotonic `seq`, redaction, keying, batching, retry and cursor advancement.

```rust
pub trait Provider {
    fn name(&self) -> &'static str;
    fn run(
        &mut self,
        ctx: Ctx,
        out: mpsc::Sender<Emission>,
    ) -> impl Future<Output = Result<(), ProviderError>> + Send;
}

pub struct Emission {
    pub records: Vec<RecordDraft>,
    pub cursor: Option<Cursor>,
    pub committed: Option<oneshot::Sender<()>>,
}
```

**Records and the cursor they advance to travel together, in one message.** The runtime writes the
envelopes and the cursor in a single SQLite transaction before acknowledging. With two independent
operations, a provider that reads up to `T`, sends, saves its cursor and then dies before the runtime
commits resumes after `T` and loses those records permanently, silently, with no gap marker; saving
the cursor first loses them the same way.

`Emission::awaiting_commit` returns a receipt the caller can block on, sent only once the transaction
committed and dropped when it failed. This is how the sleep marker is made durable before macOS
suspends the machine, and how the browser provider advances its in-memory cursor: visits that never
reached the buffer leave the cursor where it was and are read again on the next poll.

Each provider runs as one tokio task under `providers::supervise`, which restarts it with
exponential backoff from 1s to a 5-minute ceiling. A panic is caught and restarted like any other
failure; **a failing provider restarts alone and never takes the process down.** The supervisor lives
in `src/providers/mod.rs` beside the trait, because it is provider lifecycle rather than a buffer or
transport concern. Exactly one module owns it.

### The two providers

**`windows`** (`src/providers/windows.rs`) is event-driven with a heartbeat. Its sources are one
`AXObserver` per application that owns a window, watching activation, deactivation, focused-window,
title, created and destroyed, and re-scanned on every tick; `NSWorkspace` activation, sleep and wake;
distributed lock and unlock; display reconfiguration; and a tick every `tick_interval`.

Sample assembly runs in a fixed order: window list and displays first, then the frontmost
application's front-most on-screen window - which is where `display` comes from, **not** Accessibility,
because `display` is required on every `tick` and `focus` and a null one is rejected and then
deleted, which would destroy the stream on day one before the human has granted the permission. Then
the focused window's title and `AXDocument`, then titles for other visible windows, then the
extractor for the focused bundle, then idle, input counters and microphone state.

Event-driven assembly is **debounced at 300 ms, capped at 1 s** - an event schedules assembly,
further events reset the timer, one sample is emitted when it expires. The cap is what makes a burst
longer than the debounce still emit: an application that rewrites its title faster than every 300 ms
would otherwise push the deadline out forever and the sample would never be assembled, so the reset
never moves the deadline past 1 s from the first event of the burst. The heartbeat tick is not
debounced. Without this a
burst of AX notifications would spawn an `osascript` and an `agtermctl` per notification, and each
`state_change` also breaks a coalesced run on the server.

**`browser_history`** (`src/providers/browser_history.rs`) polls on `history_poll_interval`. The
browser holds the active profile's `History` locked against any external reader, so the file is
cloned with `clonefile` along with its `History-journal` sidecar, the copy is opened read-only, and
`PRAGMA quick_check` must return `ok` before anything is read - a torn snapshot frequently opens
cleanly and reads as good data, which would ship wrong rows and advance the cursor past the right
ones. The copy is deleted after the read, and also when a poll is abandoned part-way - the clone is
an unredacted copy of the whole history, so it is held under a `0700` directory and removed by the
same guard whether the poll finishes, fails or is cancelled at shutdown. A clone is only ever live
inside a poll, so one found at startup belongs to a run that was killed outright and is swept before
anything else happens - before the config file is even read, so a config that has become unusable
cannot leave the clone on disk across restarts. `--check-config` is the one invocation that does not
sweep, because a validation run must not delete the clone a running daemon is reading.

Each poll reads `id > cursor - revisit_window` rather than `id > cursor`, because Chromium fills
`visit_duration` in by updating the row it wrote when the visit began. A re-read row is emitted
**only when its `title`, `transition` or `visit_duration` changed** since it was last shipped; that is
the difference between roughly 3 000 records a day and 144 000.

### The focus source

**Accessibility answers who is focused**, through `AXFocusedApplication` on the system-wide element,
under the same messaging timeout every other Accessibility call in the crate carries - one measured
call took 707 ms and a tick must never block on it. Every error, including a null result, is silence
rather than a failure: nine samples in 2 123 returned nothing, so silence is a normal outcome and
falls through to the window list, never to an error and never to a fabricated answer.

**The fallback is the window list filtered to owners that have a bundle id.** Paint order alone puts
whatever is drawn on top in front, and an overlay that cannot hold focus wins: `borders`, a utility
that draws a frame around the active window, was recorded as the focused application for 32 minutes on
28 August. It is not a registered application and ships `bundle_id: ""`, which is what the filter
tests. An overlay is defined by what it is, not by its name - a notch bar, an HUD and a screenshot
tool belong to the same class, and a list of process names to ignore would have to grow forever, while
"no bundle id" is checkable against the records already stored.

**`NSWorkspace.frontmostApplication` was removed rather than fixed.** It was the last fallback in the
chain and had been frozen since it was written, answering with the same application forever - so a
wrong answer was indistinguishable from a right one and outlived every real switch. A second silent
source behind a silent one only makes the silence harder to see, so the chain now ends at the filtered
window list, which is either empty or right. Do not restore it.

**The system-wide read needs a warm connection to the application it names.** In a process that has
never messaged the focused application, `AXFocusedApplication` returns `kAXErrorCannotComplete`
immediately and keeps returning it - measured over six attempts at 0.4 s, 2 s and 5 s messaging
timeouts, all failed. One `AXUIElementCreateApplication` read against that pid makes every later
system-wide read succeed, and the daemon warms it by accident: each tick already builds an application
element for the front pid to read its title. The practical cost is that the first sample after a
switch to an application never messaged before falls through to the window list, which names the same
application anyway.

### Adding a provider

1. Write `src/providers/<name>.rs` and declare it in `src/providers/mod.rs`.
2. Implement `Provider`. Return `Err(ProviderError)` rather than panicking, and never block the
   runtime - the supervisor can only act on a returned error, not on a hang.
3. Emit `Emission::new(records)`, or `Emission::awaiting_commit(records, cursor)` when the provider
   has durable progress to record - it hands back a receipt that resolves once the records and the
   cursor are committed together, so a failed commit leaves the cursor where it was. Never persist a
   cursor yourself.
4. Add the `(provider, kind)` pair and its payload contract to the wire contract below **and to the
   service repository in the same pass**. The server validates per pair and *deletes* a record whose
   pair it does not know, so an unannounced kind is destroyed permanently the first time it is sent.
5. Register it in `main.rs` under `supervise` and add its name to `PROVIDERS`.
6. Write inline tests in the same file.

Deferred providers the architecture already admits, each roughly one file: shell history from the
local atuin database, git commits across the projects directory, agent session transcripts.

### Extractors

An extractor enriches the focused application's sample with `details`. The registry is keyed by
bundle id in `src/extract/mod.rs` and is invoked **only for the focused application**.

- `document.rs` - the universal `AXDocument` probe, applied to every focused window, keeping the
  value only when it is non-empty and its scheme is `file`.
- `dia.rs` - AppleScript against `company.thebrowser.dia`, producing `{url, tab, profile, pinned}`.
- `agterm.rs` - `agtermctl tree --json` against `com.umputun.agterm`, joined against the process
  table, producing `{workspace, session, surface, command, cwd}` plus a conditional `foreground`.
  The tree names the active workspace and session; the surface the session calls active and visible
  names the pane that is on screen; and the process carrying that session's `AGTERM_SESSION_ID` and
  a matching `AGTERM_PANE`, holding its tty's foreground process group, names what runs in it.

Every extractor is fallible and returns empty rather than failing the tick, and every extractor that
spawns a subprocess carries a hard 2s deadline after which the child is killed. "Fallible" covers a
hang, not only an error return: `osascript` blocking on a wedged browser would otherwise leave the
provider awaiting it forever, with no error for the supervisor to act on, so ticks would simply stop
while the daemon appeared healthy.

Add an extractor only when it answers a question the window title cannot.

## The wire contract

The boundary between this repository and the service. **The service repository carries the identical
section, and any change here must be mirrored there in the same pass.**

**Request** - `POST {service_url}/api/v1/records`, `Content-Type: application/json`:

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
        "screen_locked": false,
        "display_asleep": false,
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

`url` is in its post-redaction form, which is what the wire actually carries. `profile` carries the
display name (`MBP_21`), never the directory name.

`display` is the **zero-based index into `CGGetActiveDisplayList`**, not a `CGDirectDisplayID`. The
trade-off is accepted knowingly: an index shifts when a monitor is attached or detached, so a value
recorded before a reconfiguration stops meaning the same physical screen afterwards and the server's
run merge key splits a run across the change. The server never branches on the value, it only
compares it for equality.

### Every `(provider, kind)` pair

The server validates per pair, not per provider, and **a rejected record is deleted rather than
retried** - so a kind missing from this table is destroyed permanently the first time it is sent.

| provider | kind | required payload fields | optional |
|---|---|---|---|
| `windows` | `tick` | `app`, `bundle_id`, `display`, `tick_interval_sec`, `idle_sec`, `keys_delta`, `mouse_delta`, `mic_active`, `visible` | `title`, `path`, `details`, `screen_locked`, `display_asleep` |
| `windows` | `focus` | `app`, `bundle_id`, `display` | `title`, `path`, `details`, `visible` |
| `windows` | `state_change` | `app`, `bundle_id`, `display` | `title`, `path`, `details`, `visible` |
| `windows` | `lock`, `unlock`, `sleep`, `wake` | none | none |
| `windows` | `buffer_overflow` | `details` carrying `dropped`, `dropped_from`, `dropped_to` | none |
| `browser_history` | `visit` | `url`, `profile`, `visit_id` | `title`, `transition`, `duration_ms` |

Unknown payload fields are never rejected - the server preserves them in its `raw` column.

`focus` and `state_change` always carry `visible`; the table marks it optional because the server does
not require it, and the bodies below omit it only for brevity.

`tick_interval_sec` rides on **every** tick rather than being assumed as a server-side global, which
is what keeps a second Mac on a 60-second interval from halving that day's reported totals and keeps
historical rows meaning what they meant when recorded.

`focus` reports that the focused application changed, and carries the same body a `state_change` does
- the application, its bundle id, the display, the title, the document path, the extractor `details`
and `visible`. The kind has been in this table since day one and was never emitted until Accessibility
became the focus source; it now comes from an `AXApplicationActivated` notification, so its `ts` is
the moment of the switch, captured when the first event of the burst arrives, rather than the tick
that noticed it up to `tick_interval` later. A switch arrives as a deactivate/activate pair roughly
20 ms apart, and the 300 ms debounce is what makes that one record instead of two.

`state_change` reports that something about the screen changed without focus moving: a title
rewrite, a window created or destroyed, a display reconfiguration. The trigger need not belong to the
focused application - one observer is attached per application owning a window, so a background window
rewriting its title also emits one, and the body still describes whatever is focused when the debounce
expires, with the background title carried in `visible`. None of those fire an application
activation or a focus change, and a title rewrite is the only signal for switching files inside an
editor, a build finishing and rewriting a terminal title, or switching between two already-loaded
tabs in one browser window - which writes no history row either. The server breaks a coalesced run on
it and gives it no duration; the tick that follows opens the run where the duration lives.

`screen_locked` and `display_asleep` ride on `tick` alone and are what tells "sitting here reading"
apart from "walked away". `screen_locked` is the login session's own lock state, read from
`CGSessionCopyCurrentDictionary()`; the key it reports is present only while the session is locked, so
an absent key, a value that is not a boolean and a missing dictionary all read as unlocked, because a
wrong `true` would mark a working hour as absence. `display_asleep` is the physical panel, read per
display from `CGDisplayIsAsleep`, and is true only when there is at least one connected display and
**every** one of them is asleep - an external monitor sleeping beside a lit laptop panel is not a dark
screen. The displays are enumerated with `CGGetOnlineDisplayList` rather than the active list the
window side uses, because a display is active only while it is *awake* and drawable: enumerating the
active list would drop the very panel whose sleep is being reported, and the field would never read
true. The two are independent: a machine locks with its panel still lit, sleeps its panel on idle
without ever locking, and reports the pair in whichever combination the machine is actually in. Both
are marked optional so the addition stays additive in both directions - a new daemon reporting to an
old server and an old daemon reporting to a new one both keep working - but this daemon emits them on
every tick.

They supersede the `lock` and `unlock` record kinds for the purpose of marking an unattended span.
Those kinds remain in the table above and keep their meaning - the event thread still registers the
`com.apple.screenIsLocked` and `screenIsUnlocked` observers, and `sleep` in particular is still what
the service's liveness rule reads - but none of them arrived during the night that motivated this
change, and an edge-triggered record cannot describe a machine that was already locked when the
daemon started. The polled fields are level rather than edge triggered, so every tick answers for
itself regardless of which notifications were delivered. The sibling change in turtle-hub carries this
identical section.

**`app` is what was in front on screen, not what held the keyboard focus.** The two agree whenever
somebody is there, and a lock is where they part: Accessibility answers `loginwindow` for the whole
locked span, which is a fact `screen_locked` already states, and reporting it would blank out the
application, title, working directory and foreground command the machine kept running the entire time
- the only evidence there is that work continued while nobody was there. So an Accessibility answer of
`com.apple.loginwindow` is treated as no answer and falls through to the filtered window list, which
keeps naming the last real window until the session is unlocked. Nothing else in the record changes
because the session locked; `screen_locked` remains the sole marker of the lock.

Bodies for the kinds not shown above:

```json
{"provider":"windows","device":"mbp-21","ts":"2026-08-25T12:04:11.002Z","seq":41190,
 "kind":"lock","dedup_key":"31aa90b2c7e05f18","degraded":false,"payload":{}}

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
```

### `details` on a window record

A free-form object whose keys depend on the focused application's extractor. The server stores it as
opaque JSON and validates nothing inside it, so a new key needs no service change - but the shapes
this daemon actually sends are documented here, and any change to them is mirrored into the service
repository in the same pass.

`company.thebrowser.dia` carries `url`, `tab`, `profile` and `pinned`. `url` is post-redaction.

`com.umputun.agterm` carries `workspace`, `session`, `surface`, `command`, `cwd` and, conditionally,
`foreground`:

```json
{"workspace":"nikki","session":"nikki daemon","surface":"scratch",
 "command":"rx docs/plans/2026-08-27-agterm-panes.md","cwd":"/Users/pavel.karpovich/Projects/nikki/docs"}
```

- `session` is the session name with its animated status glyph stripped, so an auto-named session is
  one identity over its lifetime rather than a new one per spinner frame.
- `surface` is the kind of the pane on screen - `left`, `right` or `scratch`, or one of `overlay`,
  `overlay-left` and `overlay-right` while an overlay covers the session, or `quick` while the
  window's quick terminal is up, or `dashboard` while the window shows the view-only grid of several
  sessions' panes. A session's `surfaces[]` flags describe the session's own split and scratch state
  and say nothing about zoom, the quick terminal or the dashboard, so the tree's top-level
  `quickVisible`, `dashboardMembers` and `zoomedSurface` are read first and outrank them: a zoomed
  pane is what fills the window even when the session calls a different one active, and an open
  dashboard fills it with panes from several sessions at once. `surface` is absent when the tree
  reports no surface that is both active and visible - including an older agterm that omits
  `surfaces` entirely, and including a zoom that names a surface of some other session, where the
  pane on screen is not the active session's to name.
- `command` is the whole argv of the on-screen pane's foreground process, joined by spaces, **capped
  at 512 characters** with a trailing `…` when cut. The arguments are the content and are not
  trimmed away: `rx <plan file>` says what was run, `rx` says nothing. The cap exists only so a
  pathological command line cannot dominate a record. `command` is absent when no process claims the
  active surface, rather than falling back to a guess - including when the pane runs a setuid or
  hardened binary whose arguments the kernel declines to describe, and including every overlay
  surface, the quick terminal and the dashboard: the pane role agterm stamps into a process is only
  `left`, `right` or `scratch`, so no process claims an `overlay*`, `quick` or `dashboard` surface
  and the record names what is on screen without saying what runs in it. `command` is also absent
  when more than one process claims the surface, which is what a nested multiplexer produces - a
  tmux server captures the pane's `AGTERM_*` and every job in every one of its windows leads its own
  pty's foreground group, so each of them claims the pane. Naming one of them would name a job the
  user cannot see.
- `cwd` is that same process's own working directory. It falls back to the session-level `cwd` from
  the tree **only while `surface` is `left`**: that field describes the left pane alone, so lending
  it to a visible scratch pane would name a directory nobody is looking at. With any other surface on
  screen and no directory from the process, `cwd` is absent.
- `foreground` is the file name of the session's foreground program **and is emitted only when
  `surface` is `left`**. It is a session-level field from the tree describing the left pane alone, so
  reporting it while scratch is on screen names a program nobody is looking at - which is exactly the
  defect this shape fixes.

### Identity

`ts` is RFC 3339 with millisecond precision in UTC, and is when the event happened.

`seq` is a per-device monotonic counter persisted in the buffer database, so it survives restart and
never restarts at zero. A counter that reset would collide with keys already stored earlier the same
day, and the collisions would be reported as duplicates - which looks like success.

`dedup_key` is `sha256` truncated to 16 hex characters over unit-separator-joined fields:

```
windows:  device \x1F "windows" \x1F kind \x1F ts_millis \x1F seq
browser:  device \x1F "browser_history" \x1F profile \x1F generation \x1F visit_id
```

The browser key deliberately excludes `seq`, so a revision of the same visit carries the same key and
the server recognises it as a correction. It includes `generation` - a per-profile counter
incremented whenever `max(id) < cursor` reveals the history database was replaced - because the new
database's ids restart from 1 and would otherwise collide with visits already stored under the old
one, and the server would silently merge two unrelated visits under the older one's identity.

**The key is derived inside the enqueue transaction, after `seq` is allocated**, because the window
key hashes `seq`. Redaction runs first and separately, since it must happen before anything is
written to disk.

### Response, and how the daemon branches on it

Always `200` when the request parsed:

```json
{"accepted": 1, "duplicates": 0, "rejected": [{"index": 1, "reason": "unknown provider \"shell\""}]}
```

| Outcome | Action |
|---|---|
| `200`, well-formed body | delete every record of the batch **including those in `rejected`**, and log each rejection at warn. A rejected record will never be accepted by retrying, so keeping it would block the queue forever. |
| `200`, body that does not parse or whose counts do not add up | treat as `5xx`: keep and retry. Deleting on the strength of a status line alone would discard records to any proxy that answers 200 with something else. |
| `401`, `403`, `404`, `405`, `407` | **keep** the batch, back off, log at error every time. These are configuration - a wrong `service_url`, a proxy, a service not deployed yet - and dead-lettering here would feed the entire capture into a bin one batch at a time while the daemon looked healthy. |
| `408`, `425`, `429` | **keep** the batch and back off. These ask for the same bytes again later - a throttling gateway in front of the service is not a malformed batch, and dead-lettering one would destroy the capture exactly while the service was busiest. |
| other `4xx` (400, 409, 422) | move to `dead_letter`, log at error, continue with the next batch. |
| `413` | halve the batch size and retry, down to a floor of 10 records; below that, dead-letter. |
| `5xx`, timeout, connection failure | keep and back off. The only path that retries the same bytes. |

Batches are up to 500 records. Every request carries a 10s connect and 30s total deadline - without
one a half-open connection, the normal result of a laptop changing networks, hangs the shipper
indefinitely while the buffer grows behind it and no backoff branch is ever reached, because nothing
has failed yet. Backoff runs from 1s to a 5-minute ceiling on the retry path only.

## The buffer

SQLite at `{state_dir}/buffer.db`, `journal_mode = WAL`, `busy_timeout = 5000`, with every write
serialised through one owner task. The state directory is created `0700` and tightened to `0700` if
it already existed, because everything the daemon keeps on disk lives under it. That happens before
the config file is read, so an install upgrading from a looser mode is tightened even on a boot where
the config is rejected.

```
pending(id INTEGER PRIMARY KEY, envelope TEXT NOT NULL, bytes INTEGER NOT NULL, created_at TEXT NOT NULL)
cursors(provider TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(provider, key))
dead_letter(id INTEGER PRIMARY KEY, envelope TEXT NOT NULL, bytes INTEGER NOT NULL, reason TEXT NOT NULL, at TEXT NOT NULL)
meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)     -- holds the seq counter
```

`pending` is capped at `max_rows` and `max_bytes`, whichever is reached first. On overflow it evicts
oldest-first **until both totals are under their limits** - a flat "delete 10%" does not necessarily
satisfy `max_bytes`, which a handful of large records can dominate - then enqueues one
`buffer_overflow` record through the ordinary path, so the loss appears in the timeline with a real
`seq` and `dedup_key` instead of being invisible.

`dead_letter` has its **own separate** cap of 5 000 rows and 50 MB, evicting its own oldest rows.
Sharing one cap is a trap: once permanent failures filled it, every enqueue would trip overflow, live
records would be destroyed to make room for records that will never ship, and the overflow marker
reporting the loss would push the total up again.

At roughly 3 000 to 4 000 records a day, the default `max_rows` holds around seven weeks.

## The event thread

`AXObserver` callbacks, `NSWorkspace` notifications, distributed notifications and
`CGDisplayRegisterReconfigurationCallback` are all delivered by a **CFRunLoop**, and none of them fire
unless some thread is running one. A tokio worker is not a run loop, so the daemon owns exactly one
dedicated OS thread (`src/macos/events.rs`) that takes `CFRunLoopGetCurrent()`, adds every source to
that run loop in `kCFRunLoopDefaultMode`, calls `CFRunLoopRun()` and stays there. Each callback
converts its payload into a plain Rust value and sends it onto an mpsc channel; a callback carrying
transient state captures it **at callback time**, because resampling when the event is drained reads
the world after the transition it was meant to record. Shutdown is `CFRunLoopStop` on the stored
reference, then joining the thread.

**The Accessibility observers are attached from that thread, and only from there.** The registry holds
one `AXObserver` per pid owning a layer-0 window, each registered for `AXApplicationActivated`,
`AXApplicationDeactivated`, `AXFocusedWindowChanged`, `AXTitleChanged`, `AXWindowCreated` and
`AXUIElementDestroyed`. `AXObserverGetRunLoopSource` has to be added to the run loop that will
actually run; adding it from a tokio worker attaches it to a loop nobody pumps, which is the same
class of mistake that leaves an observer silently dead. An application whose activation resolves to no
`NSRunningApplication` is reported by pid rather than dropped, and a deactivation is only a hint that
an activation follows, so it produces no event of its own.

**Every tick asks the thread to re-scan the window owners**, which is how an application launched
after startup gets an observer - without the re-scan a switch to a pid never attached to produces no
event at all. The same pass detaches and releases the observer of a pid that no longer owns a window,
so a daemon running for weeks does not accumulate them. An application that refuses Accessibility -
two of nineteen on this machine - is warned about once and remembered as refusing, so it is not
retried on every pass; it is invisible to focus events for as long as it runs, and only the tick,
which reads the window list rather than the observers, still names it. A refusing pid is forgotten
once it goes away, so its successor at the same pid is tried again.

**`willSleep` is the one exception to post-and-return.** It sends its event with a completion handle
and blocks on the flush acknowledgement for at most 2 seconds. The service's liveness rule treats a
`sleep` marker as expected quiet and alarms without one, so a marker that only reaches the server
after wake inverts the check: the machine reads stale all night and healthy the moment it returns.
Only blocking inside the notification callback can delay the suspension; a handler that posts to a
channel and returns has already let the machine sleep. **The acknowledgement therefore waits on a
shipment, not only on the write**: the runtime commits the record, checkpoints the buffer, and asks
the shipper to drain now, which is what puts the marker on the server before the machine suspends. A
checkpoint alone would leave the marker on disk until wake, which is the inverted check the exception
exists to prevent. The 2s budget is a hard ceiling rather than a target - macOS grants a short window
and blocking past it risks the process being killed - and on expiry the callback returns anyway,
leaving the record durable in the buffer to ship on wake.

`NIKKI_TEST_EVENTS=<path>` makes the event thread read newline-delimited event values from that file
instead of registering OS sources. Without this seam the entire event half of the daemon could only
be exercised by a human doing things on a Mac. Each line is tab-separated and named by its first
field:

```
application_activated <pid> [name] [bundle_id]
focused_window_changed <pid>
title_changed <pid>
window_created <pid>
window_destroyed <pid>
displays_reconfigured
screen_locked
screen_unlocked
did_wake
will_sleep
```

A line that is not understood is logged at warn and skipped. The whole file is read at startup and
delivered in one drain, so it scripts a burst rather than a timeline.

## Layout

```
src/config.rs          config parsing and validation
src/macos/             every unsafe block in the crate
  ax.rs                Accessibility: the focused application, elements, titles, AXDocument, messaging timeout
  window_list.rs       CGWindowList, CGDisplayBounds, the focus choice and its filtered fallback, bundle id for pid
  activity.rs          idle seconds, input counters, microphone, cursor display
  screen.rs            the session lock state and per-display sleep
  processes.rs         the process table: argv, environment, controlling tty, cwd, agterm panes
  events.rs            the CFRunLoop thread, the AXObserver registry and every notification source
src/window/visibility.rs  the pure visible-set resolver
src/extract/           the bundle-id-keyed extractor registry
src/providers/         the Provider trait, the supervisor, and the two providers
src/runtime/           buffer, dedup keys, redaction, shipping
fixtures/              captured Dia output, agterm JSON, Local State, a small history database
```

## Known limitations

- Occlusion is not computed, so a fully covered window still counts as visible; `z` is recorded so it
  can be derived later.
- A non-focused visible window belonging to a multi-window application has no title, flagged as
  `title_reason: "ambiguous"`. There is no supported way to map a `kCGWindowNumber` to an
  `AXUIElement`: the Accessibility window array is unordered and carries no window number, two windows
  of one application can have byte-identical geometry, and matching by title is circular. When the
  owning application has exactly one Accessibility window the title is unambiguous and is used.
- A window counts as visible when its layer is 0 and it covers at least 20% of its own area on some
  active display. A window straddling two displays is attributed to the one with the largest
  intersection, ties broken by the lower index.
- The browser tab is captured only when the browser is the focused application, and only one
  profile's history is read. A rarely-used profile still leaves tab entries in window samples.
- `mic_active` means the input device is running, not that anyone is listening.
- A terminal pane running a setuid or hardened binary (`sudo`, `top`) reports no `command`:
  `KERN_PROCARGS2` declines to describe it, which is the same blind spot the agterm tree has. The
  pane the user is looking at is still named by `surface`.
- `command` names the leader of the pane's foreground process group, not every process in it. A
  helper the leader spawned without its own group - an MCP server under `claude`, a dev server under
  `mise dev` - is deliberately not reported, and neither is the second half of a pipeline.
- Windows applications running under a virtualiser in coherence mode appear as ordinary host windows
  with their own titles.
- No third-party window manager is used, so there is no tag or workspace grouping in the window layer.
- The service's coalescer ends a broken run at the last tick plus its full interval rather than at the
  event timestamp, and `focus` is not in its breaking set. Emitting `state_change` is this daemon's
  whole responsibility there; the duration only becomes correct once the service closes runs at the
  event timestamp.

Conventions for working in this repository are in `CLAUDE.md`.
