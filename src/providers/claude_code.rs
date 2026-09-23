use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::mpsc::Sender;
use tokio::time::{MissedTickBehavior, interval};

use super::{Ctx, Emission, Provider, ProviderError};
use crate::config::ClaudeRoot;
use crate::runtime::buffer::BufferHandle;
use crate::runtime::{self, Cursor, KeySource, Kind, RecordDraft, Timestamp};

pub const READ_BUDGET: ReadBudget = ReadBudget {
    max_records: 500,
    max_bytes: 4 * 1024 * 1024,
};

const TRANSCRIPT_EXTENSION: &str = "jsonl";

pub struct ClaudeCodeProvider {
    cursors: BufferHandle,
    known: HashMap<PathBuf, Option<FileCursor>>,
    unreadable: HashSet<PathBuf>,
}

enum Flow {
    Continue,
    RuntimeGone,
}

impl ClaudeCodeProvider {
    pub fn new(cursors: BufferHandle) -> ClaudeCodeProvider {
        ClaudeCodeProvider {
            cursors,
            known: HashMap::new(),
            unreadable: HashSet::new(),
        }
    }

    async fn tick(
        &mut self,
        roots: &[ClaudeRoot],
        out: &Sender<Emission>,
    ) -> Result<Flow, ProviderError> {
        for ClaudeRoot { profile, projects } in roots {
            let Some(paths) = self.transcripts_in(projects) else {
                continue;
            };
            for path in paths {
                match self.ship_file(&path, profile, out).await? {
                    Flow::Continue => {}
                    Flow::RuntimeGone => return Ok(Flow::RuntimeGone),
                }
            }
        }
        Ok(Flow::Continue)
    }

    fn transcripts_in(&mut self, projects: &Path) -> Option<Vec<PathBuf>> {
        match transcripts(projects, &mut self.unreadable) {
            Ok(paths) => {
                self.unreadable.remove(projects);
                Some(paths)
            }
            Err(error) => {
                if self.unreadable.insert(projects.to_path_buf()) {
                    tracing::info!(
                        path = %projects.display(),
                        %error,
                        "the Claude Code projects directory cannot be read and is skipped until it can"
                    );
                }
                None
            }
        }
    }

    async fn ship_file(
        &mut self,
        path: &Path,
        profile: &str,
        out: &Sender<Emission>,
    ) -> Result<Flow, ProviderError> {
        let mut cursor = self.cursor_for(path).await?;
        loop {
            let increment = match read_increment(path, profile, cursor, READ_BUDGET) {
                Ok(increment) => {
                    self.unreadable.remove(path);
                    increment
                }
                Err(error) => {
                    if self.unreadable.insert(path.to_path_buf()) {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "a transcript cannot be read and is skipped until it can"
                        );
                    }
                    return Ok(Flow::Continue);
                }
            };
            let Some(Increment {
                drafts,
                cursor: advanced,
            }) = increment
            else {
                return Ok(Flow::Continue);
            };

            let stored = Cursor {
                provider: runtime::Provider::ClaudeCode,
                key: cursor_key(path),
                value: advanced.encode(),
            };
            let (emission, committed) = Emission::awaiting_buffer(drafts, Some(stored));
            if out.send(emission).await.is_err() {
                return Ok(Flow::RuntimeGone);
            }
            if committed.await.is_err() {
                tracing::error!(
                    path = %path.display(),
                    "the transcript records were not buffered, so the file is read again from its stored cursor"
                );
                return Ok(Flow::Continue);
            }
            self.known.insert(path.to_path_buf(), Some(advanced));
            cursor = Some(advanced);
        }
    }

    async fn cursor_for(&mut self, path: &Path) -> Result<Option<FileCursor>, ProviderError> {
        if let Some(cursor) = self.known.get(path) {
            return Ok(*cursor);
        }
        let stored = self
            .cursors
            .cursor(runtime::Provider::ClaudeCode, cursor_key(path))
            .await;
        let stored = match stored {
            Ok(stored) => stored,
            Err(error) => {
                return Err(ProviderError(format!(
                    "the buffer refused a cursor: {error}"
                )));
            }
        };
        let cursor = match stored {
            Some(value) => FileCursor::decode(path, &value),
            None => None,
        };
        self.known.insert(path.to_path_buf(), cursor);
        Ok(cursor)
    }
}

impl Provider for ClaudeCodeProvider {
    fn name(&self) -> &'static str {
        runtime::Provider::ClaudeCode.as_str()
    }

    async fn run(&mut self, ctx: Ctx, out: Sender<Emission>) -> Result<(), ProviderError> {
        let roots = ctx.config.claude_code.roots.clone();
        let mut ticker = interval(Duration::from_secs(ctx.config.claude_code.poll_interval));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match self.tick(&roots, &out).await? {
                Flow::Continue => {}
                Flow::RuntimeGone => {
                    tracing::info!("the runtime is gone, so the Claude Code provider stops");
                    return Ok(());
                }
            }
        }
    }
}

fn transcripts(projects: &Path, unreadable: &mut HashSet<PathBuf>) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for project in fs::read_dir(projects)? {
        let Ok(project) = project else {
            continue;
        };
        let project = project.path();
        if !project.is_dir() {
            continue;
        }
        let entries = match fs::read_dir(&project) {
            Ok(entries) => {
                unreadable.remove(&project);
                entries
            }
            Err(error) => {
                if unreadable.insert(project.clone()) {
                    tracing::warn!(
                        path = %project.display(),
                        %error,
                        "a Claude Code project directory cannot be read and is skipped until it can"
                    );
                }
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            if path.extension() != Some(OsStr::new(TRANSCRIPT_EXTENSION)) {
                continue;
            }
            if !path.is_file() {
                continue;
            }
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn cursor_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCursor {
    pub inode: u64,
    pub offset: u64,
    pub last_message_ts: Option<i64>,
}

impl FileCursor {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn decode(path: &Path, value: &str) -> Option<FileCursor> {
        match serde_json::from_str(value) {
            Ok(cursor) => Some(cursor),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "the stored transcript cursor does not parse and the file is read from the start"
                );
                None
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReadBudget {
    pub max_records: usize,
    pub max_bytes: usize,
}

#[derive(Debug)]
pub struct Increment {
    pub drafts: Vec<RecordDraft>,
    pub cursor: FileCursor,
}

pub struct LineContext<'a> {
    pub profile: &'a str,
    pub fallback_session_id: &'a str,
    pub last_message_ts: Option<Timestamp>,
    pub file_modified: Timestamp,
}

#[derive(Debug, Default)]
pub struct LineOutcome {
    pub drafts: Vec<RecordDraft>,
    pub message_ts: Option<Timestamp>,
}

#[derive(Clone, Copy)]
enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Copy)]
enum MessageKind {
    Prompt,
    Command,
    CommandOutput,
    Notification,
    CompactSummary,
    Text,
}

impl MessageKind {
    fn as_str(self) -> &'static str {
        match self {
            MessageKind::Prompt => "prompt",
            MessageKind::Command => "command",
            MessageKind::CommandOutput => "command_output",
            MessageKind::Notification => "notification",
            MessageKind::CompactSummary => "compact_summary",
            MessageKind::Text => "text",
        }
    }
}

#[derive(Clone, Copy)]
enum LeadingTag {
    CommandName,
    CommandMessage,
    BashInput,
    LocalCommandStdout,
    LocalCommandStderr,
    BashStdout,
    BashStderr,
    TaskNotification,
}

const LEADING_TAGS: [(&str, LeadingTag); 8] = [
    ("<command-name>", LeadingTag::CommandName),
    ("<command-message>", LeadingTag::CommandMessage),
    ("<bash-input>", LeadingTag::BashInput),
    ("<local-command-stdout>", LeadingTag::LocalCommandStdout),
    ("<local-command-stderr>", LeadingTag::LocalCommandStderr),
    ("<bash-stdout>", LeadingTag::BashStdout),
    ("<bash-stderr>", LeadingTag::BashStderr),
    ("<task-notification>", LeadingTag::TaskNotification),
];

impl LeadingTag {
    fn of(text: &str) -> Option<LeadingTag> {
        let text = text.trim_start();
        for (prefix, tag) in LEADING_TAGS {
            if text.starts_with(prefix) {
                return Some(tag);
            }
        }
        None
    }

    fn message_kind(self) -> MessageKind {
        match self {
            LeadingTag::CommandName => MessageKind::Command,
            LeadingTag::CommandMessage => MessageKind::Command,
            LeadingTag::BashInput => MessageKind::Command,
            LeadingTag::LocalCommandStdout => MessageKind::CommandOutput,
            LeadingTag::LocalCommandStderr => MessageKind::CommandOutput,
            LeadingTag::BashStdout => MessageKind::CommandOutput,
            LeadingTag::BashStderr => MessageKind::CommandOutput,
            LeadingTag::TaskNotification => MessageKind::Notification,
        }
    }
}

pub fn read_increment(
    path: &Path,
    profile: &str,
    cursor: Option<FileCursor>,
    budget: ReadBudget,
) -> io::Result<Option<Increment>> {
    let metadata = fs::metadata(path)?;
    let inode = metadata.ino();
    let length = metadata.len();
    let start = match cursor {
        Some(cursor) if cursor.inode == inode && cursor.offset <= length => cursor,
        Some(_) | None => FileCursor {
            inode,
            offset: 0,
            last_message_ts: None,
        },
    };
    if length == start.offset {
        return Ok(None);
    }

    let file_modified = modified_millis(metadata.modified());
    let fallback_session_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start.offset))?;
    let mut reader = BufReader::new(file);

    let mut offset = start.offset;
    let mut last_message_ts = start.last_message_ts;
    let mut drafts = Vec::new();
    let mut text_bytes = 0;
    let mut line = Vec::new();
    while drafts.len() < budget.max_records && text_bytes < budget.max_bytes {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            break;
        }
        let line_start = offset;
        offset += read as u64;

        let value: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    offset = line_start,
                    %error,
                    "a transcript line is not JSON and is skipped"
                );
                continue;
            }
        };
        let context = LineContext {
            profile,
            fallback_session_id,
            last_message_ts: last_message_ts.map(Timestamp::from_millis),
            file_modified,
        };
        let LineOutcome {
            drafts: line_drafts,
            message_ts,
        } = drafts_from_line(&value, &context);
        if let Some(ts) = message_ts {
            last_message_ts = Some(ts.millis());
        }
        for draft in line_drafts {
            text_bytes += draft
                .payload
                .get("text")
                .and_then(Value::as_str)
                .map_or(0, str::len);
            drafts.push(draft);
        }
    }

    if offset == start.offset {
        return Ok(None);
    }
    Ok(Some(Increment {
        drafts,
        cursor: FileCursor {
            inode,
            offset,
            last_message_ts,
        },
    }))
}

fn modified_millis(modified: io::Result<SystemTime>) -> Timestamp {
    let Ok(modified) = modified else {
        return Timestamp::now();
    };
    let Ok(since_epoch) = modified.duration_since(UNIX_EPOCH) else {
        return Timestamp::now();
    };
    Timestamp::from_millis(since_epoch.as_millis() as i64)
}

pub fn drafts_from_line(line: &Value, context: &LineContext) -> LineOutcome {
    let Some(line_type) = line.get("type").and_then(Value::as_str) else {
        return LineOutcome::default();
    };
    match line_type {
        "user" => message_drafts(line, Role::User, context),
        "assistant" => message_drafts(line, Role::Assistant, context),
        "custom-title" => session_drafts(line, "custom_title", "customTitle", context),
        "ai-title" => session_drafts(line, "ai_title", "aiTitle", context),
        "pr-link" => session_drafts(line, "pr", "prUrl", context),
        _ => LineOutcome::default(),
    }
}

fn message_drafts(line: &Value, role: Role, context: &LineContext) -> LineOutcome {
    if flag(line, "isSidechain") {
        return LineOutcome::default();
    }
    let is_compact_summary = match role {
        Role::User if flag(line, "isMeta") => return LineOutcome::default(),
        Role::User => flag(line, "isCompactSummary"),
        Role::Assistant => false,
    };
    let Some(uuid) = line.get("uuid").and_then(Value::as_str) else {
        return LineOutcome::default();
    };
    let Some(ts) = line_timestamp(line) else {
        return LineOutcome::default();
    };
    let texts = text_blocks(line.pointer("/message/content"));
    if texts.is_empty() {
        return LineOutcome::default();
    }

    let session_id = session_id(line, context);
    let cwd = line.get("cwd").and_then(Value::as_str).unwrap_or_default();
    let git_branch = line.get("gitBranch").and_then(Value::as_str);

    let mut drafts = Vec::new();
    for (block, text) in (0u32..).zip(texts) {
        let message_kind = match role {
            Role::Assistant => MessageKind::Text,
            Role::User if is_compact_summary => MessageKind::CompactSummary,
            Role::User => match LeadingTag::of(text) {
                Some(tag) => tag.message_kind(),
                None => MessageKind::Prompt,
            },
        };

        let mut payload = Map::new();
        payload.insert("session_id".to_string(), Value::from(session_id));
        payload.insert("uuid".to_string(), Value::from(uuid));
        payload.insert("block".to_string(), Value::from(block));
        payload.insert("role".to_string(), Value::from(role.as_str()));
        payload.insert(
            "message_kind".to_string(),
            Value::from(message_kind.as_str()),
        );
        payload.insert("text".to_string(), Value::from(text));
        payload.insert("cwd".to_string(), Value::from(cwd));
        if let Some(git_branch) = git_branch
            && !git_branch.is_empty()
        {
            payload.insert("git_branch".to_string(), Value::from(git_branch));
        }
        payload.insert("profile".to_string(), Value::from(context.profile));

        drafts.push(RecordDraft {
            provider: runtime::Provider::ClaudeCode,
            kind: Kind::Message,
            ts,
            degraded: false,
            payload: Value::Object(payload),
            key: KeySource::ClaudeMessage {
                session_id: session_id.to_string(),
                uuid: uuid.to_string(),
                block,
            },
        });
    }

    LineOutcome {
        drafts,
        message_ts: Some(ts),
    }
}

fn session_drafts(
    line: &Value,
    field: &str,
    value_key: &str,
    context: &LineContext,
) -> LineOutcome {
    let Some(value) = line.get(value_key).and_then(Value::as_str) else {
        return LineOutcome::default();
    };
    if value.is_empty() {
        return LineOutcome::default();
    }

    let ts = match (line_timestamp(line), context.last_message_ts) {
        (Some(ts), _) => ts,
        (None, Some(ts)) => ts,
        (None, None) => context.file_modified,
    };
    let session_id = session_id(line, context);

    let mut payload = Map::new();
    payload.insert("session_id".to_string(), Value::from(session_id));
    payload.insert("field".to_string(), Value::from(field));
    payload.insert("value".to_string(), Value::from(value));

    LineOutcome {
        drafts: vec![RecordDraft {
            provider: runtime::Provider::ClaudeCode,
            kind: Kind::Session,
            ts,
            degraded: false,
            payload: Value::Object(payload),
            key: KeySource::ClaudeSession {
                session_id: session_id.to_string(),
                field: field.to_string(),
                value: value.to_string(),
            },
        }],
        message_ts: None,
    }
}

fn text_blocks(content: Option<&Value>) -> Vec<&str> {
    let mut texts = Vec::new();
    match content {
        Some(Value::String(text)) => texts.push(text.as_str()),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                let Some(text) = block.get("text").and_then(Value::as_str) else {
                    continue;
                };
                texts.push(text);
            }
        }
        Some(Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_)) | None => {}
    }
    texts
}

fn session_id<'a>(line: &'a Value, context: &LineContext<'a>) -> &'a str {
    let Some(session_id) = line.get("sessionId").and_then(Value::as_str) else {
        return context.fallback_session_id;
    };
    if session_id.is_empty() {
        return context.fallback_session_id;
    }
    session_id
}

fn line_timestamp(line: &Value) -> Option<Timestamp> {
    let timestamp = line.get("timestamp").and_then(Value::as_str)?;
    let moment = DateTime::parse_from_rfc3339(timestamp).ok()?;
    Some(Timestamp::from_millis(moment.timestamp_millis()))
}

fn flag(line: &Value, key: &str) -> bool {
    line.get(key).and_then(Value::as_bool).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use tokio::sync::mpsc::{Receiver, channel};

    use crate::config::{ClaudeCode, default_claude_code, default_redact};
    use crate::providers::tests::test_config;
    use crate::runtime::buffer::{Buffer, BufferConfig};

    const FIXTURE: &str = include_str!("../../fixtures/claude_code_session.jsonl");
    const SESSION: &str = "8f2c61d0-4b7e-4a51-9d3e-1c0b5e7a2f94";
    const CWD: &str = "/Users/u/Projects/THE_FEUD_V2";
    const PROFILE: &str = "claude";
    const FILE_MODIFIED: i64 = 1_789_000_000_000;

    fn fixture_lines() -> Vec<Value> {
        let mut lines = Vec::new();
        for line in FIXTURE.lines() {
            lines.push(serde_json::from_str(line).expect("a fixture line is JSON"));
        }
        lines
    }

    fn context(last_message_ts: Option<Timestamp>) -> LineContext<'static> {
        LineContext {
            profile: PROFILE,
            fallback_session_id: "file-stem",
            last_message_ts,
            file_modified: Timestamp::from_millis(FILE_MODIFIED),
        }
    }

    fn outcomes() -> Vec<LineOutcome> {
        let mut last_message_ts = None;
        let mut outcomes = Vec::new();
        for line in fixture_lines() {
            let outcome = drafts_from_line(&line, &context(last_message_ts));
            if let Some(ts) = outcome.message_ts {
                last_message_ts = Some(ts);
            }
            outcomes.push(outcome);
        }
        outcomes
    }

    fn ts(second: u32) -> Timestamp {
        let text = format!("2026-09-14T11:35:{second:02}.000Z");
        let moment = DateTime::parse_from_rfc3339(&text).expect("a fixture timestamp");
        Timestamp::from_millis(moment.timestamp_millis())
    }

    fn uuid(line: u32) -> String {
        format!("00000000-0000-4000-8000-{line:012}")
    }

    fn message(line: u32, block: u32, role: &str, message_kind: &str, text: &str) -> RecordDraft {
        RecordDraft {
            provider: runtime::Provider::ClaudeCode,
            kind: Kind::Message,
            ts: ts(line),
            degraded: false,
            payload: json!({
                "session_id": SESSION,
                "uuid": uuid(line),
                "block": block,
                "role": role,
                "message_kind": message_kind,
                "text": text,
                "cwd": CWD,
                "git_branch": "main",
                "profile": PROFILE,
            }),
            key: KeySource::ClaudeMessage {
                session_id: SESSION.to_string(),
                uuid: uuid(line),
                block,
            },
        }
    }

    fn session(ts: Timestamp, field: &str, value: &str) -> RecordDraft {
        RecordDraft {
            provider: runtime::Provider::ClaudeCode,
            kind: Kind::Session,
            ts,
            degraded: false,
            payload: json!({"session_id": SESSION, "field": field, "value": value}),
            key: KeySource::ClaudeSession {
                session_id: SESSION.to_string(),
                field: field.to_string(),
                value: value.to_string(),
            },
        }
    }

    fn drafts_of(outcomes: &[LineOutcome], line: usize) -> Vec<RecordDraft> {
        outcomes[line - 1].drafts.clone()
    }

    #[test]
    fn the_fixture_yields_the_exact_sequence_of_drafts() {
        let mut last_branchless = message(18, 0, "user", "prompt", "what changed on the branch?");
        let Value::Object(payload) = &mut last_branchless.payload else {
            panic!("a payload is an object");
        };
        payload.remove("git_branch");

        let expected = vec![
            session(
                Timestamp::from_millis(FILE_MODIFIED),
                "custom_title",
                "feud",
            ),
            message(2, 0, "user", "prompt", "передеплоишь дев через spot?"),
            message(
                3,
                0,
                "assistant",
                "text",
                "Checking the template version first.",
            ),
            message(6, 0, "assistant", "text", "Deployed."),
            message(
                6,
                1,
                "assistant",
                "text",
                "URL: https://dev.example.com token=abc123",
            ),
            message(
                8,
                0,
                "user",
                "command",
                "<command-message>brainstorm</command-message>\n<command-name>/brainstorm</command-name>\n<command-args>nikki sessions</command-args>",
            ),
            message(
                9,
                0,
                "user",
                "command_output",
                "<local-command-stdout>Set model to Opus</local-command-stdout>",
            ),
            message(
                10,
                0,
                "user",
                "notification",
                "<task-notification>agent finished</task-notification>",
            ),
            message(11, 0, "user", "prompt", "[Request interrupted by user]"),
            message(
                12,
                0,
                "user",
                "compact_summary",
                "This session is being continued from a previous conversation that ran out of context.",
            ),
            session(ts(12), "ai_title", "Redeploy dev via spot"),
            session(ts(12), "ai_title", "Redeploy dev via spot"),
            session(ts(16), "pr", "https://github.com/u/THE_FEUD_V2/pull/7"),
            last_branchless,
        ];

        let mut drafts = Vec::new();
        for outcome in outcomes() {
            drafts.extend(outcome.drafts);
        }
        assert_eq!(drafts, expected);
    }

    #[test]
    fn a_multi_block_line_yields_one_draft_per_text_block_under_one_uuid() {
        let drafts = drafts_of(&outcomes(), 6);
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].payload["block"], json!(0));
        assert_eq!(drafts[1].payload["block"], json!(1));
        assert_eq!(drafts[0].payload["uuid"], drafts[1].payload["uuid"]);
        assert_ne!(drafts[0].key, drafts[1].key);
    }

    #[test]
    fn tool_thinking_meta_sidechain_and_attachment_lines_yield_nothing() {
        let outcomes = outcomes();
        for line in [4, 5, 7, 13, 17] {
            let LineOutcome { drafts, message_ts } = &outcomes[line - 1];
            assert!(drafts.is_empty(), "line {line} yielded {drafts:?}");
            assert_eq!(*message_ts, None, "line {line} reported a message ts");
        }
        let thinking_then_text = drafts_of(&outcomes, 3);
        assert_eq!(thinking_then_text.len(), 1);
        assert_eq!(thinking_then_text[0].payload["block"], json!(0));
    }

    #[test]
    fn every_leading_tag_picks_its_message_kind() {
        let cases = [
            ("<command-name>/model</command-name>", "command"),
            ("<command-message>brainstorm</command-message>", "command"),
            ("<bash-input>ls</bash-input>", "command"),
            (
                "<local-command-stdout>ok</local-command-stdout>",
                "command_output",
            ),
            (
                "<local-command-stderr>no</local-command-stderr>",
                "command_output",
            ),
            ("<bash-stdout>a</bash-stdout>", "command_output"),
            ("<bash-stderr>b</bash-stderr>", "command_output"),
            (
                "<task-notification>done</task-notification>",
                "notification",
            ),
            ("  \n<bash-input>pwd</bash-input>", "command"),
            ("[Image #1] look at this", "prompt"),
            ("why <bash-input> here", "prompt"),
            ("", "prompt"),
        ];
        for (text, expected) in cases {
            let line = json!({
                "type": "user",
                "uuid": "u-1",
                "timestamp": "2026-09-14T11:35:00.000Z",
                "sessionId": SESSION,
                "cwd": CWD,
                "message": {"role": "user", "content": text},
            });
            let LineOutcome { drafts, .. } = drafts_from_line(&line, &context(None));
            assert_eq!(drafts.len(), 1, "{text:?}");
            assert_eq!(
                drafts[0].payload["message_kind"],
                json!(expected),
                "{text:?}"
            );
            assert_eq!(drafts[0].payload["text"], json!(text), "{text:?}");
        }
    }

    #[test]
    fn a_compact_summary_wins_over_a_leading_tag_and_assistant_text_is_always_text() {
        let summary = json!({
            "type": "user",
            "isCompactSummary": true,
            "uuid": "u-1",
            "timestamp": "2026-09-14T11:35:00.000Z",
            "message": {"content": "<command-name>x</command-name>"},
        });
        let reply = json!({
            "type": "assistant",
            "uuid": "u-2",
            "timestamp": "2026-09-14T11:35:00.000Z",
            "message": {"content": [{"type": "text", "text": "<bash-input>ls</bash-input>"}]},
        });
        let LineOutcome { drafts, .. } = drafts_from_line(&summary, &context(None));
        assert_eq!(drafts[0].payload["message_kind"], json!("compact_summary"));
        let LineOutcome { drafts, .. } = drafts_from_line(&reply, &context(None));
        assert_eq!(drafts[0].payload["message_kind"], json!("text"));
        assert_eq!(drafts[0].payload["session_id"], json!("file-stem"));
    }

    #[test]
    fn titles_take_the_previous_message_ts_and_a_title_before_any_message_takes_the_mtime() {
        let outcomes = outcomes();
        assert_eq!(
            drafts_of(&outcomes, 1)[0].ts,
            Timestamp::from_millis(FILE_MODIFIED)
        );
        assert_eq!(drafts_of(&outcomes, 14)[0].ts, ts(12));
        assert_eq!(drafts_of(&outcomes, 15)[0].ts, ts(12));
        assert_eq!(drafts_of(&outcomes, 16)[0].ts, ts(16));
        assert_eq!(outcomes[11].message_ts, Some(ts(12)));
    }

    #[test]
    fn a_repeated_title_carries_the_same_key() {
        let outcomes = outcomes();
        assert_eq!(drafts_of(&outcomes, 14), drafts_of(&outcomes, 15));
    }

    #[test]
    fn a_line_without_uuid_timestamp_or_text_is_skipped() {
        let cases = [
            json!({"type": "user", "timestamp": "2026-09-14T11:35:00.000Z", "message": {"content": "hi"}}),
            json!({"type": "user", "uuid": "u-1", "message": {"content": "hi"}}),
            json!({"type": "user", "uuid": "u-1", "timestamp": "yesterday", "message": {"content": "hi"}}),
            json!({"type": "user", "uuid": "u-1", "timestamp": "2026-09-14T11:35:00.000Z", "message": {"content": []}}),
            json!({"type": "ai-title", "aiTitle": "", "sessionId": SESSION}),
            json!({"type": "custom-title", "sessionId": SESSION}),
            json!({"type": "system", "uuid": "u-1", "timestamp": "2026-09-14T11:35:00.000Z"}),
            json!({"uuid": "u-1"}),
        ];
        for line in cases {
            let LineOutcome { drafts, .. } = drafts_from_line(&line, &context(None));
            assert!(drafts.is_empty(), "{line} yielded {drafts:?}");
        }
    }

    #[test]
    fn no_payload_carries_a_key_the_redactor_rewrites() {
        for outcome in outcomes() {
            for draft in outcome.drafts {
                let Value::Object(payload) = &draft.payload else {
                    panic!("a payload is an object");
                };
                for key in ["url", "title", "details", "visible"] {
                    assert!(!payload.contains_key(key), "{key} in {payload:?}");
                }
            }
        }
    }

    #[test]
    fn a_reply_with_a_url_and_a_token_ships_unchanged() {
        let drafts = drafts_of(&outcomes(), 6);
        assert_eq!(
            drafts[1].payload["text"],
            json!("URL: https://dev.example.com token=abc123")
        );
    }

    #[test]
    fn an_empty_git_branch_is_omitted() {
        let drafts = drafts_of(&outcomes(), 18);
        let Value::Object(payload) = &drafts[0].payload else {
            panic!("a payload is an object");
        };
        assert!(!payload.contains_key("git_branch"));
        assert_eq!(payload["cwd"], json!(CWD));
    }

    const UNBOUNDED: ReadBudget = ReadBudget {
        max_records: usize::MAX,
        max_bytes: usize::MAX,
    };

    struct TempTranscripts {
        path: PathBuf,
    }

    impl TempTranscripts {
        fn new(name: &str) -> TempTranscripts {
            let path = std::env::temp_dir()
                .join(format!("nikki-claude-test-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            let transcripts = TempTranscripts { path };
            fs::create_dir_all(transcripts.project())
                .expect("the temporary directory could not be created");
            transcripts
        }

        fn root(&self) -> ClaudeRoot {
            ClaudeRoot {
                profile: PROFILE.to_string(),
                projects: self.path.join(".claude").join("projects"),
            }
        }

        fn work_root(&self) -> ClaudeRoot {
            ClaudeRoot {
                profile: "claude-work".to_string(),
                projects: self.path.join(".claude-work").join("projects"),
            }
        }

        fn project(&self) -> PathBuf {
            let ClaudeRoot { projects, .. } = self.root();
            projects.join("-Users-u-Projects-THE-FEUD-V2")
        }

        fn session(&self) -> PathBuf {
            self.project().join(format!("{SESSION}.jsonl"))
        }

        fn write_subagent(&self, text: &str) {
            let subagents = self.project().join(SESSION).join("subagents");
            fs::create_dir_all(&subagents).expect("the subagents directory could not be created");
            fs::write(subagents.join("agent-x.jsonl"), text)
                .expect("the subagent transcript could not be written");
        }

        fn buffer(&self) -> Buffer {
            Buffer::open(BufferConfig {
                state_dir: self.path.join("state"),
                device: "mbp-21".to_string(),
                max_rows: 1_000_000,
                max_bytes: 1_000_000_000,
                redact: default_redact(),
            })
            .expect("the buffer could not be opened")
        }

        fn write(&self, text: &str) -> PathBuf {
            let path = self.session();
            fs::write(&path, text).expect("the transcript could not be written");
            path
        }

        fn append(&self, text: &str) {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(self.session())
                .expect("the transcript could not be opened");
            file.write_all(text.as_bytes())
                .expect("the transcript could not be appended to");
        }

        fn replace(&self, text: &str) {
            let staged = self.path.join("staged.jsonl.tmp");
            fs::write(&staged, text).expect("the replacement could not be written");
            fs::rename(&staged, self.session()).expect("the replacement could not be moved");
        }
    }

    impl Drop for TempTranscripts {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn fixture_prefix(lines: usize) -> String {
        let mut text = String::new();
        for line in FIXTURE.lines().take(lines) {
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    fn read_all(path: &Path, cursor: Option<FileCursor>, budget: ReadBudget) -> Vec<Increment> {
        let mut cursor = cursor;
        let mut increments = Vec::new();
        while let Some(increment) =
            read_increment(path, PROFILE, cursor, budget).expect("the transcript reads")
        {
            cursor = Some(increment.cursor);
            increments.push(increment);
        }
        increments
    }

    fn drafts_in(increments: &[Increment]) -> Vec<RecordDraft> {
        let mut drafts = Vec::new();
        for increment in increments {
            drafts.extend(increment.drafts.iter().cloned());
        }
        drafts
    }

    fn fixture_messages(lines: std::ops::Range<usize>) -> Vec<RecordDraft> {
        let mut drafts = Vec::new();
        for outcome in &outcomes()[lines] {
            for draft in &outcome.drafts {
                if draft.kind == Kind::Message {
                    drafts.push(draft.clone());
                }
            }
        }
        drafts
    }

    fn messages(drafts: Vec<RecordDraft>) -> Vec<RecordDraft> {
        drafts
            .into_iter()
            .filter(|draft| draft.kind == Kind::Message)
            .collect()
    }

    fn inode(path: &Path) -> u64 {
        fs::metadata(path)
            .expect("the transcript has metadata")
            .ino()
    }

    #[test]
    fn a_file_cursor_survives_a_round_trip_and_garbage_restarts() {
        let cursor = FileCursor {
            inode: 42,
            offset: 9_000,
            last_message_ts: Some(1_789_000_000_000),
        };
        let path = Path::new("/tmp/session.jsonl");
        assert_eq!(FileCursor::decode(path, &cursor.encode()), Some(cursor));
        assert_eq!(FileCursor::decode(path, "not a cursor"), None);
        assert_eq!(FileCursor::decode(path, "{\"offset\": 3}"), None);
    }

    #[test]
    fn a_fresh_file_reads_fully_in_one_increment() {
        let transcripts = TempTranscripts::new("fresh");
        let path = transcripts.write(FIXTURE);

        let increments = read_all(&path, None, UNBOUNDED);

        assert_eq!(increments.len(), 1);
        let Increment { drafts, cursor } = &increments[0];
        assert_eq!(drafts.len(), 14);
        assert_eq!(
            messages(drafts.clone()),
            fixture_messages(0..18),
            "the messages equal the line-by-line classification"
        );
        assert_eq!(cursor.inode, inode(&path));
        assert_eq!(cursor.offset, FIXTURE.len() as u64);
        assert_eq!(cursor.last_message_ts, Some(ts(18).millis()));
        assert!(
            drafts
                .iter()
                .all(|draft| draft.payload["session_id"] == json!(SESSION))
        );
    }

    #[test]
    fn an_unchanged_file_yields_nothing() {
        let transcripts = TempTranscripts::new("unchanged");
        let path = transcripts.write(FIXTURE);
        let first = read_all(&path, None, UNBOUNDED);

        let again = read_increment(&path, PROFILE, Some(first[0].cursor), UNBOUNDED)
            .expect("the transcript reads");

        assert!(again.is_none());
    }

    #[test]
    fn appended_lines_are_the_only_ones_read_again() {
        let transcripts = TempTranscripts::new("appended");
        let path = transcripts.write(&fixture_prefix(6));
        let first = read_all(&path, None, UNBOUNDED);
        assert_eq!(messages(drafts_in(&first)), fixture_messages(0..6));

        transcripts.append(&FIXTURE[fixture_prefix(6).len()..]);
        let second = read_all(&path, Some(first[0].cursor), UNBOUNDED);

        assert_eq!(messages(drafts_in(&second)), fixture_messages(6..18));
        let titles: Vec<RecordDraft> = drafts_in(&second)
            .into_iter()
            .filter(|draft| draft.kind == Kind::Session)
            .collect();
        assert_eq!(titles[0].ts, ts(12));
        assert_eq!(second[0].cursor.offset, FIXTURE.len() as u64);
    }

    #[test]
    fn a_title_after_a_split_takes_the_message_ts_carried_in_the_cursor() {
        let transcripts = TempTranscripts::new("title-after-split");
        let path = transcripts.write(&fixture_prefix(12));
        let first = read_all(&path, None, UNBOUNDED);
        assert_eq!(first[0].cursor.last_message_ts, Some(ts(12).millis()));

        transcripts.append(
            &FIXTURE
                .lines()
                .nth(13)
                .map(|line| format!("{line}\n"))
                .unwrap_or_default(),
        );
        let second = read_all(&path, Some(first[0].cursor), UNBOUNDED);

        let drafts = drafts_in(&second);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].kind, Kind::Session);
        assert_eq!(drafts[0].ts, ts(12));
    }

    #[test]
    fn a_partial_last_line_waits_for_its_newline() {
        let transcripts = TempTranscripts::new("partial");
        let complete = fixture_prefix(2);
        let third = FIXTURE.lines().nth(2).unwrap_or_default();
        let (head, tail) = third.split_at(third.len() / 2);
        let path = transcripts.write(&format!("{complete}{head}"));

        let first = read_all(&path, None, UNBOUNDED);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].cursor.offset, complete.len() as u64);
        assert_eq!(messages(drafts_in(&first)), fixture_messages(0..2));

        let waiting = read_increment(&path, PROFILE, Some(first[0].cursor), UNBOUNDED)
            .expect("the transcript reads");
        assert!(waiting.is_none(), "a partial line alone is not consumed");

        transcripts.append(tail);
        let still_waiting = read_increment(&path, PROFILE, Some(first[0].cursor), UNBOUNDED)
            .expect("the transcript reads");
        assert!(
            still_waiting.is_none(),
            "a line without its newline is not consumed"
        );

        transcripts.append("\n");
        let second = read_all(&path, Some(first[0].cursor), UNBOUNDED);
        assert_eq!(messages(drafts_in(&second)), fixture_messages(2..3));
        assert_eq!(
            second[0].cursor.offset,
            (complete.len() + third.len() + 1) as u64
        );
    }

    #[test]
    fn a_replaced_file_restarts_from_the_start() {
        let transcripts = TempTranscripts::new("replaced");
        let path = transcripts.write(FIXTURE);
        let first = read_all(&path, None, UNBOUNDED);

        let longer = format!("{FIXTURE}{FIXTURE}");
        transcripts.replace(&longer);
        assert_ne!(inode(&path), first[0].cursor.inode);
        let second = read_all(&path, Some(first[0].cursor), UNBOUNDED);

        assert_eq!(second[0].cursor.inode, inode(&path));
        assert_eq!(second[0].cursor.offset, longer.len() as u64);
        assert_eq!(drafts_in(&second).len(), 28);
    }

    #[test]
    fn a_truncated_file_restarts_from_the_start() {
        let transcripts = TempTranscripts::new("truncated");
        let path = transcripts.write(FIXTURE);
        let first = read_all(&path, None, UNBOUNDED);

        let shorter = fixture_prefix(3);
        fs::write(&path, &shorter).expect("the transcript could not be truncated");
        assert_eq!(inode(&path), first[0].cursor.inode);
        let second = read_all(&path, Some(first[0].cursor), UNBOUNDED);

        assert_eq!(second[0].cursor.offset, shorter.len() as u64);
        assert_eq!(messages(drafts_in(&second)), fixture_messages(0..3));
    }

    #[test]
    fn a_malformed_line_is_skipped_and_later_lines_still_read() {
        let transcripts = TempTranscripts::new("malformed");
        let text = format!(
            "{}{{\"type\": \"user\", broken\n{}",
            fixture_prefix(2),
            &FIXTURE[fixture_prefix(2).len()..]
        );
        let path = transcripts.write(&text);

        let increments = read_all(&path, None, UNBOUNDED);

        assert_eq!(messages(drafts_in(&increments)), fixture_messages(0..18));
        assert_eq!(
            increments.last().map(|increment| increment.cursor.offset),
            Some(text.len() as u64)
        );
    }

    #[test]
    fn a_record_budget_splits_a_file_into_increments_that_equal_one_read() {
        let transcripts = TempTranscripts::new("record-budget");
        let path = transcripts.write(FIXTURE);
        let whole = read_all(&path, None, UNBOUNDED);
        let budget = ReadBudget {
            max_records: 2,
            max_bytes: usize::MAX,
        };

        let split = read_all(&path, None, budget);

        assert!(split.len() > 1, "the budget split the file");
        let (last, full) = split.split_last().expect("at least one increment");
        for increment in full {
            let count = increment.drafts.len();
            assert!(
                (2..=3).contains(&count),
                "an increment stops at the line that reaches the budget: {count}"
            );
        }
        assert!(last.drafts.len() <= 3);
        assert_eq!(drafts_in(&split), drafts_in(&whole));
        assert_eq!(
            split.last().map(|increment| increment.cursor),
            Some(whole[0].cursor)
        );
    }

    #[test]
    fn a_byte_budget_splits_a_file_into_increments_that_equal_one_read() {
        let transcripts = TempTranscripts::new("byte-budget");
        let path = transcripts.write(FIXTURE);
        let whole = read_all(&path, None, UNBOUNDED);
        let budget = ReadBudget {
            max_records: usize::MAX,
            max_bytes: 1,
        };

        let split = read_all(&path, None, budget);

        assert!(split.len() > 1, "the budget split the file");
        assert_eq!(drafts_in(&split), drafts_in(&whole));
        assert_eq!(
            split.last().map(|increment| increment.cursor),
            Some(whole[0].cursor)
        );
    }

    #[test]
    fn a_read_file_yields_every_fixture_draft_with_its_mtime_on_the_leading_title() {
        let transcripts = TempTranscripts::new("mtime");
        let path = transcripts.write(FIXTURE);
        File::options()
            .write(true)
            .open(&path)
            .expect("the transcript could not be opened")
            .set_modified(UNIX_EPOCH + Duration::from_millis(FILE_MODIFIED as u64))
            .expect("the transcript mtime could not be set");

        let drafts = drafts_in(&read_all(&path, None, UNBOUNDED));

        let mut expected = Vec::new();
        for outcome in outcomes() {
            expected.extend(outcome.drafts);
        }
        assert_eq!(drafts, expected);
        assert_eq!(drafts[0].ts, Timestamp::from_millis(FILE_MODIFIED));
    }

    #[test]
    fn a_line_without_a_session_id_takes_the_file_stem() {
        let transcripts = TempTranscripts::new("stem");
        let path = transcripts.write(concat!(
            "{\"type\": \"user\", \"uuid\": \"u-1\", \"timestamp\": \"2026-09-14T11:40:00.000Z\", \"cwd\": \"/x\", \"message\": {\"content\": \"hi\"}}\n",
            "{\"type\": \"ai-title\", \"aiTitle\": \"a title\", \"sessionId\": \"\"}\n",
        ));

        let drafts = drafts_in(&read_all(&path, None, UNBOUNDED));

        assert_eq!(drafts.len(), 2);
        for draft in drafts {
            assert_eq!(draft.payload["session_id"], json!(SESSION));
        }
    }

    #[test]
    fn only_session_files_two_levels_under_projects_are_listed() {
        let temp = TempTranscripts::new("listing");
        let session = temp.write(FIXTURE);
        temp.write_subagent(SUBAGENT_LINE);
        let ClaudeRoot { projects, .. } = temp.root();
        fs::write(projects.join("stray.jsonl"), SUBAGENT_LINE)
            .expect("the stray transcript could not be written");
        fs::write(temp.project().join("notes.txt"), "not a transcript")
            .expect("the note could not be written");
        fs::create_dir_all(temp.project().join("folder.jsonl"))
            .expect("the directory could not be created");
        let other_project = projects.join("-Users-u-Projects-nikki");
        fs::create_dir_all(&other_project).expect("the project could not be created");
        let other = other_project.join("b-session.jsonl");
        fs::write(&other, SUBAGENT_LINE).expect("the transcript could not be written");

        let listed = transcripts(&projects, &mut HashSet::new()).expect("the root lists");

        assert_eq!(listed, vec![session, other]);
    }

    const SUBAGENT_LINE: &str = "{\"type\": \"user\", \"uuid\": \"subagent-1\", \"sessionId\": \"subagent-session\", \"timestamp\": \"2026-09-14T11:36:00.000Z\", \"cwd\": \"/Users/u/Projects/THE_FEUD_V2\", \"message\": {\"content\": \"a subagent prompt\"}}\n";

    fn prompt_line(index: usize) -> String {
        format!(
            "{{\"type\": \"user\", \"uuid\": \"prompt-{index}\", \"sessionId\": \"{SESSION}\", \"timestamp\": \"2026-09-14T11:40:00.000Z\", \"cwd\": \"{CWD}\", \"message\": {{\"content\": \"prompt {index}\"}}}}\n"
        )
    }

    async fn commit_into(
        handle: BufferHandle,
        mut inbox: Receiver<Emission>,
    ) -> Vec<Vec<RecordDraft>> {
        let mut emitted = Vec::new();
        while let Some(Emission {
            records,
            cursor,
            committed,
            ..
        }) = inbox.recv().await
        {
            handle
                .enqueue(records.clone(), cursor)
                .await
                .expect("the emission could not be buffered");
            emitted.push(records);
            let Some(committed) = committed else {
                continue;
            };
            let _ = committed.send(());
        }
        emitted
    }

    async fn drop_uncommitted(mut inbox: Receiver<Emission>) -> usize {
        let mut dropped = 0;
        while inbox.recv().await.is_some() {
            dropped += 1;
        }
        dropped
    }

    async fn tick(
        provider: &mut ClaudeCodeProvider,
        roots: &[ClaudeRoot],
        handle: &BufferHandle,
    ) -> Vec<Vec<RecordDraft>> {
        let (out, inbox) = channel(4);
        let committer = tokio::spawn(commit_into(handle.clone(), inbox));
        let flow = provider.tick(roots, &out).await.expect("the tick failed");
        match flow {
            Flow::Continue => {}
            Flow::RuntimeGone => panic!("the runtime is still there"),
        }
        drop(out);
        committer.await.expect("the committer panicked")
    }

    fn flattened(emitted: Vec<Vec<RecordDraft>>) -> Vec<RecordDraft> {
        let mut drafts = Vec::new();
        for records in emitted {
            drafts.extend(records);
        }
        drafts
    }

    #[tokio::test]
    async fn a_first_tick_ships_every_record_of_the_fixture_once_and_nothing_from_subagents() {
        let transcripts = TempTranscripts::new("provider-first");
        let path = transcripts.write(FIXTURE);
        transcripts.write_subagent(SUBAGENT_LINE);
        let buffer = transcripts.buffer();
        let mut provider = ClaudeCodeProvider::new(buffer.handle());

        let shipped = flattened(tick(&mut provider, &[transcripts.root()], &buffer.handle()).await);

        assert_eq!(shipped.len(), 14);
        assert_eq!(shipped, drafts_in(&read_all(&path, None, UNBOUNDED)));
        assert!(
            shipped
                .iter()
                .all(|draft| draft.payload["session_id"] == json!(SESSION)),
            "a subagent transcript must never be read"
        );
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn an_unchanged_transcript_ships_nothing_on_the_next_tick() {
        let transcripts = TempTranscripts::new("provider-unchanged");
        transcripts.write(FIXTURE);
        let buffer = transcripts.buffer();
        let mut provider = ClaudeCodeProvider::new(buffer.handle());
        let roots = [transcripts.root()];

        tick(&mut provider, &roots, &buffer.handle()).await;
        let emitted = tick(&mut provider, &roots, &buffer.handle()).await;

        assert!(emitted.is_empty(), "emitted {emitted:?}");
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn appended_lines_ship_on_the_next_tick() {
        let transcripts = TempTranscripts::new("provider-appended");
        transcripts.write(&fixture_prefix(12));
        let buffer = transcripts.buffer();
        let mut provider = ClaudeCodeProvider::new(buffer.handle());
        let roots = [transcripts.root()];

        let first = flattened(tick(&mut provider, &roots, &buffer.handle()).await);
        assert_eq!(messages(first), fixture_messages(0..12));

        let mut rest = String::new();
        for line in FIXTURE.lines().skip(12) {
            rest.push_str(line);
            rest.push('\n');
        }
        transcripts.append(&rest);
        let second = flattened(tick(&mut provider, &roots, &buffer.handle()).await);

        assert_eq!(messages(second.clone()), fixture_messages(12..18));
        assert_eq!(second.len(), 4);
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_missing_root_is_skipped_without_failing_the_provider_and_read_once_it_appears() {
        let transcripts = TempTranscripts::new("provider-missing-root");
        transcripts.write(FIXTURE);
        let buffer = transcripts.buffer();
        let mut provider = ClaudeCodeProvider::new(buffer.handle());
        let work = transcripts.work_root();
        let roots = [work.clone(), transcripts.root()];

        let first = flattened(tick(&mut provider, &roots, &buffer.handle()).await);
        let second = tick(&mut provider, &roots, &buffer.handle()).await;

        assert_eq!(first.len(), 14);
        assert!(second.is_empty());

        let project = work.projects.join("-Users-u-Projects-nikki");
        fs::create_dir_all(&project).expect("the work root could not be created");
        fs::write(project.join("work-session.jsonl"), prompt_line(1))
            .expect("the work transcript could not be written");
        let appeared = flattened(tick(&mut provider, &roots, &buffer.handle()).await);

        assert_eq!(appeared.len(), 1);
        assert_eq!(appeared[0].payload["profile"], json!("claude-work"));
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn every_transcript_of_every_root_ships_under_its_own_profile_and_cursor() {
        let transcripts = TempTranscripts::new("provider-two-roots");
        transcripts.write(&fixture_prefix(12));
        let second_project = transcripts.root().projects.join("-Users-u-Projects-nikki");
        fs::create_dir_all(&second_project).expect("the project could not be created");
        let second_session = second_project.join("second-session.jsonl");
        fs::write(&second_session, prompt_line(1)).expect("the transcript could not be written");
        let work = transcripts.work_root();
        let work_project = work.projects.join("-Users-u-Projects-THE-FEUD-V2");
        fs::create_dir_all(&work_project).expect("the work project could not be created");
        fs::write(
            work_project.join(format!("{SESSION}.jsonl")),
            prompt_line(2),
        )
        .expect("the work transcript could not be written");
        let buffer = transcripts.buffer();
        let roots = [transcripts.root(), work];

        let mut before = ClaudeCodeProvider::new(buffer.handle());
        let first = flattened(tick(&mut before, &roots, &buffer.handle()).await);
        let mut profiles = Vec::new();
        for draft in first.iter().filter(|draft| draft.kind == Kind::Message) {
            profiles.push((
                draft.payload["uuid"].clone(),
                draft.payload["profile"].clone(),
            ));
        }
        assert_eq!(first.len(), 10 + 1 + 1);
        assert!(profiles.contains(&(json!("prompt-1"), json!("claude"))));
        assert!(profiles.contains(&(json!("prompt-2"), json!("claude-work"))));
        assert_eq!(
            profiles
                .iter()
                .filter(|(_, profile)| *profile == json!("claude-work"))
                .count(),
            1
        );
        drop(before);

        let mut after = ClaudeCodeProvider::new(buffer.handle());
        let unchanged = tick(&mut after, &roots, &buffer.handle()).await;
        assert!(unchanged.is_empty(), "emitted {unchanged:?}");

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&second_session)
            .expect("the transcript could not be opened");
        file.write_all(prompt_line(3).as_bytes())
            .expect("the transcript could not be appended to");
        let appended = flattened(tick(&mut after, &roots, &buffer.handle()).await);
        assert_eq!(appended.len(), 1);
        assert_eq!(appended[0].payload["uuid"], json!("prompt-3"));
        assert_eq!(appended[0].payload["profile"], json!("claude"));
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn an_unreadable_transcript_is_skipped_and_the_next_one_still_ships() {
        let transcripts = TempTranscripts::new("provider-unreadable");
        let locked = transcripts.project().join("0-locked.jsonl");
        fs::write(&locked, prompt_line(1)).expect("the transcript could not be written");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
            .expect("the transcript could not be locked");
        transcripts.write(FIXTURE);
        let buffer = transcripts.buffer();
        let mut provider = ClaudeCodeProvider::new(buffer.handle());
        let roots = [transcripts.root()];

        let first = flattened(tick(&mut provider, &roots, &buffer.handle()).await);
        assert_eq!(first.len(), 14);
        assert!(
            first
                .iter()
                .all(|draft| draft.payload["uuid"] != json!("prompt-1"))
        );

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o600))
            .expect("the transcript could not be unlocked");
        let second = flattened(tick(&mut provider, &roots, &buffer.handle()).await);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].payload["uuid"], json!("prompt-1"));
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_restarted_provider_resumes_from_the_committed_cursor() {
        let transcripts = TempTranscripts::new("provider-restart");
        transcripts.write(&fixture_prefix(12));
        let buffer = transcripts.buffer();
        let roots = [transcripts.root()];

        let mut before = ClaudeCodeProvider::new(buffer.handle());
        tick(&mut before, &roots, &buffer.handle()).await;
        drop(before);

        let mut after = ClaudeCodeProvider::new(buffer.handle());
        let unchanged = tick(&mut after, &roots, &buffer.handle()).await;
        assert!(unchanged.is_empty(), "emitted {unchanged:?}");

        transcripts.append(&prompt_line(1));
        let appended = flattened(tick(&mut after, &roots, &buffer.handle()).await);
        assert_eq!(appended.len(), 1);
        assert_eq!(appended[0].payload["uuid"], json!("prompt-1"));
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn records_that_were_never_buffered_are_read_again_on_the_next_tick() {
        let transcripts = TempTranscripts::new("provider-uncommitted");
        transcripts.write(FIXTURE);
        let buffer = transcripts.buffer();
        let mut provider = ClaudeCodeProvider::new(buffer.handle());
        let roots = [transcripts.root()];

        let (out, inbox) = channel(4);
        let dropping = tokio::spawn(drop_uncommitted(inbox));
        provider
            .tick(&roots, &out)
            .await
            .expect("an unbuffered emission is not a provider failure");
        drop(out);
        assert_eq!(dropping.await.expect("the dropper panicked"), 1);

        let again = flattened(tick(&mut provider, &roots, &buffer.handle()).await);
        assert_eq!(again.len(), 14);
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_transcript_of_only_tool_lines_advances_its_cursor_without_records() {
        let transcripts = TempTranscripts::new("provider-tool-lines");
        let mut text = String::new();
        for line in FIXTURE.lines().skip(3).take(2) {
            text.push_str(line);
            text.push('\n');
        }
        transcripts.write(&text);
        let buffer = transcripts.buffer();
        let mut provider = ClaudeCodeProvider::new(buffer.handle());
        let roots = [transcripts.root()];

        let first = tick(&mut provider, &roots, &buffer.handle()).await;
        let second = tick(&mut provider, &roots, &buffer.handle()).await;

        assert_eq!(first, vec![Vec::new()]);
        assert!(second.is_empty(), "the tool lines were read again");
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_transcript_past_the_budget_ships_in_several_emissions() {
        let transcripts = TempTranscripts::new("provider-budget");
        let mut text = String::new();
        for index in 0..READ_BUDGET.max_records + 100 {
            text.push_str(&prompt_line(index));
        }
        transcripts.write(&text);
        let buffer = transcripts.buffer();
        let mut provider = ClaudeCodeProvider::new(buffer.handle());

        let emitted = tick(&mut provider, &[transcripts.root()], &buffer.handle()).await;

        assert_eq!(emitted.len(), 2);
        assert_eq!(emitted[0].len(), READ_BUDGET.max_records);
        assert_eq!(flattened(emitted).len(), READ_BUDGET.max_records + 100);
        buffer.close().await.expect("the buffer did not close");
    }

    #[tokio::test]
    async fn a_buffer_that_refuses_cursor_reads_fails_the_provider() {
        let transcripts = TempTranscripts::new("provider-closed-buffer");
        transcripts.write(FIXTURE);
        let buffer = transcripts.buffer();
        let mut provider = ClaudeCodeProvider::new(buffer.handle());
        buffer.close().await.expect("the buffer did not close");

        let (out, _inbox) = channel(4);
        let outcome = provider.tick(&[transcripts.root()], &out).await;

        let Err(ProviderError(reason)) = outcome else {
            panic!("a refused cursor read must fail the provider");
        };
        assert!(reason.contains("cursor"), "reason was {reason}");
    }

    #[tokio::test]
    async fn the_running_provider_ships_at_once_and_stops_once_the_runtime_is_gone() {
        let transcripts = TempTranscripts::new("provider-run");
        transcripts.write(FIXTURE);
        let buffer = transcripts.buffer();
        let handle = buffer.handle();
        let mut provider = ClaudeCodeProvider::new(buffer.handle());
        let mut config = test_config(30);
        config.claude_code.roots = vec![transcripts.root()];
        config.claude_code.poll_interval = 1;
        let ctx = Ctx {
            config: Arc::new(config),
        };
        let (out, mut inbox) = channel(4);
        let running = tokio::spawn(async move { provider.run(ctx, out).await });

        let Some(Emission {
            records,
            cursor,
            committed,
            ships_before_commit,
        }) = inbox.recv().await
        else {
            panic!("the provider stopped without emitting");
        };
        assert!(!ships_before_commit);
        assert_eq!(records.len(), 14);
        let Some(Cursor { provider, key, .. }) = &cursor else {
            panic!("the emission carries no cursor");
        };
        assert_eq!(*provider, runtime::Provider::ClaudeCode);
        assert_eq!(*key, transcripts.session().to_string_lossy());
        handle
            .enqueue(records, cursor)
            .await
            .expect("the emission could not be buffered");
        let Some(committed) = committed else {
            panic!("the emission awaits no commit");
        };
        let _ = committed.send(());

        drop(inbox);
        transcripts.append(&prompt_line(1));
        let stopped = tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("the provider did not notice the runtime was gone")
            .expect("the provider panicked");
        assert!(stopped.is_ok(), "stopped with {stopped:?}");
        buffer.close().await.expect("the buffer did not close");
    }

    fn meta_uuids(path: &Path, end: u64) -> HashSet<String> {
        let text = fs::read(path).expect("the live transcript reads");
        let consumed = &text[..end as usize];
        let mut uuids = HashSet::new();
        for line in consumed.split(|byte| *byte == b'\n') {
            let Ok(value) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if !flag(&value, "isMeta") {
                continue;
            }
            let Some(uuid) = value.get("uuid").and_then(Value::as_str) else {
                continue;
            };
            uuids.insert(uuid.to_string());
        }
        uuids
    }

    #[test]
    #[ignore = "reads the live machine: the real transcripts under the default ~/.claude roots"]
    fn the_live_transcripts_parse_without_loss() {
        let home = std::env::var("HOME").expect("HOME is set");
        let ClaudeCode { roots, .. } =
            default_claude_code(Path::new(&home)).expect("the default roots resolve");

        let mut files = 0;
        let mut records = 0;
        let mut messages = 0;
        for ClaudeRoot { profile, projects } in &roots {
            let Ok(paths) = transcripts(projects, &mut HashSet::new()) else {
                continue;
            };
            for path in paths {
                let length = fs::metadata(&path)
                    .expect("the live transcript has metadata")
                    .len();
                let mut cursor = None;
                let mut shipped = Vec::new();
                while let Some(Increment {
                    drafts,
                    cursor: advanced,
                }) = read_increment(&path, profile, cursor, READ_BUDGET)
                    .expect("the live transcript reads")
                {
                    records += drafts.len();
                    for draft in drafts {
                        if draft.kind != Kind::Message {
                            continue;
                        }
                        messages += 1;
                        let Some(uuid) = draft.payload.get("uuid").and_then(Value::as_str) else {
                            panic!("a message carries no uuid in {}", path.display());
                        };
                        shipped.push(uuid.to_string());
                    }
                    cursor = Some(advanced);
                }

                let offset = cursor.map_or(0, |FileCursor { offset, .. }| offset);
                if offset < length {
                    let text = fs::read(&path).expect("the live transcript reads");
                    let unread = &text[offset as usize..length as usize];
                    assert!(
                        !unread.contains(&b'\n'),
                        "{} stopped at {offset} of {length} with a complete line unread",
                        path.display()
                    );
                }

                let meta = meta_uuids(&path, offset);
                for uuid in &shipped {
                    assert!(
                        !meta.contains(uuid),
                        "{} shipped the isMeta line {uuid}",
                        path.display()
                    );
                }
                files += 1;
            }
        }
        assert!(files > 0, "no transcript was found under {roots:?}");
        assert!(records > 0, "{files} transcripts yielded no record");
        assert!(messages > 0, "{files} transcripts yielded no message");
    }
}
