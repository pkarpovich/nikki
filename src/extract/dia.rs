use std::path::Path;
use std::sync::Once;

use serde_json::Value;

use crate::extract::{CommandOutput, Details, SUBPROCESS_DEADLINE, run_with_deadline};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Application {
    Dia,
    Ardia,
}

impl Application {
    fn name(self) -> &'static str {
        match self {
            Application::Dia => "Dia",
            Application::Ardia => "Ardia",
        }
    }

    fn denial_warning(self) -> &'static Once {
        match self {
            Application::Dia => &DIA_AUTOMATION_DENIED_WARNING,
            Application::Ardia => &ARDIA_AUTOMATION_DENIED_WARNING,
        }
    }
}

fn script(application: Application) -> String {
    let name = application.name();
    format!(
        r#"if not (running of application "{name}") then return ""
tell application "{name}"
  set w to front window
  set t to active tab of w
  set u to (URL of t) as text
  set ti to (title of t) as text
  set pr to (name of active profile of w) as text
  set pn to (isPinned of t) as text
end tell
set AppleScript's text item delimiters to (ASCII character 31)
return {{u, ti, pr, pn}} as text"#
    )
}

const OSASCRIPT: &str = "/usr/bin/osascript";
const UNIT_SEPARATOR: char = '\u{1f}';

const AUTOMATION_DENIED: i32 = -1743;
const NO_FRONT_WINDOW: i32 = -1728;
const APPLE_EVENT_TIMEOUT: i32 = -1712;

static DIA_AUTOMATION_DENIED_WARNING: Once = Once::new();
static ARDIA_AUTOMATION_DENIED_WARNING: Once = Once::new();

pub async fn active_tab(application: Application) -> Details {
    let script = script(application);
    let output =
        run_with_deadline(Path::new(OSASCRIPT), &["-e", &script], SUBPROCESS_DEADLINE).await;
    let Some(output) = output else {
        return Details::new();
    };
    details_from(application, output)
}

fn details_from(application: Application, output: CommandOutput) -> Details {
    let CommandOutput {
        succeeded,
        stdout,
        stderr,
    } = output;
    if !succeeded {
        report(application, script_error(&stderr));
        return Details::new();
    }
    parse_tab(&stdout)
}

fn parse_tab(stdout: &str) -> Details {
    let stdout = stdout.trim_end_matches(['\n', '\r']);
    if stdout.is_empty() {
        return Details::new();
    }

    let fields: Vec<&str> = stdout.split(UNIT_SEPARATOR).collect();
    let [url, tab, profile, pinned] = fields[..] else {
        tracing::debug!(
            fields = fields.len(),
            "the browser tab script returned an unexpected number of fields"
        );
        return Details::new();
    };
    if url.is_empty() {
        return Details::new();
    }

    let mut details = Details::new();
    details.insert("url".to_string(), Value::String(url.to_string()));
    if !tab.is_empty() {
        details.insert("tab".to_string(), Value::String(tab.to_string()));
    }
    if !profile.is_empty() {
        details.insert("profile".to_string(), Value::String(profile.to_string()));
    }
    let pinned = match pinned {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    };
    if let Some(pinned) = pinned {
        details.insert("pinned".to_string(), Value::Bool(pinned));
    }
    details
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptError {
    AutomationDenied,
    NoFrontWindow,
    AppleEventTimeout,
    Unknown(Option<i32>),
}

fn script_error(stderr: &str) -> ScriptError {
    let Some(code) = trailing_code(stderr) else {
        return ScriptError::Unknown(None);
    };
    match code {
        AUTOMATION_DENIED => ScriptError::AutomationDenied,
        NO_FRONT_WINDOW => ScriptError::NoFrontWindow,
        APPLE_EVENT_TIMEOUT => ScriptError::AppleEventTimeout,
        code => ScriptError::Unknown(Some(code)),
    }
}

fn trailing_code(stderr: &str) -> Option<i32> {
    let stderr = stderr.trim_end();
    let stderr = stderr.strip_suffix(')')?;
    let opening = stderr.rfind('(')?;
    let Ok(code) = stderr[opening + 1..].parse::<i32>() else {
        return None;
    };
    Some(code)
}

fn report(application: Application, error: ScriptError) {
    match error {
        ScriptError::AutomationDenied => application.denial_warning().call_once(|| {
            tracing::warn!(
                "automation for the browser was denied, so no tab will be captured until it is granted in System Settings"
            );
        }),
        ScriptError::NoFrontWindow => tracing::debug!("the browser has no front window"),
        ScriptError::AppleEventTimeout => {
            tracing::debug!("the browser did not answer the apple event in time")
        }
        ScriptError::Unknown(code) => {
            tracing::debug!(?code, "the browser tab script failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURED: &str = include_str!("../../fixtures/dia_active_tab.txt");

    fn failed(stderr: &str) -> CommandOutput {
        CommandOutput {
            succeeded: false,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    fn joined(fields: [&str; 4]) -> String {
        fields.join("\u{1f}")
    }

    #[test]
    fn the_captured_output_parses_into_all_four_fields() {
        let details = parse_tab(CAPTURED);
        assert_eq!(
            details["url"],
            "https://homeassistant.pkarpovich.space/home-v2/home"
        );
        assert_eq!(details["tab"], "Home – Home Assistant");
        assert_eq!(details["profile"], "MBP_21");
        assert_eq!(details["pinned"], Value::Bool(false));
    }

    #[test]
    fn a_title_containing_a_comma_leaves_the_url_intact() {
        let details = parse_tab(&joined([
            "https://linear.app/nikki/issue/ENG-1",
            "ENG-1: buffer, shipping, and redaction",
            "MBP_21",
            "true",
        ]));
        assert_eq!(details["url"], "https://linear.app/nikki/issue/ENG-1");
        assert_eq!(details["tab"], "ENG-1: buffer, shipping, and redaction");
        assert_eq!(details["pinned"], Value::Bool(true));
    }

    #[test]
    fn an_empty_result_means_the_browser_is_not_running() {
        assert!(parse_tab("").is_empty());
        assert!(parse_tab("\n").is_empty());
    }

    #[test]
    fn a_result_with_the_wrong_number_of_fields_is_dropped() {
        assert!(parse_tab("https://example.com/\u{1f}Example").is_empty());
        assert!(parse_tab("https://example.com/").is_empty());
    }

    #[test]
    fn an_empty_url_yields_nothing_even_when_the_other_fields_are_present() {
        assert!(parse_tab(&joined(["", "Example", "MBP_21", "false"])).is_empty());
    }

    #[test]
    fn an_empty_optional_field_is_left_out_rather_than_shipped_blank() {
        let details = parse_tab(&joined(["https://example.com/", "", "", "maybe"]));
        assert_eq!(details["url"], "https://example.com/");
        assert!(!details.contains_key("tab"));
        assert!(!details.contains_key("profile"));
        assert!(!details.contains_key("pinned"));
    }

    #[test]
    fn the_captured_error_form_yields_its_trailing_code() {
        let stderr = "220:224: execution error: Can't make {...} into type text. (-1700)\n";
        assert_eq!(script_error(stderr), ScriptError::Unknown(Some(-1700)));
    }

    #[test]
    fn each_known_error_code_is_recognised() {
        let cases = [
            (-1743, ScriptError::AutomationDenied),
            (-1728, ScriptError::NoFrontWindow),
            (-1712, ScriptError::AppleEventTimeout),
        ];
        for (code, expected) in cases {
            let stderr = format!("0:0: execution error: something went wrong. ({code})\n");
            assert_eq!(script_error(&stderr), expected);
        }
    }

    #[test]
    fn an_error_without_a_trailing_code_is_unknown() {
        assert_eq!(script_error(""), ScriptError::Unknown(None));
        assert_eq!(
            script_error("osascript: command not found\n"),
            ScriptError::Unknown(None)
        );
        assert_eq!(
            script_error("execution error: (not a number)\n"),
            ScriptError::Unknown(None)
        );
    }

    #[test]
    fn every_error_branch_yields_empty_details() {
        let stderrs = [
            "0:0: execution error: Not authorised to send Apple events to Dia. (-1743)\n",
            "0:0: execution error: Dia got an error: Can't get front window. (-1728)\n",
            "0:0: execution error: AppleEvent timed out. (-1712)\n",
            "220:224: execution error: Can't make {...} into type text. (-1700)\n",
            "something else entirely\n",
        ];
        for stderr in stderrs {
            for application in [Application::Dia, Application::Ardia] {
                assert!(
                    details_from(application, failed(stderr)).is_empty(),
                    "stderr was {stderr}"
                );
            }
        }
    }

    #[test]
    fn the_automation_warning_is_tracked_per_browser() {
        report(Application::Ardia, ScriptError::AutomationDenied);
        report(Application::Ardia, ScriptError::AutomationDenied);
        assert!(ARDIA_AUTOMATION_DENIED_WARNING.is_completed());
        report(Application::Dia, ScriptError::AutomationDenied);
        assert!(DIA_AUTOMATION_DENIED_WARNING.is_completed());
    }

    #[test]
    fn the_script_guards_against_launching_the_browser_before_it_talks_to_it() {
        let script = script(Application::Dia);
        let first = script.lines().next().expect("the script has a first line");
        assert_eq!(
            first,
            r#"if not (running of application "Dia") then return """#
        );
        assert!(script.contains("ASCII character 31"));
    }

    #[test]
    fn the_dia_script_keeps_its_text() {
        let expected = r#"if not (running of application "Dia") then return ""
tell application "Dia"
  set w to front window
  set t to active tab of w
  set u to (URL of t) as text
  set ti to (title of t) as text
  set pr to (name of active profile of w) as text
  set pn to (isPinned of t) as text
end tell
set AppleScript's text item delimiters to (ASCII character 31)
return {u, ti, pr, pn} as text"#;
        assert_eq!(script(Application::Dia), expected);
    }

    #[test]
    fn the_ardia_script_talks_only_to_ardia() {
        let script = script(Application::Ardia);
        assert!(script.contains(r#"tell application "Ardia""#));
        assert!(script.contains(r#"running of application "Ardia""#));
        assert!(!script.contains(r#""Dia""#));
    }
}
