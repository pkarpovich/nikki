# Ardia: read a second Chromium browser beside Dia

## Overview

Ardia is a Chromium browser for macOS (bundle id `com.ardia.browser`) that the user now runs beside Dia. nikki reads Dia in two places: the history of one profile (`browser_history` provider) and the focused tab (the `dia` extractor). This change adds Ardia to both, with the same mechanics, behind an optional `[ardia]` config section. Dia keeps working exactly as it does today.

Acceptance scenario: on a Mac whose config carries `[ardia] profile = "MBP_21"` and a `since` instant, the daemon ships every Ardia visit of that profile newer than `since` as a `browser_history/visit` record with `browser: "ardia"` in its payload, keeps shipping new ones on every poll, and a sample taken while Ardia is focused carries `details` with the tab's `url`, `tab`, `profile` and `pinned`. Dia visits and Dia tab details are unchanged, byte for byte.

### Facts about Ardia this plan relies on

Measured on a real installation (Ardia 0.2.44):

- **Data root:** `~/Library/Application Support/com.ardia.browser/Chromium`. Below it the layout is plain Chromium: `Local State`, and one directory per profile holding `History` and `History-journal`, with the same `visits` and `urls` schema Dia has.
- **Profile names do not live in `Local State`.** Its `profile.info_cache` still says `Person 1` / `Person 2` after the user renamed the profiles. The names the user sees live in `~/Library/Application Support/com.ardia.browser/session.json`, under `profiles`: an object keyed by profile id, each value carrying at least `id` and `name`, for example `{"profiles": {"p-abc-3": {"id": "p-abc-3", "name": "MBP_21", ...}}}`. The history directory of profile id `<id>` is `Chromium/Profile <id>`. The default Chromium profile (`Chromium/Default`) has no entry in `session.json` and is never read.
- **AppleScript:** Ardia ships `Ardia.sdef` with the same classes and properties the Dia script uses: `front window`, `active tab`, `URL`, `title`, `active profile` and its `name`, `isPinned`. The Dia script works with `tell application "Ardia"`.
- **Ardia syncs history between Macs.** Each Mac reads only its own profile, as with Dia.
- **The user's Ardia profile was seeded by importing Dia's history.** The imported visits are already in the service under Dia, so a plain "read from id 0" would ship them twice. `since` cuts them off.
- **Visit ids overlap with Dia's.** Dia's ids run to about 955 000, Ardia's to about 18 000, both from 1, and both profiles can carry the same name (`MBP_21`). The current browser dedup key (`device, "browser_history", profile, generation, visit_id`) would therefore collide and the service would drop Ardia visits as duplicates of Dia's.

### Non-goals

- No change to Dia's behaviour, config, dedup keys, cursor key or payload.
- No generic multi-browser list (`[[browsers]]`), no third browser.
- `[browser]` (Dia) stays required. Making it optional is a separate change.
- No reading of Ardia's `Default` profile, of sync journals or of any Ardia file other than `session.json`, `Local State`, `History` and `History-journal`.
- No service change. The service keeps unknown payload fields in `raw`.
- No attempt to tell which Mac an Ardia visit was made on.

### Rejected alternatives

- **Resolve Ardia profiles through `Local State`, as Dia does.** Rejected: Ardia never writes the user's names there, so the configured name would never resolve.
- **A shared key for both browsers.** Rejected: the visit ids collide (see above).
- **Adding the browser to Dia's key too.** Rejected: every Dia visit already stored would stop matching its later revisions, which are emitted under the same key.
- **Make an unresolvable Ardia profile fatal at startup, as Dia's is.** Rejected: Ardia is optional and may be uninstalled. A missing Ardia profile fails only the Ardia provider, which the supervisor restarts with backoff.

## Skills to invoke

Load each skill below with the Skill tool and follow its conventions before implementing any task in this plan.

- `rust-style` - all code under `src/` follows it.

## Code-Quality Rules (verify before marking each task complete)

From `rust-style`: `for` loops instead of iterator chains; `let ... else` for early returns; minimise `if let`, prefer `match`; shadow instead of renaming; no comments; newtypes and enums over strings and bools; no wildcard `_` arm on an enum the crate owns; no `matches!`; destructure structs explicitly.

From `CLAUDE.md`: no `unsafe` outside `src/macos/`; no blanket `#[allow(dead_code)]`; every new module declared; every subprocess call has a deadline; a provider never panics; tests inline in the file they cover, through the entry point the daemon calls.

Per-task gate, and the whole of verification inside this plan:

```
mise run check
! grep -rn 'unsafe' src --include='*.rs' | grep -v '^src/macos/'
```

Do **not** run `./scripts/acceptance.sh`, any `-- --ignored` test or the real binary, and do not add `tests/stub_server.rs` cases. They read the live machine and need grants the executor does not have. A human runs acceptance after the plan (Post-Completion).

## Context (from discovery)

- `src/config.rs` - `Config { browser: Browser, history_poll_interval, revisit_window, ... }`, `Browser { profile }`, the TOML shape `FileConfig.browser: FileBrowser` with `deny_unknown_fields`, `required(...)` for mandatory fields, `ConfigError::Invalid { field, reason }`. The `claude_code.import` work added a `since` parsed with `chrono::DateTime::parse_from_rfc3339` into a `Timestamp` - reuse that parsing. `Config` literals in tests (`src/providers/mod.rs` `test_config`, `src/config.rs` tests) need the new field.
- `src/providers/browser_history.rs`:
  - `USER_DATA_RELATIVE` ("Library/Application Support/Dia/User Data"), `user_data_dir(home)`, `SNAPSHOT_DIR` ("history-snapshot");
  - `BrowserHistoryProvider { user_data, cursors }`, its `run` resolves the profile through `directory_for(user_data, profile)` (reads `Local State`), loads `stored_state(cursors, profile)`, then polls on `history_poll_interval` with `poll_once` and sends `Emission::awaiting_commit(records, Some(Cursor { provider: BrowserHistory, key: profile, value }))`;
  - `poll_once` re-resolves the directory every poll, clones `History` and `History-journal` into the snapshot dir under a `SnapshotGuard`, then `collect_from` -> `collect(connection, profile, revisit_window, state, page)`;
  - `HistoryState { cursor, generation, shipped }`, `HistoryState::fresh(highest)` starts at the current max id;
  - `visit_record(profile, generation, row)` builds the payload (`url`, `title`, `profile`, `transition`, `visit_id`, `duration_ms`) and `KeySource::BrowserVisit { profile, generation, visit_id }`;
  - `discard_stale_snapshot(state_dir)` sweeps `SNAPSHOT_DIR` at startup.
- `src/runtime/mod.rs` - `KeySource::BrowserVisit`, mapped in `RecordDraft::into_envelope` to `browser_key(device, profile, generation, visit_id)`.
- `src/runtime/dedup.rs` - `browser_key` and its tests.
- `src/extract/mod.rs` - `DIA_BUNDLE_ID`, `enum Extractor { Dia, Agterm }`, `for_bundle_id`, `details`.
- `src/extract/dia.rs` - `SCRIPT` (AppleScript, `tell application "Dia"`), `active_tab()`, `details_from`, `parse_tab`, error reporting with a once-only Automation-denied warning.
- `src/main.rs` - resolves Dia's directory at startup (fatal on failure), spawns `BrowserHistoryProvider` under `supervise`, calls `discard_stale_snapshot` before reading config, `PROVIDERS` lists provider names for the startup log.
- `README.md` - Configuration (TOML example and field table), Permissions table (Automation -> Dia), the `browser_history` provider section, Extractors list, the wire contract `browser_history/visit` row and `### Identity` key list, `### details on a window record`.

## Solution Overview

1. **A browser is a value, not a constant.** `enum Browser { Dia, Ardia }` (in `src/providers/browser_history.rs`, or a small shared module if both the provider and the extractor need it) answers: data root under HOME, how a profile name resolves to a history directory, snapshot dir name, cursor key for a profile, whether the key and payload carry a browser name. Dia's answers are exactly today's constants.
2. **The history provider is parameterised by browser.** `BrowserHistoryProvider` gains the browser and an optional `since`. `main.rs` spawns a second instance for Ardia when `[ardia]` is present. Each instance has its own snapshot directory, so two polls never clone into the same place.
3. **Ardia profile resolution reads `session.json`.** Name -> id -> `Chromium/Profile <id>`. The same "lists the names that exist" error as Dia's.
4. **`since` bounds the first read.** With no stored state and a `since`, the provider starts from id 0 instead of the current max id, and every poll drops rows whose visit time is before `since` (they still advance the cursor and are never put in `shipped`).
5. **Ardia's identity carries the browser.** `KeySource::BrowserVisit` gains `browser: Option<String>`. `None` keys exactly as today. `Some("ardia")` keys over `device, "browser_history", "ardia", profile, generation, visit_id`. The Ardia payload gains `"browser": "ardia"`. The Ardia cursor key is `ardia:<profile>`.
6. **The extractor is the Dia script with another application name.** `Extractor::Ardia` for `com.ardia.browser` runs the same script text with `tell application "Ardia"` and the same parsing and error reporting.

## Technical Details

### Config

```toml
[ardia]
profile = "MBP_21"                 # required: the name Ardia shows, from session.json
since = "2026-11-02T05:36:00Z"     # required: RFC 3339 with offset
```

- `Config` gains `pub ardia: Option<Ardia>` with `pub struct Ardia { pub profile: String, pub since: Timestamp }`.
- TOML: `ardia: Option<FileArdia>` with `deny_unknown_fields`.
- Validation (`ConfigError::Invalid` naming the field): `ardia.profile` non-empty, `ardia.since` parses as RFC 3339 with an offset. Absent section means `None`.

### Browser

- Dia: root `Library/Application Support/Dia/User Data`, resolution through `Local State` (existing `directory_for`), snapshot dir `history-snapshot`, cursor key `<profile>`, no browser in key or payload.
- Ardia: root `Library/Application Support/com.ardia.browser/Chromium`, resolution through `../session.json` (the file sits beside `Chromium`, in `com.ardia.browser`), snapshot dir `history-snapshot-ardia`, cursor key `ardia:<profile>`, browser name `ardia` in key and payload.
- `discard_stale_snapshot` sweeps both snapshot directories.

### Ardia profile resolution

`fn ardia_directory_for(root: &Path, profile: &str) -> Result<String, String>` (root is the `Chromium` dir) reads `root.parent()/session.json`, finds the entry under `profiles` whose `name` equals `profile`, and returns `Profile <id>`. Errors name the file and list the names that exist, like `resolve_directory`. A parse helper over the file text keeps it testable without the file system. Unknown fields in `session.json` are ignored (it carries tabs, windows, settings).

### Provider behaviour for Ardia

- `run` resolves the directory first. On failure it returns `Err(ProviderError(reason))` without touching the process: the supervisor restarts it with backoff. Dia keeps its startup-fatal resolution in `main.rs`.
- Fresh state with `since: Some(_)`: start at cursor 0 and generation 1, so the first poll reads every row in pages of `PAGE_LIMIT`. The rows before `since` advance `position` but produce no record and no `shipped` entry.
- The `since` filter applies on every poll, so a re-read in the revisit window never ships an old row either.
- Everything else (clone, journal, `quick_check`, revisit window, generation on replacement, digests) is shared code and unchanged.

### Dedup and payload

- `browser_key(device, browser: Option<&str>, profile, generation, visit_id)`: with `None` the field list is exactly today's; with `Some(name)` the name is inserted after `"browser_history"`.
- `visit_record` takes the browser and adds `"browser": "ardia"` to the payload only for Ardia.

### Extractor

- `pub const ARDIA_BUNDLE_ID: &str = "com.ardia.browser"`, `Extractor::Ardia`.
- The script text is built for an application name, so Dia's script is unchanged in content (the existing `SCRIPT` test or fixture still matches). `active_tab` takes the application (an enum or `&'static str` constant per browser). Parsing and error reporting are shared. The once-only Automation-denied warning is per browser, so a denial for one does not hide a denial for the other.

## What Goes Where

- **Implementation Steps:** config, browser abstraction and Ardia resolution, provider and keys, extractor, wiring in `main.rs`, README. All in this repository.
- **Post-Completion:** acceptance run, the user's config on each Mac, the service README mirror, release.

## Implementation Steps

### Task 1: Parse and validate `[ardia]`

**Files:**
- Modify: `src/config.rs`
- Modify: `src/providers/mod.rs` (the `test_config` literal)

- [x] Add `Ardia { profile, since }`, `Config.ardia: Option<Ardia>`, and `FileArdia` with `deny_unknown_fields`.
- [x] Validate as in Technical Details, reusing the RFC 3339 parsing used by `claude_code.import.since`.
- [x] Update every `Config` literal with `ardia: None`.
- [x] Tests: a full section parses; an absent section is `None`; empty `profile`, missing `profile`, missing `since`, unparseable `since` and an unknown key are each rejected with the field named.
- [x] Run the gate.

### Task 2: Browser value and Ardia profile resolution

**Files:**
- Modify: `src/providers/browser_history.rs`
- Create: `fixtures/ardia_session.json`

- [x] Add `enum Browser { Dia, Ardia }` with the per-browser answers from Technical Details. Dia's answers reuse the existing constants and `directory_for`.
- [x] Add `ardia_directory_for` and its text-level parse helper.
- [x] Add the fixture: a minimal `session.json` with `profiles` holding two entries (`MBP_21` and `MBA_22`, ids `p-test-3` and `p-test-2c`, each with `id`, `name`, `color`, `createdAt`), plus a couple of unrelated top-level keys (`tabs: []`, `settings: {}`) to prove they are ignored.
- [x] Make `discard_stale_snapshot` sweep both snapshot directories.
- [x] Tests: `MBP_21` resolves to `Profile p-test-3`; an unknown name fails and the message lists `MBP_21, MBA_22`; invalid JSON and a missing `profiles` key fail with the file named; Dia resolution through `Local State` is unchanged (existing tests pass untouched); both stale snapshot directories are swept.
- [x] Run the gate.

### Task 3: Browser-aware keys, payload, cursor and `since` in the provider

**Files:**
- Modify: `src/providers/browser_history.rs`
- Modify: `src/runtime/mod.rs`
- Modify: `src/runtime/dedup.rs`

- [x] Add `browser: Option<String>` to `KeySource::BrowserVisit` and the browser argument to `browser_key`, as in Technical Details. Update every construction site with `None` for Dia.
- [x] Give `BrowserHistoryProvider` its `Browser` and `since: Option<Timestamp>`. Dia is constructed with `Browser::Dia` and `None`.
- [x] Use the browser's resolution, snapshot dir and cursor key in `run` and `poll_once`. Ardia's resolution failure in `run` returns `ProviderError`.
- [x] Implement the fresh-state-at-0 rule and the per-poll `since` filter in `collect`.
- [x] `visit_record` adds `browser` to the payload for Ardia only.
- [x] Tests, through `collect` / `collect_from` on the existing `history_sample.db` fixture and through `dedup`:
  - a Dia key is byte-for-byte the value the existing test pins;
  - an Ardia key differs from the Dia key for the same device, profile, generation and visit id;
  - a fresh Ardia state with `since` ships every fixture visit at or after `since` and none before, and the cursor ends at the max id;
  - a second poll on the same data ships nothing;
  - a fresh Dia state still starts at the max id (existing test unchanged);
  - the Ardia payload carries `browser: "ardia"`, the Dia payload has no `browser` key;
  - the Ardia cursor key is `ardia:MBP_21`.
- [x] Run the gate.

### Task 4: Ardia tab extractor

**Files:**
- Modify: `src/extract/mod.rs`
- Modify: `src/extract/dia.rs`

- [x] Add `ARDIA_BUNDLE_ID` and `Extractor::Ardia`, dispatched to the same tab code with Ardia's application name.
- [x] Build the script for an application name. Dia's script text is unchanged.
- [x] Make the Automation-denied warning once per browser.
- [x] Tests: `for_bundle_id("com.ardia.browser")` is `Ardia`; Ardia's script contains `tell application "Ardia"` and `running of application "Ardia"` and no `"Dia"`; Dia's script is unchanged; the existing captured-output parse test passes for both.
- [x] Run the gate.

### Task 5: Wire the Ardia provider in `main.rs`

**Files:**
- Modify: `src/main.rs`

- [x] When `config.ardia` is `Some`, spawn a second `supervise(BrowserHistoryProvider::new(Browser::Ardia, ardia_root, records.clone(), Some(since)), ...)` beside the Dia one, abort and await it on shutdown like the others. When `None`, spawn nothing.
- [x] Log the Ardia profile on the startup line (or one `info` line when present). Keep `PROVIDERS` accurate.
- [x] Do not resolve Ardia at startup. Dia's startup resolution is unchanged.
- [x] The existing tests and the gate pass.

### Task 6: Verify acceptance criteria

- [ ] Every Overview requirement against the acceptance scenario: Ardia visits after `since` with `browser: "ardia"`, Ardia tab details, Dia unchanged.
- [ ] All pre-existing tests pass with edits limited to new parameters.
- [ ] Run the gate. Nothing else.

### Task 7: [Final] Update documentation

- [ ] `README.md`:
  - Configuration: the `[ardia]` example and its two field-table rows.
  - Permissions: Automation -> Ardia beside Dia, one-time prompt on first use.
  - The `browser_history` provider section: Ardia as a second instance, profile names from `session.json` and why not `Local State`, `since` and why (the seeded import), its own snapshot directory, failure restarts only the Ardia provider.
  - Extractors: Ardia beside Dia.
  - Wire contract: `browser_history/visit` optional `browser` field (`ardia` today, absent for Dia), and in `### Identity` the Ardia key line `device \x1F "browser_history" \x1F "ardia" \x1F profile \x1F generation \x1F visit_id`, with the reason (ids collide with Dia's).
  - `details` on a window record: `com.ardia.browser` carries the same fields as Dia.
  - Status: the browsers it reads are Dia and Ardia.
- [ ] Move this plan to `docs/plans/completed/`.

## Post-Completion

*Items requiring manual intervention or external systems - no checkboxes, informational only.*

- Run `./scripts/acceptance.sh` once from a terminal holding the Accessibility grant.
- Mirror the wire-contract change (optional `browser` on `browser_history/visit`, the Ardia key line) into the service README in the same pass. The service code needs no change.
- Release, then on each Mac add `[ardia]` with that Mac's profile name and a `since` at the end of the imported history, run `nikki --check-config`, and upgrade. Allow the Automation prompt for Ardia when it appears.
- Check that Ardia visits arrive with `browser: "ardia"` and that the first poll shipped the history since `since`.
