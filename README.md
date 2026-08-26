# nikki

A macOS daemon that records what was on screen and what was being done, and ships it to the nikki
service. One binary per Mac, delivered as an `LSUIElement` app bundle.

Toggl's Activity view reports only which application was frontmost and never what was being done in
it. This daemon captures the missing layer: window titles, the open document path, the browser tab
and profile, the terminal workspace and working directory, plus real activity signals - input
volume, idle, lock, sleep and microphone.

The daemon never interprets. It captures faithfully and ships; a summary of the day is produced
elsewhere, from the service API.

## Build

```
mise run build      # debug binary
mise run check      # fmt check, clippy with warnings denied, tests
./scripts/bundle.sh # target/nikki.app
./scripts/acceptance.sh  # daemon against the stub server in tests/stub_server.rs
```

The crate links Apple frameworks and builds only on macOS. Cross-compiling needs the Apple SDK.

## Permissions

| Permission | What it buys | Indicator / prompts |
|---|---|---|
| Accessibility | window titles, the focused window, `AXDocument` paths, title-change events | none |
| Automation -> Dia | the active tab's URL, title and profile | one-time prompt on first use |
| (none needed) | window list, geometry, z-order, displays, idle seconds, input counters, lock, sleep, microphone state | none |

Grant Accessibility to `nikki.app` rather than to a terminal: the bundle keeps the grant across
upgrades, and macOS attributes Accessibility to the *responsible* process, so a daemon launched from
an already-granted terminal inherits that terminal's trust.

**Screen Recording is deliberately never requested.** Holding it triggers a macOS re-consent dialog
roughly monthly which cannot be disabled, in exchange for `kCGWindowName` - a field this design gets
from Accessibility instead. `kCGWindowName` is therefore never read.

**The microphone is never opened.** `kAudioDevicePropertyDeviceIsRunningSomewhere` answers whether
the default input device is running without opening it, so no Microphone permission is requested and
no orange indicator lights. It means "the device is running", not "someone is listening" - some
applications hold it open idle - so `mic_active` is a hint and nothing more.

**Automation is what the Dia extractor needs.** `Info.plist` must carry a non-empty
`NSAppleEventsUsageDescription`; without that string macOS never shows the prompt, the extractor is
denied for the life of the bundle, and the only symptom is a `-1743` in the log. A declined prompt is
logged once at warn level, because no tab will ever be captured until a human fixes it in System
Settings.

### Degraded capture

When Accessibility is not granted, or its calls fail, capture continues: application, bundle id,
geometry, `display` and every activity signal still work, because none of them touch Accessibility.
Titles, paths and extractor details go null and the record carries `degraded: true`, which reaches
the service and the API - a degraded day must not be indistinguishable from a quiet one to whoever
reads the timeline later.

## Configuration

`~/.config/nikki/config.toml`, or the path in `NIKKI_CONFIG`. `NIKKI_STATE_DIR` overrides the
directory holding `buffer.db` (default `~/Library/Application Support/nikki`). Both overrides exist
so a test harness never touches the operator's real state.

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
| `service_url` | none, required | must parse as an absolute http or https URL with a non-empty host |
| `device` | none, required | a component of every `dedup_key` and an indexed server column, so an empty one produces records that store but can never be attributed to a machine |
| `tick_interval` | 30 | validated at startup against the same `[1, 3600]` bound the server enforces per record |
| `history_poll_interval` | 300 | |
| `revisit_window` | 500 | rows re-read below the cursor so a filled-in `visit_duration` is picked up |
| `browser.profile` | none, required | resolved to a directory through `Local State` on every poll; a name absent at startup is fatal and lists the names that do exist |
| `buffer.max_rows` | 200000 | roughly seven weeks at the real record rate |
| `buffer.max_bytes` | 536870912 | 500 MB |
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
- **`drop = ["title"]` applies to `visible[]` entries too**, not only the top-level title.

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

`Emission::awaiting_commit` returns a receipt the caller can block on - this is how the sleep marker
is made durable before macOS suspends the machine.

Each provider runs as one tokio task under `providers::supervise`, which restarts it with
exponential backoff from 1s to a 5-minute ceiling. A panic is caught and restarted like any other
failure; **a failing provider restarts alone and never takes the process down.** The supervisor lives
in `src/providers/mod.rs` beside the trait, because it is provider lifecycle rather than a buffer or
transport concern. Exactly one module owns it.

### The two providers

**`windows`** (`src/providers/windows.rs`) is event-driven with a heartbeat. Its sources are
`NSWorkspace` activation, sleep and wake; distributed lock and unlock; an `AXObserver` on the
frontmost application watching focused-window, title, created and destroyed, re-registered whenever
that application changes; display reconfiguration; and a tick every `tick_interval`.

Sample assembly runs in a fixed order: window list and displays first, then the frontmost
application's front-most on-screen window - which is where `display` comes from, **not** Accessibility,
because `display` is required on every `tick` and `focus` and a null one is rejected and then
deleted, which would destroy the stream on day one before the human has granted the permission. Then
the focused window's title and `AXDocument`, then titles for other visible windows, then the
extractor for the focused bundle, then idle, input counters and microphone state.

Event-driven assembly is **debounced at 300 ms** - an event schedules assembly, further events reset
the timer, one sample is emitted when it expires. The heartbeat tick is not debounced. Without this a
burst of AX notifications would spawn an `osascript` and an `agtermctl` per notification, and each
`state_change` also breaks a coalesced run on the server.

**`browser_history`** (`src/providers/browser_history.rs`) polls on `history_poll_interval`. The
browser holds the active profile's `History` locked against any external reader, so the file is
cloned with `clonefile` along with its `History-journal` sidecar, the copy is opened read-only, and
`PRAGMA quick_check` must return `ok` before anything is read - a torn snapshot frequently opens
cleanly and reads as good data, which would ship wrong rows and advance the cursor past the right
ones. The copy is deleted after the read.

Each poll reads `id > cursor - revisit_window` rather than `id > cursor`, because Chromium fills
`visit_duration` in by updating the row it wrote when the visit began. A re-read row is emitted
**only when its `title`, `transition` or `visit_duration` changed** since it was last shipped; that is
the difference between roughly 3 000 records a day and 144 000.

### Adding a provider

1. Write `src/providers/<name>.rs` and declare it in `src/providers/mod.rs`.
2. Implement `Provider`. Return `Err(ProviderError)` rather than panicking, and never block the
   runtime - the supervisor can only act on a returned error, not on a hang.
3. Emit `Emission::new(records)`, or `Emission::with_cursor(records, cursor)` when the provider has
   durable progress to record. Never persist a cursor yourself.
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
- `agterm.rs` - `agtermctl tree --json` against `com.umputun.agterm`, producing
  `{workspace, session, cwd, foreground}`.

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
| `windows` | `tick` | `app`, `bundle_id`, `display`, `tick_interval_sec`, `idle_sec`, `keys_delta`, `mouse_delta`, `mic_active`, `visible` | `title`, `path`, `details` |
| `windows` | `focus` | `app`, `bundle_id`, `display` | `title`, `path`, `details`, `visible` |
| `windows` | `state_change` | `app`, `bundle_id`, `display` | `title`, `path`, `details`, `visible` |
| `windows` | `lock`, `unlock`, `sleep`, `wake` | none | none |
| `windows` | `buffer_overflow` | `details` carrying `dropped`, `dropped_from`, `dropped_to` | none |
| `browser_history` | `visit` | `url`, `profile`, `visit_id` | `title`, `transition`, `duration_ms` |

Unknown payload fields are never rejected - the server preserves them in its `raw` column.

`tick_interval_sec` rides on **every** tick rather than being assumed as a server-side global, which
is what keeps a second Mac on a 60-second interval from halving that day's reported totals and keeps
historical rows meaning what they meant when recorded.

`state_change` reports that something about the focused window changed without focus moving: a title
rewrite, a window created or destroyed, a display reconfiguration. None of those fire an application
activation or a focus change, and a title rewrite is the only signal for switching files inside an
editor, a build finishing and rewriting a terminal title, or switching between two already-loaded
tabs in one browser window - which writes no history row either. The server breaks a coalesced run on
it and gives it no duration; the tick that follows opens the run where the duration lives.

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
| `401`, `403`, `404`, `405` | **keep** the batch, back off, log at error every time. These are configuration - a wrong `service_url`, a proxy, a service not deployed yet - and dead-lettering here would feed the entire capture into a bin one batch at a time while the daemon looked healthy. |
| other `4xx` (400, 409, 422) | move to `dead_letter`, log at error, continue with the next batch. |
| `413` | halve the batch size and retry, down to a floor of 10 records; below that, dead-letter. |
| `5xx`, timeout, connection failure | keep and back off. The only path that retries the same bytes. |

Batches are up to 500 records. Every request carries a 10s connect and 30s total deadline - without
one a half-open connection, the normal result of a laptop changing networks, hangs the shipper
indefinitely while the buffer grows behind it and no backoff branch is ever reached, because nothing
has failed yet. Backoff runs from 1s to a 5-minute ceiling on the retry path only.

## The buffer

SQLite at `{state_dir}/buffer.db`, `journal_mode = WAL`, `busy_timeout = 5000`, with every write
serialised through one owner task.

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

**`willSleep` is the one exception to post-and-return.** It sends its event with a completion handle
and blocks on the flush acknowledgement for at most 2 seconds. The service's liveness rule treats a
`sleep` marker as expected quiet and alarms without one, so a marker that only reaches the server
after wake inverts the check: the machine reads stale all night and healthy the moment it returns.
Only blocking inside the notification callback can delay the suspension; a handler that posts to a
channel and returns has already let the machine sleep. The 2s budget is a hard ceiling rather than a
target - macOS grants a short window and blocking past it risks the process being killed - and on
expiry the callback returns anyway, leaving the record durable in the buffer to ship on wake.

`NIKKI_TEST_EVENTS=<path>` makes the event thread read newline-delimited event values from that file
instead of registering OS sources. Without this seam the entire event half of the daemon could only
be exercised by a human doing things on a Mac.

## Layout

```
src/config.rs          config parsing and validation
src/macos/             every unsafe block in the crate
  ax.rs                Accessibility: elements, titles, AXDocument, messaging timeout
  window_list.rs       CGWindowList, CGDisplayBounds, frontmost application, bundle id for pid
  activity.rs          idle seconds, input counters, microphone, cursor display
  events.rs            the CFRunLoop thread and every notification source
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
- Windows applications running under a virtualiser in coherence mode appear as ordinary host windows
  with their own titles.
- No third-party window manager is used, so there is no tag or workspace grouping in the window layer.
- The service's coalescer ends a broken run at the last tick plus its full interval rather than at the
  event timestamp, and `focus` is not in its breaking set. Emitting `state_change` is this daemon's
  whole responsibility there; the duration only becomes correct once the service closes runs at the
  event timestamp.

Conventions for working in this repository are in `CLAUDE.md`.
