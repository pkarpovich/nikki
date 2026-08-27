# The agterm extractor learns about panes

## Overview

The agterm extractor reports the wrong thing whenever the user is not in a session's main pane.
It reads `agtermctl tree --json`, walks to the active session, and takes that session's
`name`, `cwd` and `foreground`. Those three fields describe the session's **left** (primary) pane.
A session can also show a **scratch** terminal - a third full-coverage shell toggled on top - and
when scratch is the visible surface, the left pane is hidden and everything the extractor reports
is about a pane nobody is looking at.

Measured on 2026-08-27: for 131 minutes the daemon recorded `foreground=claude` for the DC session,
while the visible surface was scratch running `revdiff` over a plan file. The record is not merely
incomplete - it names a program the user was not using.

This plan makes the extractor pane-aware. It resolves which surface is on screen, and what is
actually running in it, and reports both. The mechanism is entirely local: agterm stamps
`AGTERM_SESSION_ID`, `AGTERM_PANE` and `AGTERM_PANE_ID` into the environment of every process it
spawns, and a process's environment is readable for the same user with no permission at all. Joining
that against the tree gives pane, program and working directory for every live surface.

### Out of scope

- **The frozen `frontmost` / dead `focus` events.** Separate defect, separate plan: `NSWorkspace`
  needs a run loop on the **main** thread and the daemon gives its main thread to tokio. The
  `app` field is already corrected by deriving it from the window list; the missing `focus` events
  and the dead `sleep`/`wake` notifications are that plan's subject, not this one's.
- **A shell-history provider over atuin.** Also separate, and less urgent now: atuin sees only what a
  shell ran, so it misses every overlay and `--command` session (both revmux runs on 2026-08-27 are
  absent from it), while the process table sees all of them.
- **Asking upstream for `scratchForeground` in the tree.** It would be the tidier source, but this
  plan deliberately needs nothing from agterm that it does not already publish.

## Context (from discovery)

Every fact below was established by throwaway spikes run against the live machine on 2026-08-27 and
then deleted. The measurements are reproduced here because nothing else in the repository records
them.

**Verified on the live machine, 2026-08-27:**

- Every process agterm spawns carries `AGTERM_ENABLED=1`, `AGTERM_SESSION_ID`, `AGTERM_PANE`
  (`left` | `right` | `scratch`) and `AGTERM_PANE_ID` in its environment. Overlay programs carry
  them too.
- `sysctl(KERN_PROCARGS2)` returns argv **and** the environment for a process of the same uid, with
  no permission, no TCC prompt and no entitlement. It fails for setuid/hardened binaries (`top`,
  `sudo`), which the tree also declines to describe - the same blind spot, not a new one.
- A full pass over 1188 processes costs **50 ms**, so the tick can do this inline.
- Filtering matters twice over. The environment is inherited by everything a pane ever spawned, so a
  raw scan attributes `op daemon`, node servers and stale build processes to the session that
  happened to start them. Requiring a controlling tty removes those. Requiring the process to be its
  tty's **foreground process group** (`pgid == tpgid`) then picks the program the user sees rather
  than the parent shell sitting behind it.
- `proc_pidinfo(PROC_PIDVNODEPATHINFO)` gives each process's own cwd, which is what makes a scratch
  shell's directory knowable even though the tree only reports one cwd per session.
- The tree's `surfaces[]` array carries `kind`, `active` and `visible` per surface, including hidden
  ones. `foreground` and `splitForeground` describe the left and split panes; **there is no
  `scratchForeground`** - that is the gap this plan closes.
- Auto-named sessions carry an animated status glyph inside `name` itself (`✳ План создания`,
  `◑ План создания`, `●ask-dealcloud: done`); hand-named ones are stable (`nikki`, `nhop`). On
  2026-08-27 that animation produced 30 `state_change` records, six of them inside two seconds.

**Files this plan touches:**

- `src/extract/agterm.rs` - the extractor, currently session-level only.
- `src/macos/` - where every `unsafe` block in this crate lives and must keep living.
- `README.md` - the extractor registry section and the wire contract's `details` shape.
- `fixtures/agterm_tree.json` - the existing tree fixture, to be extended.

**Constraints that already hold and must keep holding:**

- Every `unsafe` block lives in `src/macos/`. The gate greps for it:
  `! grep -rn 'unsafe' src --include='*.rs' | grep -v '^src/macos/'`.
- No comments; clear names instead.
- An extractor is fallible and returns empty rather than failing the tick, and any subprocess it
  spawns carries a hard 2s deadline.
- `details` is an optional free-form object on `windows` records. The service stores it as JSON and
  validates nothing inside it, so new keys need no server change - but the wire contract in this
  repository's README documents the shape and the service repository carries the identical section.

## Development Approach

- **Testing approach**: regular - implement, then tests, within the same task.
- Complete each task fully before the next.
- Every task ends with tests as separate checklist items, and with `mise run check` passing.
- `mise run check` is `cargo fmt --all -- --check`, then `cargo clippy --all-targets -- -D warnings`,
  then `cargo test`. It must pass before the next task starts.
- Execution is native macOS. The toolchain compiles and links Apple frameworks for real, agterm is
  running on the machine, and `agtermctl` is at
  `/Applications/agterm.app/Contents/MacOS/agtermctl`. **No test may depend on a particular session
  layout being open** - the live tree is a developer's convenience, committed fixtures are the gate.
- Maintain backward compatibility: a machine without agterm, or with `agtermctl` missing, must
  behave exactly as it does today (the extractor warns once and returns empty).

## Testing Strategy

- **Unit tests** are required in every task, over committed fixtures, never over the live machine.
- **Pure functions carry the logic.** Parsing a `KERN_PROCARGS2` buffer, classifying a process as
  foreground, grouping panes and rendering `details` are all pure and testable without any syscall.
  The syscalls themselves are a thin unsafe shell in `src/macos/` with no branching worth testing.
- **No e2e suite exists**; `scripts/acceptance.sh` is the closest thing and gains one case here.

## Progress Tracking

- Mark completed items `[x]` immediately.
- Add newly discovered tasks with a ➕ prefix.
- Record blockers with a ⚠️ prefix.
- Update this plan when the scope changes.

## What Goes Where

- **Implementation Steps** are what this repository can do on its own.
- **Post-Completion** is what needs a human or another repository - mirroring the contract into the
  service repository, and watching real records after a release.

## Implementation Steps

### Task 1: Read a process's argv and environment

**Files:**
- Create: `src/macos/processes.rs`
- Modify: `src/macos/mod.rs`

- [x] declare `mod processes;` in `src/macos/mod.rs`
- [x] add `libc = "0.2"` to `Cargo.toml` dependencies
- [x] create `src/macos/processes.rs` with `pub struct ProcessArgs { pub argv: Vec<String>, pub env: HashMap<String, String> }`
- [x] implement the pure `parse_procargs(buffer: &[u8]) -> ProcessArgs`: read `argc` from the first
      four native-endian bytes, skip the executable path and the nul padding that follows it, take
      `argc` nul-terminated strings as argv, then take nul-terminated `KEY=VALUE` strings until an
      empty string or the end of the buffer; a string without `=` is skipped, not fatal
- [x] implement `pub fn read_args(pid: i32) -> Option<ProcessArgs>` over
      `sysctl(CTL_KERN, KERN_PROCARGS2, pid)`, sizing with a probe call and returning `None` on any
      failure (a restricted binary is a normal outcome, not an error to log per tick)
- [x] write tests for `parse_procargs` over a hand-built buffer: argv and env both recovered
- [x] write tests for `parse_procargs` edge cases: a buffer shorter than four bytes, `argc` larger
      than the strings present, an env entry without `=`, a trailing empty string ending the env
- [x] run `mise run check` - must pass before task 2

➕ `read_args` carries `#[expect(dead_code)]` until task 3 wires it up. `#[expect]` errors once the
expectation is fulfilled, so the attribute removes itself the moment `agterm_panes` calls it - no
blanket module allow, and no way to forget it.

### Task 2: Describe a process and its terminal

**Files:**
- Modify: `src/macos/processes.rs`

- [x] add `pub struct Process { pub pid: i32, pub pgid: i32, pub tdev: i32, pub tpgid: i32 }`
- [x] implement `pub fn list() -> Vec<Process>` over `proc_listpids(PROC_ALL_PIDS)` followed by
      `proc_pidinfo(PROC_PIDTBSDINFO)` per pid, reading `pbi_pgid`, `e_tdev` and `e_tpgid`; declare
      `PROC_ALL_PIDS` locally as `1` because `libc` does not export it
- [x] implement the pure `Process::has_tty(&self) -> bool` as `tdev != -1` and the pure
      `Process::is_foreground(&self) -> bool` as `has_tty() && pgid == tpgid`
- [x] implement `pub fn cwd(pid: i32) -> Option<String>` over `proc_pidinfo(PROC_PIDVNODEPATHINFO)`,
      returning `None` on a short read or an empty path
- [x] write tests for `has_tty` and `is_foreground` over constructed values: no tty, tty with a
      background pgid, tty with a matching pgid
- [x] run `mise run check` - must pass before task 3

➕ `e_tdev` and `e_tpgid` are `u32` in `libc`'s `proc_bsdinfo`, so "no controlling terminal" arrives
as `0xFFFFFFFF`; casting to `i32` is what makes the plan's `tdev != -1` the right test.

➕ `has_tty` and `is_foreground` carry `#[cfg_attr(not(test), expect(dead_code))]` rather than a bare
`#[expect]`: their tests already use them, so an unconditional expectation is unfulfilled in the test
build and `-D warnings` rejects it. `Process` needs no attribute at all - the derived `Debug` reads
every field.

### Task 3: Resolve agterm panes from the process table

**Files:**
- Modify: `src/macos/processes.rs`

- [x] add `pub struct Pane { pub session: String, pub pane: String, pub pane_id: String, pub argv: Vec<String>, pub cwd: Option<String> }`
- [x] implement the pure `pane_of(process: &Process, args: &ProcessArgs) -> Option<Pane>`: require
      `AGTERM_ENABLED == "1"` and a non-empty `AGTERM_SESSION_ID`, require
      `process.is_foreground()`, and default a missing `AGTERM_PANE` to `left`
- [x] implement `pub fn agterm_panes() -> Vec<Pane>` composing `list`, `read_args`, `pane_of` and
      `cwd`, uppercasing the session id so it joins against the tree case-insensitively
- [x] write tests for `pane_of` accepting a foreground agterm process and reading its pane
- [x] write tests for `pane_of` rejecting, one case each: a process with no `AGTERM_ENABLED`, one
      with the variables but no controlling tty (the inherited-environment daemon), and one with a
      tty that is not its foreground group (the parent shell)
- [x] write a test that a missing `AGTERM_PANE` is read as `left`
- [x] run `mise run check` - must pass before task 4

➕ Review finding: `pgid == tpgid` is true for **every** member of the foreground process group, not
only its leader, so `pane_on` returned whichever member `proc_listpids` happened to list first. On the
live machine that made `command` read `chrome-devtools-mcp` for a pane running `claude`, and `vite.js`
for one running `mise dev` - a value that flips between ticks as helpers come and go. `pane_of` now
also requires `pid == pgid`: the leader is the job the shell put in front, and a pane whose leader has
exited reports no command rather than one of its survivors. `is_foreground` and `leads_its_group` are
hoisted into `agterm_panes`'s loop so `read_args` no longer materialises the full environment of every
process on the machine before the filter that discards 97% of them.

➕ Review finding: the session-id uppercasing moved from `agterm_panes` into the pure `pane_of`, where
a test can reach it. It was the whole case-insensitive join and deleting it broke no test.

➕ `agterm_panes` becomes the only live root in the module, so the `#[expect(dead_code)]` on `list`,
`read_args`, `cwd`, `has_tty` and `is_foreground` had to go the moment it called them - a fulfilled
expectation is itself an error under `-D warnings`. `agterm_panes` now carries the attribute alone,
until task 5 wires it into the extractor.

### Task 4: Read surfaces out of the tree

**Files:**
- Modify: `src/extract/agterm.rs`
- Modify: `fixtures/agterm_tree.json`
- Create: `fixtures/agterm_tree_scratch.json`

- [x] extend the tree structs with `surfaces: Vec<Surface>` where `Surface { kind: String, active: bool, visible: bool }`, all `#[serde(default)]` so an older agterm that omits them still parses
- [x] implement the pure `active_surface(surfaces: &[Surface]) -> Option<String>` returning the
      `kind` of the first surface that is both `active` and `visible`
- [x] implement the pure `session_identity(name: &str) -> String` stripping the leading status glyph
      and whitespace from an auto-named session, leaving a hand-named one untouched
- [x] extend `fixtures/agterm_tree.json` so its active session carries a `surfaces` array with an
      active, visible `left`
- [x] create `fixtures/agterm_tree_scratch.json`: an active session whose `left` is inactive and
      hidden and whose `scratch` is active and visible, plus a second session with a split
- [x] write tests for `active_surface`: left active, scratch active, no surface active, an empty array
- [x] write tests for `session_identity` over each glyph seen in the wild (`✳`, `◑`, `◐`, `●`) and
      over a hand-named session that must survive unchanged
- [x] run `mise run check` - must pass before task 5

➕ `session_identity` matches a fixed glyph set rather than "any leading non-alphanumeric", so a
hand-named session starting with a symbol survives. The set carries both full spinner cycles the
observed glyphs belong to (`✳ ✢ ✶ ✻ ✽`, `◐ ◓ ◑ ◒`) plus `● ○`.

➕ `Surface`, the `surfaces` field, `active_surface` and `session_identity` all carry
`#[cfg_attr(not(test), expect(dead_code))]` until task 5 composes them: their tests read them, so an
unconditional `#[expect]` would be unfulfilled in the test build and `-D warnings` would reject it.

### Task 5: Report the pane that is on screen

**Files:**
- Modify: `src/extract/agterm.rs`

- [x] implement the pure `compose(tree: &TreeSession, panes: &[Pane]) -> Details` producing
      `workspace`, `session` (the stripped identity), `surface` (the active surface's kind), and,
      from the pane whose `AGTERM_PANE` equals that surface, `command` and `cwd`
- [x] emit `foreground` **only** when the active surface is `left`, so the field keeps meaning
      exactly what it means today and never describes a hidden pane
- [x] fall back to the session-level `cwd` when the matching pane reports none, and omit `command`
      entirely when no pane matches rather than guessing
- [x] cap `command` at 512 characters, appending `…` when it is cut, and ship argv otherwise whole -
      the arguments are the content (`rx <plan file>` says what was run, `rx` says nothing)
- [x] wire `compose` into `active_session()` so the extractor calls `agterm_panes()` once per
      invocation
- [x] write tests for `compose` over the scratch fixture: `surface` is `scratch`, `command` and `cwd`
      come from the scratch pane, `foreground` is absent
- [x] write tests for `compose` over the left fixture: `surface` is `left`, `foreground` present
- [x] write tests for `compose` with no matching pane: `surface` still reported, `command` absent
- [x] write a test that a 600-character argv is cut to 512 with the marker
- [x] run `mise run check` - must pass before task 6

➕ `compose` takes the workspace name beside the session rather than a `TreeSession` wrapper -
`workspace` lives on the tree's workspace node, not on its session, and a struct existing only to
carry the pair across one call is an abstraction the plan does not need.

➕ The join needs the session's `id`, which the extractor did not deserialise before; `Session` gains
`#[serde(default)] id: String`. `pane_on` uppercases it at the point of comparison, matching what
`agterm_panes` already does to `AGTERM_SESSION_ID`.

➕ Review finding: the `cwd` fallback is now gated on `surface == left`. The checklist item above asks
for it unconditionally, but the tree's session-level `cwd` describes the **left** pane - the same
reason `foreground` is gated - so falling back while scratch is on screen paired `surface: scratch`
with a directory nobody was looking at. That is the exact falsehood this plan exists to remove, and
`CLAUDE.md`'s new rule forbids it. The fallback still fires where it is truthful; with any other
surface on screen and no directory from the process, `cwd` is omitted.

➕ Review finding: `details.command` was exempt from every redaction rule. A user configuring
`drop = ["title"]` for `com.umputun.agterm` still shipped the whole argv, credentials in flags
included, and argv-borne URLs bypassed the host-only floor. `redact.rs` now nulls `details.command`
alongside `details.tab` under the same rule, and the README's Redaction section says why.

➕ Review finding: `a_session_running_nothing_carries_no_foreground` had gone vacuous - the fixture
session it selects carried `"surfaces": []`, so `compose` returned at the surface gate before reading
`foreground` at all. The fixture's `notes` session gained an active, visible `left`. The four
`compose` tests also went through `parse_tree`, deleting two test-local helpers that re-walked the
tree exactly as `parse_tree` does; a second copy of that walk can drift while the tests keep passing.

➕ A session whose `surfaces` array is absent or carries nothing active-and-visible now reports
neither `surface` nor `foreground`. That is the plan's rule read literally: without a known surface
the extractor cannot say the left pane is the one on screen, and the whole point of the change is to
stop asserting that blind.

### Task 6: Document the shape

**Files:**
- Modify: `README.md`

- [x] update the extractor registry entry for `agterm.rs` to name the fields it now produces
- [x] document in the wire contract that `details` for `com.umputun.agterm` carries `workspace`,
      `session`, `surface`, `command`, `cwd` and a conditional `foreground`
- [x] state why `foreground` is conditional: it is a session-level field describing the left pane,
      and reporting it while scratch is on screen is what this change fixes
- [x] state that `command` is capped and why it is otherwise whole
- [x] run `mise run check` - must pass before task 7

➕ The wire contract had no section describing `details` at all - the Dia shape lived only in the
extractor registry and in an example body. The new `### details on a window record` section documents
both extractors' shapes in the place the service repository mirrors, so the agterm keys are not the
only documented ones while Dia's stay implicit.

### Task 7: Verify acceptance criteria

**Files:**
- Modify: `scripts/acceptance.sh`

- [x] add an acceptance case asserting the binary reports a `surface` for the live tree when agterm
      is running, and skips cleanly when `agtermctl` is absent - modelled on the existing
      `PlistBuddy` checks, which return early rather than failing when a tool is missing
- [x] verify the inherited-environment daemon is excluded: assert `pane_of` rejects a process
      carrying the variables with no tty
- [x] run the full suite: `mise run check`
- [x] run `./scripts/acceptance.sh`
- [x] verify the `unsafe` containment gate: `! grep -rn 'unsafe' src --include='*.rs' | grep -v '^src/macos/'`
- [x] verify a machine without agterm still ticks: run with `agtermctl` unreachable and confirm the
      extractor returns empty rather than failing

➕ The live assertion is an `#[ignore]`d `#[tokio::test]` in `src/extract/agterm.rs` rather than shell
against the release binary: the binary has only `--check-config` and no way to print a tick, and the
test exercises the same `active_session()` the daemon calls. `scripts/acceptance.sh` runs it with
`--ignored`, so `cargo test` never touches the live machine. The run on 2026-08-27 reported
`surface=scratch`, `command=ralphex docs/plans/2026-08-27-agterm-panes.md` and no `foreground` - the
exact record the old extractor got wrong.

➕ The unreachable-`agtermctl` check used a failing stub first on `PATH` rather than a missing one:
`resolve_program` falls back to the hardcoded bundled path, which cannot be hidden without touching
`/Applications`. The extractor returned empty and the test passed, which is the behaviour the
checkbox asks for; the truly-absent branch is what the script's own guard skips on.

➕ `pane_of` rejecting the tty-less daemon was already asserted by
`a_daemon_inheriting_the_environment_is_not_a_pane` from task 3, so this task re-ran it rather than
writing a second one.

### Task 8: [Final] Update documentation

- [x] update `CLAUDE.md` if the pane-resolution rule is a pattern worth stating for future work

➕ The rule was worth stating: `CLAUDE.md` gains "An extractor reports the surface on screen", carrying
both halves - resolve the visible surface before reporting any field, and omit a field that could only
describe a hidden pane; and the two process-table filters (controlling tty, foreground process group)
with the lie each one answers.

## Technical Details

**The join.** The tree names sessions; the process table says what is running. They meet on
`AGTERM_SESSION_ID`, which is the tree's session `id` (case-insensitively - uppercase both sides).
Within a session, `AGTERM_PANE` meets the tree's `surfaces[].kind`. Comparing the pane a process
belongs to against the surface the tree calls active is what distinguishes "this program is on
screen" from "this program is alive in a hidden pane" - both are true facts and only the first
belongs in `surface`.

**Why the two filters are not optional.** They answer two different lies. Without the tty
requirement, a daemon started from a pane months ago still claims that pane, so `op daemon` becomes
"what the user is doing". Without the foreground-group requirement, the parent `fish` shows up
instead of the `claude` or `revdiff` running inside it, and every pane reports a shell.

**`KERN_PROCARGS2` layout**, since nothing in the repository parses it today:

```
int32  argc
char[] executable path, nul-terminated
char[] nul padding to alignment
char[] argv[0..argc], each nul-terminated
char[] KEY=VALUE, each nul-terminated, ending at an empty string or the buffer end
```

**Cost.** One `proc_listpids`, then two `proc_pidinfo` calls and one `sysctl` per pid. 50 ms for
1188 processes, measured. The extractor already spawns `agtermctl` with a 2s deadline, so this adds
no new latency class.

**Failure is always empty, never an error.** A restricted binary, a vanished pid, a tree that does
not parse, an `agtermctl` that is not installed - each yields fewer fields, never a failed tick.

## Post-Completion

**Mirror the contract.** `services/nikki/README.md` in the `turtle-hub` repository carries the
identical wire-contract section and must gain the same `details` description in the same pass. No
service code changes: `details` is stored as opaque JSON and validated only for its presence on
`buffer_overflow`.

**Watch real records after the release.** The one thing no fixture can prove is that the join holds
against a working day: switch into a scratch pane, run something recognisable, and confirm the
record names it. The failure this plan exists to fix was invisible for a full day precisely because
`foreground=claude` looked plausible.

**Consider `scratchForeground` upstream.** If agterm ever publishes per-surface `foreground` and
`cwd`, most of tasks 1-3 becomes redundant and the extractor collapses back to a tree read. Worth
raising as a Discussion, not a blocker.
