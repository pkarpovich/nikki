# claude_code provider: ship Claude Code conversations to nikki

## Overview

A third provider, `claude_code`, reads the Claude Code session transcripts on this Mac and ships the conversation as text: every prompt the user typed and every text reply the model wrote, one record per text block, with the text unmodified. Tool calls, tool results, thinking blocks, images, injected skill bodies and subagent transcripts never leave the Mac. The service half (storage, sessions API, search) is a sibling plan in the service repository (`turtle-hub`, `docs/plans/2026-09-23-nikki-claude-code-sessions.md`); the wire contract below is identical in both.

Why: "what did I do last week, and when" cannot be answered from nikki today. The timeline shows that a terminal window titled with a Claude Code session name was focused from 13:30 to 14:30, and nothing of what was discussed. The conversation itself is the only source that answers it, and it lives only in local transcript files that Claude Code prunes on its own schedule. The README already lists "agent session transcripts" as a deferred provider the architecture admits; this plan builds it.

Acceptance scenario: after this daemon has shipped the existing transcripts, an agent using only nikki's API describes the week of 13-20 September 2026 and names, for Monday 14 September 13:30-14:30, the THE_FEUD_V2 dev redeploy via spot - text that exists only in the shipped messages.

The provider interprets nothing beyond classifying which transcript lines are conversation. It does not summarise, shorten, redact or merge text.

## Context (from discovery)

- `src/providers/mod.rs` - `Provider` trait (`name`, `run(ctx, out)`), `Emission::new` / `Emission::awaiting_commit(records, cursor)` (records and cursor committed in one buffer transaction; the receipt resolves only after both are durable), `supervise` with restart backoff. Test helpers `tests::test_config` / `test_ctx` build a `Config` literal - a new `Config` field must be added there.
- `src/providers/browser_history.rs` - the model to follow: a polling loop on `tokio::time::interval` with `MissedTickBehavior::Delay`, cursor read once via `BufferHandle::cursor(provider, key)`, per-poll `Emission::awaiting_commit`, cursor state kept in memory and advanced only after `committed.await` succeeds, JSON-encoded cursor value with a tolerant decode that restarts on garbage.
- `src/runtime/mod.rs` - `Provider` and `Kind` enums with `as_str` wire names, `KeySource` (drives `dedup_key` in `RecordDraft::into_envelope`), `Timestamp::from_millis`, `Cursor { provider, key, value }`, the `every_provider_and_kind_carries_its_wire_name` test.
- `src/runtime/dedup.rs` - `key(fields)` = sha256 over `\x1F`-joined fields, first 16 hex chars; `windows_key`, `browser_key` and their tests.
- `src/runtime/buffer.rs` - `enqueue` runs `Redactor::apply` on every payload and stores the cursor in the same transaction; `cursors(provider, key, value)` table.
- `src/runtime/redact.rs` - `apply` only touches the payload keys `url`, `title`, `details`, `visible` (and reads `bundle_id`). A `claude_code` payload must use none of those names, so redaction never alters conversation text.
- `src/config.rs` - `FileConfig` (TOML, `deny_unknown_fields` on nested sections), `parse` with per-field validation (`HISTORY_POLL_INTERVAL_MIN` pattern), `Config` struct, `Paths` (HOME resolution).
- `src/main.rs` - `run` spawns each provider under `supervise`, aborts and awaits them on shutdown; `PROVIDERS` constant lists provider names for the startup log.
- `tests/stub_server.rs` - end-to-end harness: runs the real binary with `HOME` pointed at a temp dir, a generated `config.toml` (`config_text`), and a stub ingest server that records envelopes.
- `README.md` - `## Configuration` (the TOML example), `## The provider model` / `### The two providers` / `### Adding a provider` (steps 1-6), `## The wire contract` / `### Every (provider, kind) pair` / `### Identity`, `## Known limitations`.
- `CLAUDE.md` - per-task gate `mise run check` (fmt, clippy `-D warnings`, test) plus `! grep -rn 'unsafe' src --include='*.rs' | grep -v '^src/macos/'`; tests inline in the file they cover; every module declared; no blanket `dead_code` allows; a provider never panics; a test reading the live machine is `#[ignore]`d and run from `scripts/acceptance.sh`.
- Everything builds and runs natively on macOS only.

Transcript facts, measured on the user's real `~/.claude/projects` (396 session files, 988 MB):
- layout: `<profile_dir>/projects/<project-dir>/<session-id>.jsonl` is one session; subagent transcripts live one level deeper at `<project-dir>/<session-id>/subagents/*.jsonl` and are **not** read;
- one JSON object per line, appended as the session runs; a line may be written while the daemon reads it, so the last line of a file can be incomplete;
- conversation lines have `type` `user` or `assistant` and carry `uuid`, `sessionId`, `timestamp` (RFC 3339), `cwd`, `gitBranch`, `isSidechain`, `message.content` (a string, or an array of blocks with `type` `text`, `tool_use`, `tool_result`, `thinking`, `image`);
- `isMeta: true` user lines are harness injections (97% of them are skill bodies, 9 MB) - skipped; `isCompactSummary: true` user lines are the model's own summary at context compaction - kept;
- 1410 `uuid`s appear in more than one file (a resumed or forked session copies history into its new file) - identity is `(session_id, uuid, block)`;
- 9 of 21598 conversation lines hold more than one text block - hence `block`;
- metadata lines: `{"type":"custom-title","customTitle":...,"sessionId":...}`, `{"type":"ai-title","aiTitle":...,"sessionId":...}` (no timestamp, repeated many times per file), `{"type":"pr-link","sessionId":...,"prNumber":...,"prUrl":...,"prRepository":...,"timestamp":...}`;
- every other `type` (`attachment`, `system`, `mode`, `permission-mode`, `last-prompt`, `file-history-snapshot`, `queue-operation`, `cost-state`, ...) is skipped;
- non-meta user text starts with `<command-name>`/`<command-message>` (slash commands, with `<command-args>`), `<bash-input>` (a `!` command), `<local-command-stdout>`/`<bash-stdout>` (their output), `<task-notification>`, or is plain typed text (including `[Request interrupted by user]` and `[Image ...]` markers);
- largest single text block 906 KB; conversation text in total about 20 MB per Mac.

## Development Approach

- **Testing approach**: regular - code, then tests, in the same task.
- Tests inline in the file they cover (`#[cfg(test)] mod tests`), reaching production code through the entry point the provider calls. Fixtures live in `fixtures/`.
- Every task ends with `mise run check` green and the unsafe grep empty, before the next one starts.
- Additive: with no transcript directory present, the daemon behaves exactly as today (the provider logs once and idles), and every existing test passes unchanged.
- Style per `CLAUDE.md` and the repo: no comments, early returns / `let ... else`, explicit destructuring, no wildcard `_ =>` match arms on project enums, a failing provider returns `Err(ProviderError)` and never panics.

## Implementation Steps

### Task 1: Wire names and identity
- [ ] add `Provider::ClaudeCode` (`"claude_code"`) and `Kind::Message` (`"message"`), `Kind::Session` (`"session"`) in `src/runtime/mod.rs`; extend `every_provider_and_kind_carries_its_wire_name`
- [ ] add `KeySource::ClaudeMessage { session_id: String, uuid: String, block: u32 }` and `KeySource::ClaudeSession { session_id: String, field: String, value: String }`, mapped in `into_envelope` to new `dedup::claude_message_key` / `dedup::claude_session_key`
- [ ] in `src/runtime/dedup.rs`: `claude_message_key(device, session_id, uuid, block)` = `key([device, "claude_code", "message", session_id, uuid, block])` and `claude_session_key(device, session_id, field, value)` = `key([device, "claude_code", "session", session_id, field, value])`
- [ ] write dedup tests in the style of the existing ones: exact hash of the joined fields; every field changes the key; the key ignores `seq` (the same message enqueued twice gets the same key)
- [ ] `mise run check` - must pass before task 2

### Task 2: Configuration
- [ ] add an optional `[claude_code]` section to `FileConfig` (`deny_unknown_fields`): `roots` (array of strings, default `["~/.claude", "~/.claude-work"]`) and `poll_interval` (seconds, default 60, minimum 1 - reject below with the same reasoning as `history_poll_interval`)
- [ ] resolve each root: a leading `~/` expands against `HOME` (`Paths` already requires it); an absolute path is taken as is; a relative path without `~/` is a config error; an empty list is valid and disables the provider
- [ ] each resolved root yields a `ClaudeRoot { profile, projects }`: `projects` = `<root>/projects`, `profile` = the root directory's final component with one leading `.` stripped (`~/.claude` -> `claude`, `~/.claude-work` -> `claude-work`); two roots resolving to the same profile name are a config error
- [ ] add `claude_code: ClaudeCode { roots: Vec<ClaudeRoot>, poll_interval: u64 }` to `Config`, and to `providers::tests::test_config`
- [ ] write config tests: section absent -> both defaults; custom roots and interval; `poll_interval = 0` rejected; relative root rejected; duplicate profile rejected; unknown key in the section rejected
- [ ] `mise run check` - must pass before task 3

### Task 3: Classify one transcript line
- [ ] create `src/providers/claude_code.rs` (declare it in `providers/mod.rs`) with a pure function `drafts_from_line(line: &Value, context: &LineContext) -> LineOutcome` where `LineContext { profile, fallback_session_id, last_message_ts: Option<Timestamp>, file_modified: Timestamp }` and `LineOutcome` carries the drafts plus the message `ts` seen on this line (for the next title)
- [ ] `user`/`assistant` lines (skip when `isSidechain` is `true`; skip `user` lines with `isMeta` `true`): take `message.content`; a string is one text block, an array contributes only its `type: "text"` blocks; `block` is the index among the line's text blocks, from 0; skip the line when it has no text block, no `uuid`, or no parseable `timestamp`
- [ ] one `claude_code/message` draft per text block, `ts` = the line's `timestamp`, payload exactly per the contract in Technical Details: `session_id` (line `sessionId`, else the file stem), `uuid`, `block`, `role` (`user`/`assistant`), `message_kind`, `text` (the block text, byte-for-byte), `cwd`, `git_branch` (omitted when absent or empty), `profile`
- [ ] `message_kind` for `user` lines: `compact_summary` when `isCompactSummary` is `true`; else by the text's leading tag after leading whitespace - `<command-name>`, `<command-message>`, `<bash-input>` -> `command`; `<local-command-stdout>`, `<local-command-stderr>`, `<bash-stdout>`, `<bash-stderr>` -> `command_output`; `<task-notification>` -> `notification`; anything else -> `prompt`. For `assistant` lines always `text`. Implement the tag table as a `match` on an enum, not a wildcard
- [ ] metadata lines -> one `claude_code/session` draft: `custom-title` -> `field: "custom_title"`, `value: customTitle`; `ai-title` -> `ai_title`/`aiTitle`; `pr-link` -> `pr`/`prUrl`. `ts`: the line's own `timestamp` when present (pr-link), else `last_message_ts`, else `file_modified`. Skip when the value is missing or empty
- [ ] every other `type` yields nothing
- [ ] write tests against a committed fixture `fixtures/claude_code_session.jsonl` (content specified in Technical Details), feeding lines one by one: the exact sequence of drafts and payloads; the multi-block line yields `block` 0 and 1 with the same `uuid`; the tool/thinking/meta/sidechain/attachment lines yield nothing; each `message_kind` case; titles take the previous message's `ts`; a title before any message takes `file_modified`; a payload carries no `url`/`title`/`details`/`visible` key (so the redactor cannot touch it); the text of the reply containing a URL and a token-like string is shipped unchanged
- [ ] `mise run check` - must pass before task 4

### Task 4: Read a file incrementally with a durable cursor
- [ ] `FileCursor { inode: u64, offset: u64, last_message_ts: Option<i64> }`, JSON-encoded as the cursor value under `Cursor { provider: ClaudeCode, key: <absolute file path> }`; an undecodable value restarts the file from 0 with a warn (dedup absorbs the repeats)
- [ ] `read_increment(path, profile, cursor: Option<FileCursor>, budget: ReadBudget) -> io::Result<Option<Increment>>` where `Increment { drafts, cursor: FileCursor }`: stat the file; a changed inode or a length below `offset` restarts from 0; length equal to `offset` returns `None`; otherwise open, seek to `offset`, and read line by line with `BufRead::read_until(b'\n')`
- [ ] only a line ending in `\n` is consumed - a trailing partial line is left for the next poll and `offset` stops before it
- [ ] a line that is not valid JSON is logged at warn with the path and offset and skipped (its bytes are consumed), never aborting the file
- [ ] stop the increment once it holds `ReadBudget.max_records` (500) drafts or `max_bytes` (4 MiB) of text, so a first read of a large file ships in bounded emissions; the returned `offset` is the end of the last consumed line
- [ ] write tests on temp files: a fresh file reads fully; appending lines and reading again yields only the new ones; a partial last line is not consumed until its `\n` arrives; a replaced file (new inode) and a truncated file restart from 0; malformed line skipped and later lines still read; the budget splits a long file into several increments that together equal one unbounded read
- [ ] `mise run check` - must pass before task 5

### Task 5: The provider loop
- [ ] `ClaudeCodeProvider::new(cursors: BufferHandle)` implementing `Provider` with `name()` = `"claude_code"`; `run` ticks every `config.claude_code.poll_interval` seconds (`MissedTickBehavior::Delay`)
- [ ] per tick, for each configured root: a missing `projects` directory is logged once at info (not every tick) and skipped; otherwise list `<projects>/*/*.jsonl` (exactly two levels - this excludes `subagents/`), sorted by path
- [ ] per file: load its cursor (cache cursors in memory after the first read; read from the buffer only on first sight), call `read_increment` repeatedly until it returns `None`, sending each increment as `Emission::awaiting_commit(drafts, Some(cursor))` - also when `drafts` is empty but the offset moved, so a file of only tool lines is not re-read forever - and advancing the in-memory cursor only after `committed.await` succeeds (on failure: log and move on to the next file; the next tick retries from the durable cursor)
- [ ] a send failure on `out` (runtime gone) returns `Ok(())`, like `browser_history`; any IO error on one file is logged and the loop continues with the next file - the provider returns `Err` only for a buffer that refuses cursor reads
- [ ] register it in `main.rs` under `supervise` with `Backoff::default()`, abort and await it on shutdown with the others, and add `claude_code` to `PROVIDERS`
- [ ] write tests with a real temp buffer (`Buffer::open` as in `runtime::tests`) and a temp root: a first run ships every message and session record of the fixture once; a second run over an unchanged file ships nothing; appended lines ship on the next tick; a missing root does not fail the provider; a restarted provider resumes from the committed cursor without re-shipping
- [ ] `mise run check` - must pass before task 6

### Task 6: End-to-end through the stub server
- [ ] in `tests/stub_server.rs`, a test that installs `fixtures/claude_code_session.jsonl` as `<home>/.claude/projects/-Users-u-Projects-THE-FEUD-V2/<session-id>.jsonl` plus a `subagents/agent-x.jsonl` beside it, writes `[claude_code] poll_interval = 1` into the config, starts the daemon, and asserts that the stub receives exactly the expected `claude_code/message` and `claude_code/session` envelopes (provider, kind, payload fields, 16-hex `dedup_key`), none from the subagent file, and that existing windows/browser behaviour in the same run is unchanged
- [ ] make `config_text` emit the `[claude_code]` section only when a test asks for it, so every existing test keeps its current config byte-for-byte
- [ ] add a live test `the_live_transcripts_parse_without_loss` (`#[ignore]`d, reason: reads the real `~/.claude`): runs `read_increment` over every file under the real default roots and asserts it yields records, consumes each file to its end or to a partial last line, and never ships a line with `isMeta: true`; call it from `scripts/acceptance.sh` like the other live cases
- [ ] `mise run check` - must pass before task 7

### Task 7: Document the provider and mirror the contract
- [ ] `README.md` `## Configuration`: the `[claude_code]` section in the TOML example with both keys and their defaults
- [ ] `### The two providers` becomes three: what `claude_code` reads (the two-level glob, subagents excluded), what it ships and what it never ships, the per-file cursor (inode, offset, last message ts) and why a partial line waits; remove "agent session transcripts" from the deferred list in `### Adding a provider`
- [ ] `## The wire contract`: both pairs in the `(provider, kind)` table, the payload tables and captured bodies from Technical Details, the `dedup_key` constructions in `### Identity`, and the note that `role`/`message_kind`/`field` values are stored opaquely by the service - identical in content to the service README section
- [ ] `## Known limitations`: cursors of deleted transcript files stay in `buffer.db` (a few bytes each, never re-read); a session copied into a fork is shipped once per session by design
- [ ] `mise run check` - must pass before task 8

### Task 8: Verify acceptance criteria
- [ ] `mise run check` green; `! grep -rn 'unsafe' src --include='*.rs' | grep -v '^src/macos/'` empty
- [ ] `cargo test --test stub_server` green
- [ ] `./scripts/acceptance.sh` runs, including the new live transcript case, on this Mac
- [ ] no `#[allow(dead_code)]` added; every new module declared; no `_ =>` arm on `Provider`, `Kind`, `KeySource` or the tag enum

## Technical Details

### Wire contract (identical in the service repository)

`claude_code/message` - one record per text block of a conversation line. Envelope `ts` = the line's `timestamp`.

| field | type | required | meaning |
|---|---|---|---|
| `session_id` | string | yes | line `sessionId` (fallback: file stem) |
| `uuid` | string | yes | line `uuid` |
| `block` | integer | yes | index among the line's text blocks, from 0 |
| `role` | string | yes | `user` or `assistant` |
| `message_kind` | string | yes | `prompt`, `command`, `command_output`, `notification`, `compact_summary` (user lines); `text` (assistant lines) |
| `text` | string | yes | the block text exactly as in the transcript; may be `""` |
| `cwd` | string | yes | line `cwd` |
| `git_branch` | string | no | line `gitBranch`, omitted when absent or empty |
| `profile` | string | yes | profile name of the root the file was read from |

`claude_code/session` - a session metadata event. Envelope `ts` = the line's `timestamp` if it has one, else the last message `ts` read before it in the file, else the file's modification time.

| field | type | required | meaning |
|---|---|---|---|
| `session_id` | string | yes | line `sessionId` |
| `field` | string | yes | `custom_title`, `ai_title` or `pr` |
| `value` | string | yes | the title, or the PR url |

`dedup_key`: `device \x1F "claude_code" \x1F "message" \x1F session_id \x1F uuid \x1F block` and `device \x1F "claude_code" \x1F "session" \x1F session_id \x1F field \x1F value`, sha256, first 16 hex. A title repeated verbatim collapses to one row; a message copied into a forked session is stored once per session.

The service validates the required fields per pair and stores the values of `role`, `message_kind` and `field` without checking them against a set, so a new `message_kind` later does not get records rejected.

Captured bodies:

```json
{"provider":"claude_code","device":"mbp-21","ts":"2026-09-14T11:35:12.410Z","seq":90211,
 "kind":"message","dedup_key":"4c1e9a07b2d85f36","degraded":false,
 "payload":{"session_id":"8f2c61d0-4b7e-4a51-9d3e-1c0b5e7a2f94","uuid":"d41f0c2a-7e93-4b6d-a8f1-5c2e90b7d316","block":0,
            "role":"user","message_kind":"prompt","text":"передеплоишь дев через spot?",
            "cwd":"/Users/pavel.karpovich/Projects/THE_FEUD_V2","git_branch":"main","profile":"claude"}}

{"provider":"claude_code","device":"mbp-21","ts":"2026-09-14T11:35:12.410Z","seq":90212,
 "kind":"session","dedup_key":"b07d3e5a91c4f268","degraded":false,
 "payload":{"session_id":"8f2c61d0-4b7e-4a51-9d3e-1c0b5e7a2f94","field":"ai_title","value":"Redeploy dev via spot"}}
```

### Fixture `fixtures/claude_code_session.jsonl`

Hand-written, one JSON object per line, all with `sessionId` `8f2c61d0-4b7e-4a51-9d3e-1c0b5e7a2f94`, `cwd` `/Users/u/Projects/THE_FEUD_V2`, `gitBranch` `main`, increasing `timestamp`s on 2026-09-14 and distinct `uuid`s. In order:

1. `custom-title` with `customTitle` `feud` (before any message - takes the file mtime)
2. `user`, content string `передеплоишь дев через spot?` -> `prompt`
3. `assistant`, content `[thinking block, text block "Checking the template version first."]` -> one `text`
4. `assistant`, content `[tool_use block]` -> nothing
5. `user`, content `[tool_result block]` -> nothing
6. `assistant`, content `[text "Deployed.", text "URL: https://dev.example.com token=abc123"]` -> two drafts, `block` 0 and 1, text unchanged
7. `user`, `isMeta: true`, content `Base directory for this skill: ...` -> nothing
8. `user`, content `<command-message>brainstorm</command-message>\n<command-name>/brainstorm</command-name>\n<command-args>nikki sessions</command-args>` -> `command`
9. `user`, content `<local-command-stdout>Set model to Opus</local-command-stdout>` -> `command_output`
10. `user`, content `<task-notification>agent finished</task-notification>` -> `notification`
11. `user`, content `[Request interrupted by user]` -> `prompt`
12. `user`, `isCompactSummary: true`, content `This session is being continued ...` -> `compact_summary`
13. `assistant`, `isSidechain: true`, text content -> nothing
14. `ai-title` with `aiTitle` `Redeploy dev via spot` -> `session`, `ts` of line 12
15. `ai-title` repeated verbatim -> the same `dedup_key` as line 14
16. `pr-link` with `prUrl` `https://github.com/u/THE_FEUD_V2/pull/7` and its own `timestamp` -> `session`, `field: "pr"`, that `ts`
17. `attachment` line -> nothing
18. `user` line with `gitBranch` `""` -> `git_branch` omitted

## Post-Completion

**Order matters - the service goes first.** The service rejects an unknown provider per record inside a `200`, and the daemon deletes every record of a batch it got a 2xx for, so a daemon shipping `claude_code` to a service that does not know it destroys the whole transcript backfill permanently:
- confirm the service plan is deployed: one hand-made `claude_code/message` record posted to `/api/v1/records` comes back `accepted: 1`
- only then release this daemon and upgrade both Macs; the MBP ships both profiles, the Air only `~/.claude` (the missing `~/.claude-work` is skipped by default)
- after the upgrade, re-grant Accessibility if the Homebrew upgrade dropped the TCC grant, as with every release

**Manual verification**:
- watch the first backfill drain (`buffer.db` pending count falls to zero) and check the service's `cc_messages` row count is in the tens of thousands on the MBP
- the acceptance scenario once a reader can reach the sessions API: describe the week of 13-20 September 2026 from nikki alone

**Later, separate plans**: diffs (what a session changed in files) as a third `claude_code` kind; Codex sessions.
