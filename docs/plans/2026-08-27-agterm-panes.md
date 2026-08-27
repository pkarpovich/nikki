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

- [ ] declare `mod processes;` in `src/macos/mod.rs`
- [ ] add `libc = "0.2"` to `Cargo.toml` dependencies
- [ ] create `src/macos/processes.rs` with `pub struct ProcessArgs { pub argv: Vec<String>, pub env: HashMap<String, String> }`
- [ ] implement the pure `parse_procargs(buffer: &[u8]) -> ProcessArgs`: read `argc` from the first
      four native-endian bytes, skip the executable path and the nul padding that follows it, take
      `argc` nul-terminated strings as argv, then take nul-terminated `KEY=VALUE` strings until an
      empty string or the end of the buffer; a string without `=` is skipped, not fatal
- [ ] implement `pub fn read_args(pid: i32) -> Option<ProcessArgs>` over
      `sysctl(CTL_KERN, KERN_PROCARGS2, pid)`, sizing with a probe call and returning `None` on any
      failure (a restricted binary is a normal outcome, not an error to log per tick)
- [ ] write tests for `parse_procargs` over a hand-built buffer: argv and env both recovered
- [ ] write tests for `parse_procargs` edge cases: a buffer shorter than four bytes, `argc` larger
      than the strings present, an env entry without `=`, a trailing empty string ending the env
- [ ] run `mise run check` - must pass before task 2

### Task 2: Describe a process and its terminal

**Files:**
- Modify: `src/macos/processes.rs`

- [ ] add `pub struct Process { pub pid: i32, pub pgid: i32, pub tdev: i32, pub tpgid: i32 }`
- [ ] implement `pub fn list() -> Vec<Process>` over `proc_listpids(PROC_ALL_PIDS)` followed by
      `proc_pidinfo(PROC_PIDTBSDINFO)` per pid, reading `pbi_pgid`, `e_tdev` and `e_tpgid`; declare
      `PROC_ALL_PIDS` locally as `1` because `libc` does not export it
- [ ] implement the pure `Process::has_tty(&self) -> bool` as `tdev != -1` and the pure
      `Process::is_foreground(&self) -> bool` as `has_tty() && pgid == tpgid`
- [ ] implement `pub fn cwd(pid: i32) -> Option<String>` over `proc_pidinfo(PROC_PIDVNODEPATHINFO)`,
      returning `None` on a short read or an empty path
- [ ] write tests for `has_tty` and `is_foreground` over constructed values: no tty, tty with a
      background pgid, tty with a matching pgid
- [ ] run `mise run check` - must pass before task 3

### Task 3: Resolve agterm panes from the process table

**Files:**
- Modify: `src/macos/processes.rs`

- [ ] add `pub struct Pane { pub session: String, pub pane: String, pub pane_id: String, pub argv: Vec<String>, pub cwd: Option<String> }`
- [ ] implement the pure `pane_of(process: &Process, args: &ProcessArgs) -> Option<Pane>`: require
      `AGTERM_ENABLED == "1"` and a non-empty `AGTERM_SESSION_ID`, require
      `process.is_foreground()`, and default a missing `AGTERM_PANE` to `left`
- [ ] implement `pub fn agterm_panes() -> Vec<Pane>` composing `list`, `read_args`, `pane_of` and
      `cwd`, uppercasing the session id so it joins against the tree case-insensitively
- [ ] write tests for `pane_of` accepting a foreground agterm process and reading its pane
- [ ] write tests for `pane_of` rejecting, one case each: a process with no `AGTERM_ENABLED`, one
      with the variables but no controlling tty (the inherited-environment daemon), and one with a
      tty that is not its foreground group (the parent shell)
- [ ] write a test that a missing `AGTERM_PANE` is read as `left`
- [ ] run `mise run check` - must pass before task 4

### Task 4: Read surfaces out of the tree

**Files:**
- Modify: `src/extract/agterm.rs`
- Modify: `fixtures/agterm_tree.json`
- Create: `fixtures/agterm_tree_scratch.json`

- [ ] extend the tree structs with `surfaces: Vec<Surface>` where `Surface { kind: String, active: bool, visible: bool }`, all `#[serde(default)]` so an older agterm that omits them still parses
- [ ] implement the pure `active_surface(surfaces: &[Surface]) -> Option<String>` returning the
      `kind` of the first surface that is both `active` and `visible`
- [ ] implement the pure `session_identity(name: &str) -> String` stripping the leading status glyph
      and whitespace from an auto-named session, leaving a hand-named one untouched
- [ ] extend `fixtures/agterm_tree.json` so its active session carries a `surfaces` array with an
      active, visible `left`
- [ ] create `fixtures/agterm_tree_scratch.json`: an active session whose `left` is inactive and
      hidden and whose `scratch` is active and visible, plus a second session with a split
- [ ] write tests for `active_surface`: left active, scratch active, no surface active, an empty array
- [ ] write tests for `session_identity` over each glyph seen in the wild (`✳`, `◑`, `◐`, `●`) and
      over a hand-named session that must survive unchanged
- [ ] run `mise run check` - must pass before task 5

### Task 5: Report the pane that is on screen

**Files:**
- Modify: `src/extract/agterm.rs`

- [ ] implement the pure `compose(tree: &TreeSession, panes: &[Pane]) -> Details` producing
      `workspace`, `session` (the stripped identity), `surface` (the active surface's kind), and,
      from the pane whose `AGTERM_PANE` equals that surface, `command` and `cwd`
- [ ] emit `foreground` **only** when the active surface is `left`, so the field keeps meaning
      exactly what it means today and never describes a hidden pane
- [ ] fall back to the session-level `cwd` when the matching pane reports none, and omit `command`
      entirely when no pane matches rather than guessing
- [ ] cap `command` at 512 characters, appending `…` when it is cut, and ship argv otherwise whole -
      the arguments are the content (`rx <plan file>` says what was run, `rx` says nothing)
- [ ] wire `compose` into `active_session()` so the extractor calls `agterm_panes()` once per
      invocation
- [ ] write tests for `compose` over the scratch fixture: `surface` is `scratch`, `command` and `cwd`
      come from the scratch pane, `foreground` is absent
- [ ] write tests for `compose` over the left fixture: `surface` is `left`, `foreground` present
- [ ] write tests for `compose` with no matching pane: `surface` still reported, `command` absent
- [ ] write a test that a 600-character argv is cut to 512 with the marker
- [ ] run `mise run check` - must pass before task 6

### Task 6: Document the shape

**Files:**
- Modify: `README.md`

- [ ] update the extractor registry entry for `agterm.rs` to name the fields it now produces
- [ ] document in the wire contract that `details` for `com.umputun.agterm` carries `workspace`,
      `session`, `surface`, `command`, `cwd` and a conditional `foreground`
- [ ] state why `foreground` is conditional: it is a session-level field describing the left pane,
      and reporting it while scratch is on screen is what this change fixes
- [ ] state that `command` is capped and why it is otherwise whole
- [ ] run `mise run check` - must pass before task 7

### Task 7: Verify acceptance criteria

**Files:**
- Modify: `scripts/acceptance.sh`

- [ ] add an acceptance case asserting the binary reports a `surface` for the live tree when agterm
      is running, and skips cleanly when `agtermctl` is absent - modelled on the existing
      `PlistBuddy` checks, which return early rather than failing when a tool is missing
- [ ] verify the inherited-environment daemon is excluded: assert `pane_of` rejects a process
      carrying the variables with no tty
- [ ] run the full suite: `mise run check`
- [ ] run `./scripts/acceptance.sh`
- [ ] verify the `unsafe` containment gate: `! grep -rn 'unsafe' src --include='*.rs' | grep -v '^src/macos/'`
- [ ] verify a machine without agterm still ticks: run with `agtermctl` unreachable and confirm the
      extractor returns empty rather than failing

### Task 8: [Final] Update documentation

- [ ] update `CLAUDE.md` if the pane-resolution rule is a pattern worth stating for future work

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
