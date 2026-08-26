use std::env;
use std::path::{Path, PathBuf};
use std::sync::Once;

use serde::Deserialize;
use serde_json::Value;

use crate::extract::{CommandOutput, Details, SUBPROCESS_DEADLINE, run_with_deadline};

const PROGRAM: &str = "agtermctl";
const BUNDLED_PROGRAM: &str = "/Applications/agterm.app/Contents/MacOS/agtermctl";

static MISSING_PROGRAM_WARNING: Once = Once::new();

pub async fn active_session() -> Details {
    let Some(program) = resolve_program() else {
        MISSING_PROGRAM_WARNING.call_once(|| {
            tracing::warn!(
                "{PROGRAM} is on neither PATH nor {BUNDLED_PROGRAM}, so no terminal workspace will be captured"
            );
        });
        return Details::new();
    };

    let output = run_with_deadline(&program, &["tree", "--json"], SUBPROCESS_DEADLINE).await;
    let Some(CommandOutput {
        succeeded,
        stdout,
        stderr,
    }) = output
    else {
        return Details::new();
    };
    if !succeeded {
        tracing::debug!(stderr = stderr.trim(), "the terminal tree command failed");
        return Details::new();
    }
    parse_tree(&stdout)
}

fn resolve_program() -> Option<PathBuf> {
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            let candidate = directory.join(PROGRAM);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let bundled = PathBuf::from(BUNDLED_PROGRAM);
    if bundled.is_file() {
        return Some(bundled);
    }
    None
}

fn parse_tree(stdout: &str) -> Details {
    let response: Response = match serde_json::from_str(stdout) {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!(%error, "the terminal tree did not parse");
            return Details::new();
        }
    };
    let Response {
        result: TreeResult {
            tree: Tree { workspaces },
        },
    } = response;

    let mut active = None;
    for workspace in workspaces {
        if workspace.active {
            active = Some(workspace);
            break;
        }
    }
    let Some(Workspace {
        name: workspace,
        active: _,
        sessions,
    }) = active
    else {
        return Details::new();
    };

    let mut active = None;
    for session in sessions {
        if session.active {
            active = Some(session);
            break;
        }
    }
    let Some(Session {
        name: session,
        active: _,
        cwd,
        foreground,
    }) = active
    else {
        return Details::new();
    };

    let mut details = Details::new();
    details.insert("workspace".to_string(), Value::String(workspace));
    details.insert("session".to_string(), Value::String(session));
    if let Some(cwd) = cwd
        && !cwd.is_empty()
    {
        details.insert("cwd".to_string(), Value::String(cwd));
    }

    let Some(foreground) = foreground else {
        return details;
    };
    let Some(command) = foreground.first() else {
        return details;
    };
    let Some(command) = file_name(command) else {
        return details;
    };
    details.insert("foreground".to_string(), Value::String(command));
    details
}

fn file_name(command: &str) -> Option<String> {
    let name = Path::new(command).file_name()?;
    let name = name.to_str()?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

#[derive(Deserialize)]
struct Response {
    result: TreeResult,
}

#[derive(Deserialize)]
struct TreeResult {
    tree: Tree,
}

#[derive(Deserialize)]
struct Tree {
    #[serde(default)]
    workspaces: Vec<Workspace>,
}

#[derive(Deserialize)]
struct Workspace {
    name: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    sessions: Vec<Session>,
}

#[derive(Deserialize)]
struct Session {
    name: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    foreground: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURED: &str = include_str!("../../fixtures/agterm_tree.json");

    fn captured() -> Value {
        serde_json::from_str(CAPTURED).expect("the fixture parses")
    }

    fn sessions_of(workspace: &mut Value) -> &mut Vec<Value> {
        workspace["sessions"]
            .as_array_mut()
            .expect("a workspace has sessions")
    }

    fn workspaces_of(tree: &mut Value) -> &mut Vec<Value> {
        tree["result"]["tree"]["workspaces"]
            .as_array_mut()
            .expect("the tree has workspaces")
    }

    #[test]
    fn the_captured_tree_yields_only_the_active_session() {
        let details = parse_tree(CAPTURED);
        assert_eq!(details["workspace"], "nikki");
        assert_eq!(details["session"], "nikki daemon");
        assert_eq!(details["cwd"], "/Users/pavel.karpovich/Projects/nikki");
        assert_eq!(details["foreground"], "claude");
        assert_eq!(details.len(), 4);
    }

    #[test]
    fn a_session_running_nothing_carries_no_foreground() {
        let mut tree = captured();
        for workspace in workspaces_of(&mut tree) {
            for session in sessions_of(workspace) {
                let running_nothing = session["foreground"].is_null();
                session["active"] = Value::Bool(running_nothing);
            }
        }
        let details = parse_tree(&tree.to_string());
        assert_eq!(details["session"], "notes");
        assert_eq!(details["cwd"], "/Users/pavel.karpovich/Obsidian");
        assert!(!details.contains_key("foreground"));
    }

    #[test]
    fn an_empty_foreground_array_carries_no_foreground() {
        let mut tree = captured();
        for workspace in workspaces_of(&mut tree) {
            for session in sessions_of(workspace) {
                session["foreground"] = Value::Array(Vec::new());
            }
        }
        let details = parse_tree(&tree.to_string());
        assert_eq!(details["session"], "nikki daemon");
        assert!(!details.contains_key("foreground"));
    }

    #[test]
    fn a_tree_with_no_active_session_yields_nothing() {
        let mut tree = captured();
        for workspace in workspaces_of(&mut tree) {
            for session in sessions_of(workspace) {
                session["active"] = Value::Bool(false);
            }
        }
        assert!(parse_tree(&tree.to_string()).is_empty());
    }

    #[test]
    fn a_tree_with_no_active_workspace_yields_nothing() {
        let mut tree = captured();
        for workspace in workspaces_of(&mut tree) {
            workspace["active"] = Value::Bool(false);
        }
        assert!(parse_tree(&tree.to_string()).is_empty());
    }

    #[test]
    fn an_active_session_in_an_inactive_workspace_is_never_reported() {
        let mut tree = captured();
        let workspaces = workspaces_of(&mut tree);
        workspaces[1]["active"] = Value::Bool(false);
        for session in sessions_of(&mut workspaces[0]) {
            session["active"] = Value::Bool(true);
        }
        assert!(parse_tree(&tree.to_string()).is_empty());
    }

    #[test]
    fn an_unparseable_or_incomplete_response_yields_nothing() {
        assert!(parse_tree("").is_empty());
        assert!(parse_tree("not json at all").is_empty());
        assert!(parse_tree(r#"{"ok":false,"error":"no server"}"#).is_empty());
        assert!(parse_tree(r#"{"ok":true,"result":{"tree":{"workspaces":[]}}}"#).is_empty());
    }

    #[test]
    fn the_stored_foreground_is_the_file_name_of_the_first_argument() {
        assert_eq!(
            file_name("/Users/pavel.karpovich/.local/bin/claude"),
            Some("claude".to_string())
        );
        assert_eq!(file_name("tail"), Some("tail".to_string()));
        assert_eq!(file_name(""), None);
        assert_eq!(file_name("/"), None);
    }
}
