use std::env;
use std::path::{Path, PathBuf};
use std::sync::Once;

use serde::Deserialize;
use serde_json::Value;

use crate::extract::{CommandOutput, Details, SUBPROCESS_DEADLINE, run_with_deadline};
use crate::macos::processes::{Pane, agterm_panes};

const PROGRAM: &str = "agtermctl";
const BUNDLED_PROGRAM: &str = "/Applications/agterm.app/Contents/MacOS/agtermctl";
const LEFT_SURFACE: &str = "left";
const QUICK_SURFACE: &str = "quick";
const SURFACE_ADDRESS: &str = "surface:";
const COMMAND_LIMIT: usize = 512;

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
    parse_tree(&stdout, &agterm_panes())
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

fn parse_tree(stdout: &str, panes: &[Pane]) -> Details {
    let response: Response = match serde_json::from_str(stdout) {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!(%error, "the terminal tree did not parse");
            return Details::new();
        }
    };
    let Response {
        result: TreeResult {
            tree: Tree { workspaces, screen },
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
    let Some(session) = active else {
        return Details::new();
    };

    compose(&workspace, &session, panes, &screen)
}

fn compose(workspace: &str, session: &Session, panes: &[Pane], screen: &Screen) -> Details {
    let Session {
        id,
        name,
        active: _,
        cwd,
        foreground,
        surfaces,
    } = session;

    let mut details = Details::new();
    details.insert(
        "workspace".to_string(),
        Value::String(workspace.to_string()),
    );
    details.insert("session".to_string(), Value::String(session_identity(name)));

    let surface = surface_on_screen(screen, id, surfaces);
    let mut on_screen = None;
    if let Some(surface) = &surface {
        details.insert("surface".to_string(), Value::String(surface.clone()));
        on_screen = pane_on(panes, id, surface);
    }

    let on_left = surface.as_deref() == Some(LEFT_SURFACE);
    if let Some(command) = on_screen.and_then(|pane| command_line(&pane.argv)) {
        details.insert("command".to_string(), Value::String(command));
    }

    let pane_cwd = on_screen.and_then(|pane| pane.cwd.clone());
    let cwd = match pane_cwd {
        Some(pane_cwd) => Some(pane_cwd),
        None if on_left => cwd.clone(),
        None => None,
    };
    if let Some(cwd) = cwd
        && !cwd.is_empty()
    {
        details.insert("cwd".to_string(), Value::String(cwd));
    }

    if !on_left {
        return details;
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

fn pane_on<'a>(panes: &'a [Pane], session: &str, surface: &str) -> Option<&'a Pane> {
    let session = session.to_uppercase();
    let mut found = None;
    for pane in panes {
        if pane.session == session && pane.pane == surface {
            found = Some(pane);
            break;
        }
    }
    found
}

fn command_line(argv: &[String]) -> Option<String> {
    let command = argv.join(" ");
    if command.is_empty() {
        return None;
    }
    if command.chars().count() <= COMMAND_LIMIT {
        return Some(command);
    }

    let mut cut: String = command.chars().take(COMMAND_LIMIT).collect();
    cut.push('…');
    Some(cut)
}

fn surface_on_screen(screen: &Screen, session: &str, surfaces: &[Surface]) -> Option<String> {
    if screen.quick_visible {
        return Some(QUICK_SURFACE.to_string());
    }
    let Some(zoomed) = &screen.zoomed_surface else {
        return active_surface(surfaces);
    };
    zoomed_surface(zoomed, session)
}

fn zoomed_surface(zoomed: &str, session: &str) -> Option<String> {
    if zoomed == QUICK_SURFACE {
        return Some(QUICK_SURFACE.to_string());
    }
    let address = zoomed.strip_prefix(SURFACE_ADDRESS)?;
    let (zoomed_session, kind) = address.rsplit_once(':')?;
    if kind.is_empty() || !zoomed_session.eq_ignore_ascii_case(session) {
        return None;
    }
    Some(kind.to_string())
}

fn active_surface(surfaces: &[Surface]) -> Option<String> {
    for Surface {
        kind,
        active,
        visible,
    } in surfaces
    {
        if *active && *visible {
            return Some(kind.clone());
        }
    }
    None
}

const STATUS_GLYPHS: [char; 11] = ['✳', '✢', '✶', '✻', '✽', '◐', '◓', '◑', '◒', '●', '○'];

fn session_identity(name: &str) -> String {
    let name = name.trim();
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };
    if !STATUS_GLYPHS.contains(&first) {
        return name.to_string();
    }
    characters.as_str().trim_start().to_string()
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
    #[serde(flatten)]
    screen: Screen,
}

#[derive(Deserialize, Default)]
struct Screen {
    #[serde(default, rename = "quickVisible")]
    quick_visible: bool,
    #[serde(default, rename = "zoomedSurface")]
    zoomed_surface: Option<String>,
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
    #[serde(default)]
    id: String,
    name: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    foreground: Option<Vec<String>>,
    #[serde(default)]
    surfaces: Vec<Surface>,
}

#[derive(Deserialize)]
struct Surface {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    visible: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURED: &str = include_str!("../../fixtures/agterm_tree.json");
    const SCRATCH: &str = include_str!("../../fixtures/agterm_tree_scratch.json");

    fn captured() -> Value {
        serde_json::from_str(CAPTURED).expect("the fixture parses")
    }

    fn surface(kind: &str, active: bool, visible: bool) -> Surface {
        Surface {
            kind: kind.to_string(),
            active,
            visible,
        }
    }

    fn pane(session: &str, kind: &str, argv: &[&str], cwd: Option<&str>) -> Pane {
        let mut arguments = Vec::new();
        for argument in argv {
            arguments.push((*argument).to_string());
        }
        Pane {
            session: session.to_string(),
            pane: kind.to_string(),
            pane_id: "p7".to_string(),
            argv: arguments,
            cwd: cwd.map(str::to_string),
        }
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
        let details = parse_tree(CAPTURED, &[]);
        assert_eq!(details["workspace"], "nikki");
        assert_eq!(details["session"], "nikki daemon");
        assert_eq!(details["cwd"], "/Users/pavel.karpovich/Projects/nikki");
        assert_eq!(details["surface"], "left");
        assert_eq!(details["foreground"], "claude");
        assert_eq!(details.len(), 5);
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
        let details = parse_tree(&tree.to_string(), &[]);
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
        let details = parse_tree(&tree.to_string(), &[]);
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
        assert!(parse_tree(&tree.to_string(), &[]).is_empty());
    }

    #[test]
    fn a_tree_with_no_active_workspace_yields_nothing() {
        let mut tree = captured();
        for workspace in workspaces_of(&mut tree) {
            workspace["active"] = Value::Bool(false);
        }
        assert!(parse_tree(&tree.to_string(), &[]).is_empty());
    }

    #[test]
    fn an_active_session_in_an_inactive_workspace_is_never_reported() {
        let mut tree = captured();
        let workspaces = workspaces_of(&mut tree);
        assert_eq!(workspaces[1]["active"], Value::Bool(true));
        for session in sessions_of(&mut workspaces[1]) {
            session["active"] = Value::Bool(false);
        }
        for session in sessions_of(&mut workspaces[0]) {
            session["active"] = Value::Bool(true);
        }
        assert!(parse_tree(&tree.to_string(), &[]).is_empty());
    }

    #[test]
    fn an_unparseable_or_incomplete_response_yields_nothing() {
        assert!(parse_tree("", &[]).is_empty());
        assert!(parse_tree("not json at all", &[]).is_empty());
        assert!(parse_tree(r#"{"ok":false,"error":"no server"}"#, &[]).is_empty());
        assert!(parse_tree(r#"{"ok":true,"result":{"tree":{"workspaces":[]}}}"#, &[]).is_empty());
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

    #[test]
    fn the_active_surface_is_the_first_one_both_active_and_visible() {
        assert_eq!(
            active_surface(&[surface("left", true, true)]),
            Some("left".to_string())
        );
        assert_eq!(
            active_surface(&[
                surface("left", false, false),
                surface("scratch", true, true),
            ]),
            Some("scratch".to_string())
        );
    }

    #[test]
    fn a_surface_that_is_active_but_hidden_is_not_the_active_surface() {
        assert_eq!(active_surface(&[surface("left", true, false)]), None);
    }

    #[test]
    fn no_active_surface_and_no_surfaces_at_all_both_yield_nothing() {
        assert_eq!(
            active_surface(&[surface("left", false, true), surface("right", false, true)]),
            None
        );
        assert_eq!(active_surface(&[]), None);
    }

    #[test]
    fn the_scratch_tree_reports_its_scratch_pane_as_the_active_surface() {
        let details = parse_tree(SCRATCH, &[]);
        assert_eq!(details["workspace"], "nikki");
        assert_eq!(details["session"], "nikki daemon");
        assert_eq!(details["surface"], "scratch");
    }

    #[test]
    fn a_visible_quick_terminal_is_the_surface_on_screen_and_names_no_pane() {
        let mut tree = captured();
        tree["result"]["tree"]["quickVisible"] = Value::Bool(true);
        let panes = [pane(
            "5E7B21C4-6F30-4D9A-A8B5-3C2E1D0F9A46",
            "left",
            &["claude", "--resume"],
            Some("/Users/pavel.karpovich/Projects/nikki/src"),
        )];

        let details = parse_tree(&tree.to_string(), &panes);

        assert_eq!(details["session"], "nikki daemon");
        assert_eq!(details["surface"], "quick");
        assert!(!details.contains_key("command"));
        assert!(!details.contains_key("cwd"));
        assert!(!details.contains_key("foreground"));
    }

    #[test]
    fn a_zoomed_pane_outranks_the_one_the_session_calls_active() {
        let mut tree = captured();
        tree["result"]["tree"]["zoomedSurface"] =
            Value::String("surface:5E7B21C4-6F30-4D9A-A8B5-3C2E1D0F9A46:scratch".to_string());
        let panes = [
            pane(
                "5E7B21C4-6F30-4D9A-A8B5-3C2E1D0F9A46",
                "left",
                &["claude", "--resume"],
                Some("/Users/pavel.karpovich/Projects/nikki/src"),
            ),
            pane(
                "5E7B21C4-6F30-4D9A-A8B5-3C2E1D0F9A46",
                "scratch",
                &["rx", "plan.md"],
                Some("/Users/pavel.karpovich/Projects/nikki/docs"),
            ),
        ];

        let details = parse_tree(&tree.to_string(), &panes);

        assert_eq!(details["surface"], "scratch");
        assert_eq!(details["command"], "rx plan.md");
        assert_eq!(details["cwd"], "/Users/pavel.karpovich/Projects/nikki/docs");
        assert!(!details.contains_key("foreground"));
    }

    #[test]
    fn a_zoom_that_names_another_session_leaves_the_surface_unknown() {
        let mut tree = captured();
        tree["result"]["tree"]["zoomedSurface"] =
            Value::String("surface:A-DIFFERENT-SESSION:left".to_string());
        let panes = [pane(
            "5E7B21C4-6F30-4D9A-A8B5-3C2E1D0F9A46",
            "left",
            &["claude", "--resume"],
            Some("/Users/pavel.karpovich/Projects/nikki/src"),
        )];

        let details = parse_tree(&tree.to_string(), &panes);

        assert_eq!(details["session"], "nikki daemon");
        assert!(!details.contains_key("surface"));
        assert!(!details.contains_key("command"));
        assert!(!details.contains_key("foreground"));
    }

    #[test]
    fn a_zoomed_surface_is_read_from_its_control_address() {
        let session = "5E7B21C4-6F30-4D9A-A8B5-3C2E1D0F9A46";
        assert_eq!(
            zoomed_surface(&format!("surface:{session}:right"), session),
            Some("right".to_string())
        );
        assert_eq!(
            zoomed_surface(&format!("surface:{}:left", session.to_lowercase()), session),
            Some("left".to_string())
        );
        assert_eq!(zoomed_surface("quick", session), Some("quick".to_string()));
        assert_eq!(zoomed_surface("", session), None);
        assert_eq!(
            zoomed_surface(&format!("surface:{session}:"), session),
            None
        );
        assert_eq!(zoomed_surface("surface:left", session), None);
    }

    #[test]
    fn a_session_omitting_surfaces_still_parses() {
        let mut tree = captured();
        for workspace in workspaces_of(&mut tree) {
            for session in sessions_of(workspace) {
                let session = session.as_object_mut().expect("a session is an object");
                session.remove("surfaces");
            }
        }
        let details = parse_tree(&tree.to_string(), &[]);
        assert_eq!(details["session"], "nikki daemon");
        assert!(!details.contains_key("surface"));
        assert!(!details.contains_key("foreground"));
    }

    #[test]
    fn an_animated_status_glyph_is_not_part_of_the_session_identity() {
        assert_eq!(session_identity("✳ План создания"), "План создания");
        assert_eq!(session_identity("◑ План создания"), "План создания");
        assert_eq!(session_identity("◐ План создания"), "План создания");
        assert_eq!(
            session_identity("●ask-dealcloud: done"),
            "ask-dealcloud: done"
        );
    }

    #[test]
    fn a_visible_scratch_pane_is_reported_instead_of_the_hidden_left_one() {
        let panes = [
            pane(
                "5E7B21C4-6F30-4D9A-A8B5-3C2E1D0F9A46",
                "left",
                &["claude", "--resume"],
                Some("/Users/pavel.karpovich/Projects/nikki"),
            ),
            pane(
                "5E7B21C4-6F30-4D9A-A8B5-3C2E1D0F9A46",
                "scratch",
                &["rx", "docs/plans/2026-08-27-agterm-panes.md"],
                Some("/Users/pavel.karpovich/Projects/nikki/docs"),
            ),
        ];

        let details = parse_tree(SCRATCH, &panes);

        assert_eq!(details["workspace"], "nikki");
        assert_eq!(details["session"], "nikki daemon");
        assert_eq!(details["surface"], "scratch");
        assert_eq!(
            details["command"],
            "rx docs/plans/2026-08-27-agterm-panes.md"
        );
        assert_eq!(details["cwd"], "/Users/pavel.karpovich/Projects/nikki/docs");
        assert!(!details.contains_key("foreground"));
    }

    #[test]
    fn a_visible_left_pane_carries_the_session_foreground_alongside_its_command() {
        let panes = [pane(
            "5E7B21C4-6F30-4D9A-A8B5-3C2E1D0F9A46",
            "left",
            &["claude", "--resume"],
            Some("/Users/pavel.karpovich/Projects/nikki/src"),
        )];

        let details = parse_tree(CAPTURED, &panes);

        assert_eq!(details["surface"], "left");
        assert_eq!(details["foreground"], "claude");
        assert_eq!(details["command"], "claude --resume");
        assert_eq!(details["cwd"], "/Users/pavel.karpovich/Projects/nikki/src");
    }

    #[test]
    fn a_hidden_left_pane_never_lends_its_directory_to_the_surface_on_screen() {
        let panes = [pane(
            "A-DIFFERENT-SESSION",
            "scratch",
            &["rx"],
            Some("/tmp"),
        )];

        let details = parse_tree(SCRATCH, &panes);

        assert_eq!(details["surface"], "scratch");
        assert!(!details.contains_key("command"));
        assert!(!details.contains_key("cwd"));
        assert!(!details.contains_key("foreground"));
    }

    #[test]
    fn a_scratch_pane_reporting_no_directory_reports_none_rather_than_the_sessions() {
        let panes = [pane(
            "5E7B21C4-6F30-4D9A-A8B5-3C2E1D0F9A46",
            "scratch",
            &["rx"],
            None,
        )];

        let details = parse_tree(SCRATCH, &panes);

        assert_eq!(details["command"], "rx");
        assert!(!details.contains_key("cwd"));
    }

    #[test]
    fn a_left_pane_reporting_no_directory_falls_back_to_the_session_directory() {
        let panes = [pane(
            "5E7B21C4-6F30-4D9A-A8B5-3C2E1D0F9A46",
            "left",
            &["claude"],
            None,
        )];

        let details = parse_tree(CAPTURED, &panes);

        assert_eq!(details["command"], "claude");
        assert_eq!(details["cwd"], "/Users/pavel.karpovich/Projects/nikki");
    }

    #[test]
    fn a_command_longer_than_the_cap_is_cut_and_marked() {
        let argument = "a".repeat(600);
        let command = command_line(std::slice::from_ref(&argument)).expect("a command is reported");

        assert_eq!(command.chars().count(), COMMAND_LIMIT + 1);
        assert!(command.ends_with('…'));
        assert!(command.starts_with(&argument[..COMMAND_LIMIT]));
    }

    #[test]
    fn a_command_within_the_cap_keeps_all_of_its_arguments() {
        assert_eq!(
            command_line(&["rx".to_string(), "plan.md".to_string()]),
            Some("rx plan.md".to_string())
        );
        assert_eq!(command_line(&[]), None);
        assert_eq!(command_line(&[String::new()]), None);
    }

    #[tokio::test]
    #[ignore = "reads the live agterm tree, so scripts/acceptance.sh runs it and cargo test does not"]
    async fn the_live_tree_names_the_surface_on_screen() {
        let details = active_session().await;
        if details.is_empty() {
            println!("agterm reports no active session, so there is nothing on screen to name");
            return;
        }

        println!("{}", Value::Object(details.clone()));
        assert!(details.contains_key("workspace"));
        assert!(details.contains_key("session"));
        assert!(
            details.contains_key("surface"),
            "a live session must name the surface the user is looking at"
        );

        let panes = agterm_panes();
        println!("the process table yields {} agterm panes", panes.len());
        assert!(
            !panes.is_empty(),
            "a live session runs a shell, so the process table must yield at least one pane"
        );
        assert!(
            panes.iter().any(|pane| !pane.argv.is_empty()),
            "a pane read out of the process table carries the argv of the job in front of it"
        );
    }

    #[test]
    fn a_hand_named_session_survives_identity_stripping_unchanged() {
        assert_eq!(session_identity("nikki"), "nikki");
        assert_eq!(session_identity("nhop"), "nhop");
        assert_eq!(session_identity("nikki daemon"), "nikki daemon");
        assert_eq!(session_identity(""), "");
    }
}
