# Capture screen lock and display sleep on every tick

## Overview

The daemon cannot currently tell "sitting here reading" from "walked away, machine locked". On the
night of 27-28 August it emitted 1433 ticks with `app: Dia` while the Mac sat locked with its screen
off, and the day screen honestly rendered that as three hours of browsing. The `lock`/`sleep` records
that were meant to mark such spans are never emitted at all - they arrive through
`NSWorkspace`/`NSDistributedNotificationCenter` notifications, which need a run loop on the **main**
thread, and main belongs to tokio.

Two polled probes answer the question without notifications, without permissions and without a run
loop, both proven on a spike against this Mac on 2026-08-29:

```
00:52:11   asleep=false  locked=absent    sitting at the machine
00:53:36   asleep=false  locked=true      Ctrl+Cmd+Q pressed
00:53:38   asleep=true   locked=true      panel went dark 2s later
00:54:26   asleep=false  locked=true      keyboard touched, panel lit
00:54:28   asleep=false  locked=absent    unlocked
```

- `CGSessionCopyCurrentDictionary()` carries the key `CGSSessionScreenIsLocked` **only while the
  session is locked** - its presence is the signal.
- `CGDisplayIsAsleep(display)` reports the physical panel, independently of the lock, which is what
  catches "walked away without locking and the screen slept on idle".

This plan adds both to every `windows/tick` as optional payload fields. The server half (storage,
coalescing, timeline output) is a sibling plan in the turtle-hub repository; this one also updates the
wire contract, which both READMEs carry and which must be mirrored in the same pass.

Rejected: fixing the notification path. It would need the main thread, it is a bigger change, and the
polled state is strictly better here - it also covers a machine that was already locked when the
daemon started, which an edge-triggered notification never reports.

## Context (from discovery)

- `src/macos/activity.rs` - where `idle_seconds()`, `input_counters()` and `microphone_active()` live;
  the same shape of cheap per-tick probe.
- `src/providers/windows.rs` - `Activity { idle_sec, counters, mic_active }` (line 47), the `Sources`
  trait (line 53) and `MacSources` (line 65) that the provider polls; payload assembly around line
  495; `FakeSources` in the test module (line 563).
- `README.md` - the wire contract: payload example (line ~332), the per-`(provider, kind)` field
  table (line 380), the semantics notes (line ~629).
- `Cargo.toml` already depends on `objc2-core-graphics` and `objc2-core-foundation`; the spike used
  raw `extern "C"`, but check the objc2 crates first - `CGDisplayIsAsleep` and the CF dictionary
  helpers may already be exposed, and matching the existing FFI style beats hand-rolled externs.

## Development Approach

- **Testing approach**: regular - write the code, then the tests, in the same task.
- Every task ends with tests and a green `cargo test`; no task starts on top of a red one.
- The two probes are FFI, so unit tests cover the pure reductions and the payload assembly, and the
  FFI itself is verified by the manual acceptance in Post-Completion.
- Keep the tick payload additive: both fields are **optional** on the wire, so a new daemon reporting
  to an old server, and an old daemon reporting to a new one, both keep working.

## Implementation Steps

### Task 1: Read the screen state from macOS
- [x] add `src/macos/screen.rs` with `screen_locked() -> bool` reading
      `CGSessionCopyCurrentDictionary()` and reporting whether `CGSSessionScreenIsLocked` is present
      and true (absent key = not locked, null dictionary = not locked)
- [x] add `displays_asleep() -> bool` reporting true only when there is at least one active display
      and **every** one of them is asleep (`CGGetActiveDisplayList` + `CGDisplayIsAsleep` per display)
      - a single external monitor sleeping while the laptop panel is lit must not read as "screen off"
- [x] factor the reduction out as a pure `fn all_asleep(states: &[bool]) -> bool` so it is testable
      without a display attached
- [x] register the module in `src/macos/mod.rs`
- [x] write tests for `all_asleep`: empty slice is false, all-true is true, any-false is false
- [x] run `cargo test` - must pass before task 2

Both probes were already exposed by `objc2-core-graphics`, so no hand-rolled `extern "C"` was needed;
the crate's `CGSession` and `libc` features had to be turned on in `Cargo.toml`. Until Task 2 wires
the module into `MacSources`, `cargo clippy -- -D warnings` reports every item in `screen.rs` as
never used - that is the compiler naming the unwired provider, and it clears in Task 2 rather than
being silenced with an allow.

### Task 2: Carry the state through the provider
- [x] extend `Activity` in `src/providers/windows.rs` with `screen_locked: bool` and
      `display_asleep: bool`
- [x] extend the `Sources` trait with the two readings and implement them in `MacSources` from Task 1
- [x] update `FakeSources` in the test module so existing tests compile with explicit values
- [x] write a test that the provider carries both flags from the source into the sample unchanged
- [x] run `cargo test` - must pass before task 3

The two readings ride on the `Activity` the `Sources` trait already returns rather than on two new
trait methods: the tick calls `activity()` once, so a separate method per reading would be a second
call per tick for nothing. `MacSources::activity()` fills them from `screen::screen_locked()` and
`screen::displays_asleep()`, which clears the unused-item warnings Task 1 left behind. The payload
does not carry them yet - that is Task 3 - so the provider test asserts through the `Activity` the
fake hands out on each tick, which is the whole of what the provider reads today.

### Task 3: Put both fields on the tick payload
- [ ] emit `screen_locked` and `display_asleep` in the tick payload beside `mic_active`
- [ ] write a payload test for the locked-and-dark case (`screen_locked: true`,
      `display_asleep: true`)
- [ ] write a payload test for the ordinary working case (both false)
- [ ] confirm the fields ride only on `tick`, not on `focus`/`state_change`, whose payloads are
      unchanged - add an assertion if none covers it
- [ ] run `cargo test` - must pass before task 4

### Task 4: Mirror the wire contract in the README
- [ ] add both fields to the `windows/tick` row of the `(provider, kind)` table as **optional**
- [ ] add them to the captured tick body example
- [ ] write the semantics paragraph: what each field means, that `screen_locked` is the session's own
      lock state and `display_asleep` is the panel, that they are independent, and that
      `display_asleep` is true only when every active display is asleep
- [ ] state plainly that these supersede the `lock`/`sleep` record kinds for the purpose of marking an
      unattended span, that those kinds remain in the contract but are still never emitted, and why
      (the notification path needs a main-thread run loop)
- [ ] note that the sibling change in turtle-hub carries the identical section

### Task 5: Verify acceptance criteria
- [ ] `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` all green
- [ ] `./scripts/acceptance.sh` passes
- [ ] confirm the payload additions are optional end to end: a tick built with both flags false still
      validates against the contract's required-field list

## Technical Details

`CGSSessionScreenIsLocked` is not a public constant - the key is read by name from the session
dictionary, and it is absent rather than false when unlocked. Treat absent, a non-boolean value, and a
null dictionary all as "not locked": this reading must never fail loudly, because a wrong `true` would
mark a working hour as absence.

The spike that proved both probes lives outside the repository and is not part of this change.

## Post-Completion

**Manual verification** (needs a human at the machine, cannot be automated here):
- run the built binary, lock the screen with Ctrl+Cmd+Q, wait for the panel to go dark, unlock, and
  confirm the shipped ticks carry `screen_locked` and `display_asleep` flipping in that order
- confirm a night: leave the machine locked and check the next morning that the whole span carries
  both flags, and that the day screen no longer reports it as browsing

**External system updates**:
- the turtle-hub sibling plan must land for the fields to be stored and reach the timeline; until it
  does, the fields survive only inside the stored `raw` envelope
- release a daemon version and upgrade the local install; the Homebrew upgrade drops the Accessibility
  grant, so it has to be re-granted afterwards
