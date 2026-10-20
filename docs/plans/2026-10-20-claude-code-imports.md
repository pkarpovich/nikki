# claude_code imports: ship another machine's transcripts from a folder

## Overview

Some Claude Code work now happens on a second machine where nikki will never run. This change lets
the Mac daemon collect for that machine too. Every `[[claude_code.import]]` entry names:

- a folder of Claude Code transcripts that something else fills;
- the device name those transcripts belong to.

The daemon reads the folder with the existing transcript reader and ships what it finds under that
device. An entry may also carry a command, and the daemon runs it on a fixed period to refill the
folder.

The folder is the interface, and it is deliberately weak: `<dir>/<project>/<session>.jsonl`, with
content that is only ever appended to. How the bytes arrive (`scp`, ftp, a sync folder, a manual
copy) is the command's business, not the daemon's. Adding a machine whose transcripts arrive some
other way means writing a different command, or no command at all, and no daemon change.

Acceptance scenario:
- a second machine whose `~/.claude/projects` holds one large live session (about 120 MB, growing
  daily) and about a hundred finished ones;
- the configured command is a one-line `scp -r` of that directory into the import folder, run once a
  day;
- `since` is set to the moment that machine took over from the Mac;
- after a day, the service holds that machine's new messages under its own device name. Nothing from
  before `since` is resent, and nothing already shipped is resent.

When the command rewrites the folder the next day:
- the files get new inodes and are overwritten whole;
- the daemon ships only the lines appended since the last pull;
- it does not re-read the 120 MB file from the start.

### Non-goals

- The daemon speaks no transport protocol. There is no SSH, ftp or sync code in the crate, only "run
  this argv with a deadline".
- No `claude_code/touch` records for imported transcripts. Their paths belong to the other machine's
  file system, and resolving `.git` there from the Mac is not possible.
- No per-import `profile` setting. An imported machine has exactly one profile, and every imported
  record carries the fixed profile `claude`.
- No cron syntax, no time-of-day schedule, no new table. A period in seconds and one row in the
  existing `cursors` table are the whole schedule.
- No change to the wire contract, no new `(provider, kind)` pair and no service change.
- No pruning of the import folder and no cleanup of cursors for files that disappear from it.
- No second `seq` counter.

### Rejected alternatives

- **The daemon reads the remote machine over SSH itself.** Rejected because it ties the daemon to
  one transport, and every future machine reachable another way would need new code in the crate.
- **An external launchd agent or cron job fills the folder.** Rejected because the daemon owns the
  whole nikki flow, and a second scheduler is one more thing to install and watch.
- **The command preserves inodes by appending only new bytes (`tail -c +N >>`).** Rejected because it
  makes every transport responsible for a quirk of the reader. Instead, the reader stops depending on
  the inode for import folders.
- **A `device` field on `RecordDraft`.** Rejected because `RecordDraft` is constructed in about 70
  places, while one emission is always one file and therefore one device. The override lives on
  `Emission`.
- **A per-device `seq` counter.** Rejected after checking the service:
  - it enforces uniqueness only on `dedup_key`;
  - it uses `seq` only as an ordering tie-breaker after `ts`;
  - every dedup key already includes the device.

  One shared, monotonic counter stays correct for both devices.

## Skills to invoke

Load each skill below with the Skill tool and follow its conventions before implementing any task in
this plan.

- `rust-style` - all code under `src/` and `tests/` follows it.
- `rust-analyzer-ssr` - use it for the mechanical change to every `Emission` construction site and
  every `ClaudeCode` literal.

## Context (from discovery)

- **`src/config.rs`:**
  - `ClaudeCode { roots: Vec<ClaudeRoot>, poll_interval }` and `ClaudeRoot { profile, projects }`;
  - `claude_code_from` validates the section, and `claude_root_from` expands a leading `~/` against
    HOME and rejects relative paths;
  - `FileClaudeCode` is the `deny_unknown_fields` TOML shape;
  - `ClaudeCode` literals also exist at `src/config.rs` tests (around 677 and 695) and at
    `src/providers/mod.rs` (`test_config`, around 198).
- **`src/providers/claude_code.rs`:**
  - `ClaudeCodeProvider::run` ticks on `interval(poll_interval)`;
  - `tick` walks roots, then `transcripts_in` / `transcripts` list `<projects>/*/*.jsonl` sorted by
    path;
  - `ship_file` loops `read_increment` and sends `Emission::awaiting_buffer(drafts, Some(cursor))`,
    then advances the in-memory `known` cursor only after the receipt resolves;
  - `cursor_key(path)` is the absolute path string;
  - `FileCursor { inode, offset, last_message_ts, version }` uses `CURSOR_VERSION = 1`;
  - `read_increment(path, Source { profile, repos }, cursor, budget)` restarts from 0 when the inode
    changed or `offset > len`, and consumes only lines ending in `\n`;
  - `drafts_from_line` adds `touch_drafts` for assistant lines.
- **`src/providers/mod.rs`:** `Emission { records, cursor, committed, ships_before_commit }` with
  constructors `new`, `awaiting_commit`, `awaiting_buffer`.
- **`src/runtime/mod.rs`:**
  - `store` calls `BufferHandle::enqueue(drafts, cursor)`;
  - `RecordDraft::into_envelope(device, seq)` derives the dedup key from the device;
  - `Timestamp`.
- **`src/runtime/buffer.rs`:**
  - `enqueue(connection, device, caps, redactor, drafts, cursor)` gives every draft
    `into_envelope(device, seq)` from the one daemon-wide device;
  - the `buffer_overflow` marker uses the same device;
  - `BufferHandle::cursor(provider, key)` reads a cursor row;
  - an enqueue with no drafts and a cursor still commits the cursor.
- **`src/extract/mod.rs`:**
  - `run_with_deadline(program, args, deadline) -> Option<CommandOutput { succeeded, stdout, stderr }>`
    spawns with `kill_on_drop(true)` and kills on timeout;
  - it kills only the direct child, not its descendants.
- **`tests/stub_server.rs`:** runs the real binary with `config_text(...)` and a stub ingest server
  that records envelopes. `Transcripts::Fixture` enables the `[claude_code]` section.
- **`fixtures/claude_code_session.jsonl`:** the existing transcript fixture.
- **`README.md`:** `## Configuration` (TOML example and field table), the `claude_code` provider
  section, `### claude_code` in the wire contract, `### Identity`, and `## Known limitations`.
- **`CLAUDE.md` gate:**
  - `mise run check` plus the unsafe grep;
  - a change to `src/providers/claude_code.rs` also requires `./scripts/acceptance.sh`;
  - every subprocess has a deadline, and a provider never panics.

## Development Approach

- **Testing approach:** Regular (code first, then tests in the same task).
- Complete each task fully before moving to the next, in small, focused changes.
- **CRITICAL: every task MUST include new or updated tests** for the code it changes, covering both
  success and error scenarios.
- **CRITICAL: all tests must pass before starting the next task.**
- **CRITICAL: update this plan file when scope changes during implementation.**
- Tests live inline in the file they cover (`#[cfg(test)] mod tests`), and reach production code
  through the entry point the daemon calls.
- Local roots keep their exact current behaviour. Their tests must pass unchanged.

## Code-Quality Rules (verify before marking each task complete)

From `rust-style` (it has no `## Hard rules` block; these are its section rules):

- Use `for` loops, not iterator chains.
- Early returns use `let ... else`.
- Minimize `if let`; prefer `match`.
- Shadow variables rather than inventing new names for the same value.
- No comments.
- Prefer newtypes over strings, and strongly-typed enums over bools.
- No wildcard `_` matches on enums the crate owns.
- No `matches!`.
- Destructure structs explicitly.

From `CLAUDE.md`:

- No `unsafe` outside `src/macos/`.
- No blanket `#[allow(dead_code)]`.
- Every new module is declared.
- Every subprocess call has a deadline.
- A provider never panics.

Per-task gate:

```
mise run check
! grep -rn 'unsafe' src --include='*.rs' | grep -v '^src/macos/'
```

Any task touching `src/providers/claude_code.rs` also runs `./scripts/acceptance.sh`.

## Testing Strategy

- **Unit tests:** inline, per task. They cover config parsing and validation, the device override in
  the buffer, the reader's head identity and `since` filter, touch suppression, and the pull schedule.
- **End to end:** one new case in `tests/stub_server.rs`. It runs the real binary with an import
  whose command copies the fixture into the import folder, and asserts what reaches the stub server.
- No UI, no e2e browser tests.

## Progress Tracking

- Mark completed items with `[x]` immediately when done.
- Add newly discovered tasks with ➕, and document blockers with ⚠️.
- Keep the plan in sync with the work actually done.

## Solution Overview

Three pieces, each reusing what exists.

1. **The schedule is one cursor row.**
   - The row is `(claude_code, "import:<device>")` with value `{"last_attempt_at": <millis>}`.
   - On every existing claude_code poll tick, an import with a command checks whether
     `now - last_attempt_at >= every`. If so, it runs the command through `run_with_deadline` and then
     writes the current time.
   - Wall-clock time stored in SQLite survives restarts, upgrades and sleep. A missed day is caught
     on the first tick after it.
   - The attempt time is written whether the command succeeded or failed, so an unreachable machine
     costs one attempt per period rather than one per minute.
2. **Reading is the existing reader on another folder.**
   - After the local roots, each import's `dir` is listed with the same two-level glob and read with
     `read_increment`.
   - A `Source` describes how to read it: by head identity, with touches skipped and lines before
     `since` dropped, under the fixed profile `claude`.
   - The emission carries the import's device.
3. **The device rides on the emission.**
   - `Emission` gains an optional device. `store` passes it to the buffer, and `enqueue` seals those
     drafts with it instead of the daemon device.
   - `seq` stays the one shared counter. The `buffer_overflow` marker always uses the daemon's own
     device, because it describes this daemon's buffer.

The import cursor drops the inode, because a refilled folder gives every file a new inode. Identity
is the digest of the file's first line instead: Claude Code writes that line once and never rewrites
it, and it carries the session's identity. While a copy is in progress the file can briefly be
shorter than the stored offset, so with head identity a shorter file is waited for, not restarted.

## Technical Details

### Config

```toml
[[claude_code.import]]
device = "<device>"                                   # required
dir = "~/Library/Application Support/nikki/imports/<device>"   # required, ~/ or absolute
since = "2026-10-19T00:00:00+02:00"                   # required, RFC 3339 with offset
command = ["~/.config/nikki/pull-<device>.sh"]        # optional argv
every = 86400                                         # seconds, required with command, forbidden without
```

Resolved type in `src/config.rs`:

```rust
pub struct ClaudeImport { pub device: String, pub projects: PathBuf, pub since: Timestamp,
                          pub pull: Option<Pull> }
pub struct Pull { pub program: PathBuf, pub args: Vec<String>, pub every: u64 }
```

`ClaudeCode` gains `pub imports: Vec<ClaudeImport>`. The TOML key is `import`, the default is an
empty list, and `deny_unknown_fields` applies to each entry.

Validation (each failure is `ConfigError::Invalid` naming the field, for example
`claude_code.import.since`):

- **`device`:**
  - non-empty;
  - different from the top-level `device`, because records would otherwise be indistinguishable from
    this Mac's;
  - unique across imports.
- **`dir`:** expanded like a root (`~/` against HOME, otherwise absolute) and used directly as the
  projects directory, with no `/projects` appended. It does not have to exist at startup.
- **`since`:** parses as RFC 3339 with an offset. It is stored as a `Timestamp` in UTC millis.
- **`command` and `every`:**
  - both present or both absent;
  - `command` is non-empty, and its first element is expanded like `dir` and must end up absolute;
  - `every` is at least 1.

### Device on the emission

- `Emission` gains `pub device: Option<String>`. All existing constructors set `None`.
- A consuming builder `pub fn for_device(self, device: String) -> Emission` sets it.
- `BufferHandle::enqueue(drafts, cursor, device: Option<String>)` forwards to the buffer thread.
- Inside `enqueue`, each draft is sealed with `into_envelope(device_override.unwrap_or(daemon_device), seq)`.
  The overflow marker keeps `daemon_device`.
- Redaction runs exactly as today.

### Reader

`Source` gains three fields, as enums rather than bools:

```rust
pub enum Identity { Inode, Head }
pub enum Touches { Collected, Skipped }
pub since: Option<Timestamp>
```

`FileCursor` gains `#[serde(default)] pub head: Option<String>`, the first-line digest:
- `sha256` of the first line's bytes, including its `\n`, as 16 hex characters;
- computed with the existing `sha2` dependency, through a helper beside `dedup::key` or a local one.

`CURSOR_VERSION` stays 1, so stored local cursors keep working.

`read_increment` with `Identity::Inode` behaves exactly as today. With `Identity::Head`:

- **No complete first line yet:** yield nothing and keep the cursor.
- **Digest equals `cursor.head` and `len >= offset`:** continue from `offset`.
- **Digest equals `cursor.head` and `len < offset`:** a copy is in progress or the file was cut
  short. Yield nothing and keep the cursor.
- **Digest differs, or there is no cursor:** start from offset 0 with the new digest.
- **Returned cursor:** carries the digest and the current inode, which is informational only for
  `Head`.

The `since` filter is applied per draft after `drafts_from_line`:
- a draft whose `ts < since` is dropped;
- the offset still advances past its line, and `last_message_ts` is still tracked;
- a session record that takes its `ts` from an earlier message is therefore dropped with it.

`Touches::Skipped` makes `drafts_from_line` skip `touch_drafts`. `LineContext` carries the same enum
so that `drafts_from_line` stays the single entry point.

### Provider

- `tick(roots, imports, out)` reads the local roots exactly as now. Then, for each import:
  1. If it has a `Pull`, run `pull_if_due`.
  2. List `import.projects`.
  3. `ship_file` each path with the import `Source` and `Some(device)`.
- `ship_file` takes the `Source` (or what it needs to build it) and an `Option<&str>` device. A
  device present means `.for_device(...)` on the emission.
- `pull_if_due(import, out)`:
  1. Read the schedule cursor through `BufferHandle::cursor`. An absent or undecodable row means
     never attempted.
  2. Decide with `fn pull_due(last_attempt: Option<Timestamp>, now: Timestamp, every: u64) -> bool`.
  3. If due, run `run_with_deadline(program, args, PULL_DEADLINE)` with `PULL_DEADLINE` = 10
     minutes.
  4. On `None` or `succeeded == false`, log one `warn` with the device and the last 512 bytes of
     stderr. On success, log one `info`.
  5. In both cases send `Emission::awaiting_buffer(vec![], Some(schedule_cursor_now))` and await the
     receipt.
- A missing or unreadable import folder goes through the existing `transcripts_in` path: one `info`,
  then silence until it is readable.
- The pull runs inside the tick, so local transcripts wait for it at most `PULL_DEADLINE` once per
  period. That is accepted.
- `run_with_deadline` kills only its direct child. The README therefore tells the command to `exec`
  its transfer as the last step, so the killed child is the transfer itself.

## What Goes Where

- **Implementation Steps:** config, emission device, reader, provider, the end-to-end test and docs,
  all in this repository.
- **Post-Completion:** the real pull script and config on the Mac, a release, and a check on the
  service.

## Implementation Steps

### Task 1: Parse and validate `[[claude_code.import]]`

**Files:**
- Modify: `src/config.rs`
- Modify: `src/providers/mod.rs` (the `test_config` literal)

- [x] Add `ClaudeImport` and `Pull` (shapes in Technical Details), plus `imports` on `ClaudeCode`.
- [x] Add the `FileClaudeImport` TOML shape under `FileClaudeCode.import` (default empty,
  `deny_unknown_fields`).
- [x] Resolve in `claude_code_from`:
  - reuse the `~/` expansion from `claude_root_from` by extracting it into a shared function rather
    than copying it;
  - parse `since` with `chrono::DateTime::parse_from_rfc3339`;
  - apply every validation rule in Technical Details.
- [x] `claude_code_from` needs the top-level device for the "differs from the daemon device" rule:
  pass it in, or check after the section resolves in `parse`, whichever keeps the field name in the
  error.
- [x] Update every `ClaudeCode` literal (`src/config.rs` tests, `src/providers/mod.rs`) with
  `imports: Vec::new()`.
- [x] Write tests for a full entry, an entry without a command, and the default empty list.
- [x] Write tests for each rejection:
  - empty device;
  - device equal to the daemon's;
  - duplicate devices;
  - relative `dir`;
  - unparseable `since`;
  - `command` without `every`, and `every` without `command`;
  - empty `command`;
  - `every = 0`;
  - an unknown key.
- [x] Run `mise run check`.

### Task 2: Carry a device override on `Emission` into the buffer

**Files:**
- Modify: `src/providers/mod.rs`
- Modify: `src/runtime/mod.rs`
- Modify: `src/runtime/buffer.rs`

- [x] Add `device: Option<String>` to `Emission`, set to `None` in `new`, `awaiting_commit`,
  `awaiting_buffer` and the `#[cfg(test)]` constructor, and add `for_device(self, device: String)`.
- [x] Thread it through `store` into `BufferHandle::enqueue(drafts, cursor, device)`, the
  `Command::Enqueue` variant and `fn enqueue`. Drafts are sealed with the override when present.
  The overflow marker always uses the daemon device.
- [x] Update every existing `enqueue` call site, including tests, with `None`.
- [x] Write buffer tests:
  - an emission with an override seals envelopes whose `device` is the override and whose
    `dedup_key` equals the key computed for the override device;
  - `seq` keeps increasing across mixed-device enqueues;
  - an overflow triggered by an override enqueue writes its marker under the daemon device.
- [x] Write a test that an enqueue without an override is byte-for-byte what it was (an existing test
  passing unchanged is enough if it asserts the device).
- [x] Run `mise run check`.

### Task 3: Head identity, `since` and touch suppression in the reader

**Files:**
- Modify: `src/providers/claude_code.rs`

- [x] Add `Identity`, `Touches` and `since` to `Source`, and `Touches` to `LineContext`.
  `drafts_from_line` skips `touch_drafts` under `Touches::Skipped`.
- [x] Add `head: Option<String>` to `FileCursor` (`#[serde(default)]`), and the first-line digest
  helper.
- [x] Implement the `Identity::Head` rules in `read_increment` exactly as listed in Technical Details.
  The `Identity::Inode` path stays unchanged.
- [x] Apply the `since` filter per draft, while still advancing the offset and `last_message_ts`.
- [x] Update existing callers and tests to pass `Identity::Inode`, `Touches::Collected` and
  `since: None`.
- [x] Write tests through `read_increment` (using the existing `TempTranscripts` helpers):
  - a file replaced by a copy with identical content and a new inode yields nothing under `Head`,
    while the same replacement under `Inode` restarts from 0 (the existing behaviour);
  - an identical copy plus appended lines yields only the appended lines;
  - a file temporarily shorter than the offset with the same head yields nothing and keeps the
    cursor, and once it is whole again the read continues from the old offset;
  - a different first line restarts from 0;
  - a file with no complete first line yields nothing;
  - a stored cursor without `head` decodes, and under `Head` it restarts from 0;
  - `since` drops earlier messages and a title stamped from an earlier message, keeps later ones,
    and the offset reaches the end of the file;
  - `Touches::Skipped` yields no `touch` drafts for the tool-call fixture lines that do yield them
    under `Collected`.
- [x] Run `mise run check` and `./scripts/acceptance.sh` (the claude_code live case passed; the Accessibility live case cannot run here, as Accessibility is not granted to this shell).
- [x] ➕ `Identity::Head`, `Touches::Skipped`, `head_digest` and their imports are `#[cfg(test)]` until Task 4 constructs them; Task 4 removes those attributes. `FileCursor` lost `Copy` because of `head: Option<String>`.

### Task 4: Read import folders in the provider under their device

**Files:**
- Modify: `src/providers/claude_code.rs`

- [x] Remove the `#[cfg(test)]` attributes Task 3 left on `Identity::Head`, `Touches::Skipped`, `head_digest`, `HEAD_HEX_CHARS` and the `Write`/`Sha256` imports.
- [x] Add `const IMPORT_PROFILE: &str = "claude"`, and build the import `Source` with it:
  `Identity::Head`, `Touches::Skipped`, `since: Some(import.since)`.
- [x] `run` clones `imports` beside `roots`. `tick` reads the local roots first, then each import
  folder through the existing `transcripts_in` and `ship_file`.
- [x] `ship_file` takes the source settings and `Option<&str>` device, and calls `.for_device(...)`
  on the emission when a device is given.
- [x] Write provider-level tests. Drive `tick` with a real `Buffer`, as existing provider tests do,
  and assert what was enqueued:
  - an import folder's records carry the import device, profile `claude` and no touches;
  - a local root in the same tick still carries the daemon device and its touches;
  - a missing import folder is skipped without failing the tick.
- [x] Run `mise run check` and `./scripts/acceptance.sh`. (the claude_code live case passed; the Accessibility live case cannot run here, as Accessibility is not granted to this shell)

### Task 5: Run the pull command on its period

**Files:**
- Modify: `src/providers/claude_code.rs`

- [ ] Add the schedule cursor:
  - key `import:<device>`;
  - value JSON `{"last_attempt_at": <millis>}`, encoded and decoded like `FileCursor`, with garbage
    read as never attempted;
  - `pull_due(last_attempt, now, every)`;
  - `PULL_DEADLINE`.
- [ ] Implement `pull_if_due` as described in Technical Details, and call it in `tick` before
  listing that import's folder.
- [ ] Write tests for `pull_due`:
  - never attempted means due;
  - exactly `every` elapsed means due;
  - one second short is not due;
  - a clock that moved backwards (`now < last`) is not due.
- [ ] Write tests through `tick` with a real `Buffer` and a command that is a small shell script
  written into a temp dir:
  - the script copies the fixture into the import folder, and the records appear in the same tick;
  - a second tick within `every` does not run the script again (it appends to a counter file);
  - a script exiting non-zero still writes the attempt time, and the next tick does not retry;
  - a script outliving a shortened deadline is killed and the tick completes. Expose the deadline as
    a field the test can lower, not a test-only code path.
- [ ] Run `mise run check` and `./scripts/acceptance.sh`.

### Task 6: End-to-end import through the real binary

**Files:**
- Modify: `tests/stub_server.rs`

- [ ] Extend `Settings` / `config_text` with an optional import:
  - `device = "imported-device"`;
  - `dir` under the test's temp dir;
  - `since` between two fixture timestamps;
  - `command` = a script in the temp dir that copies the fixture into `dir/<project>/`;
  - `every = 3600`.
- [ ] Add a test asserting that the stub server receives:
  - `claude_code/message` records with `device = "imported-device"` and `profile = "claude"`;
  - no `claude_code/touch` records under that device;
  - none of the fixture messages before `since`;
  - this Mac's own records under the daemon device.
- [ ] Add a restart case:
  - stop the daemon, rewrite the import file with the same content plus one appended line, and start
    it again;
  - only the appended line's records arrive;
  - the command did not run again (counter file), because `every` has not elapsed.
- [ ] Run `mise run check` and `./scripts/acceptance.sh`.

### Task 7: Verify acceptance criteria

- [ ] Verify every Overview requirement against the acceptance scenario:
  - own device;
  - nothing before `since`;
  - a daily rewrite ships only appended lines;
  - one attempt per period, even on failure;
  - no transport code in the crate.
- [ ] Verify that local-root behaviour is unchanged: all pre-existing claude_code tests pass without
  edits beyond the new parameters.
- [ ] Run the full gate:
  - `mise run check`;
  - the unsafe grep;
  - `./scripts/acceptance.sh`.

### Task 8: [Final] Update documentation

- [ ] Update `README.md`:
  - **Configuration:** the `[[claude_code.import]]` example and the field-table rows.
  - **claude_code provider section:** a paragraph on imports:
    - the folder contract;
    - head identity and why the inode is not used;
    - `since`;
    - the schedule row and its attempt-time semantics;
    - no touches, and the fixed profile.
  - **An example command:** a script whose last line is `exec scp -rp <host>:.claude/projects/. "<dir>/"`,
    with the note that `exec` makes the deadline kill the transfer itself.
  - **`### Identity`:** an imported record's keys use the import device. Moving a session between
    machines therefore needs `since`, or its history is stored once per device.
  - **Known limitations:**
    - a title line before any message falls back to the file's modification time;
    - a command that does not preserve mtimes makes that the copy time;
    - nothing prunes the import folder or its cursors.
- [ ] Wire contract sections stay as they are. Confirm that no `(provider, kind)` pair or payload
  field changed.
- [ ] Update `CLAUDE.md` only if a new rule emerged. The `claude_code.rs` acceptance rule already
  covers this.
- [ ] Move this plan to `docs/plans/completed/`.

## Post-Completion

*Items requiring manual intervention or external systems - no checkboxes, informational only.*

**On the Mac:**
- write the pull script for the second machine;
- add its `[[claude_code.import]]` entry with `since` set to the hand-over moment;
- run `nikki --check-config`;
- release, upgrade, and watch `~/Library/Logs/nikki.log` for the first pull's `info` line.

**On the service:**
- confirm the new device appears in `GET /api/v1/devices` after the first pull;
- confirm the second machine's sessions list under it.

Two known effects on the service side:
- a session that moved between machines lists under the alphabetically smallest device, because
  `Sessions` takes `MIN(device)`;
- the service needs no change.

**Release ordering:** no new `(provider, kind)` pair, so the "service first" rule from the README
does not apply.
