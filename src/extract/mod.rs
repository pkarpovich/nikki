pub mod agterm;
pub mod dia;
pub mod document;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::process::Command;
use tokio::time::timeout;

pub const SUBPROCESS_DEADLINE: Duration = Duration::from_secs(2);

pub const DIA_BUNDLE_ID: &str = "company.thebrowser.dia";
pub const AGTERM_BUNDLE_ID: &str = "com.umputun.agterm";

pub type Details = Map<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extractor {
    Dia,
    Agterm,
}

impl Extractor {
    pub fn for_bundle_id(bundle_id: &str) -> Option<Extractor> {
        match bundle_id {
            DIA_BUNDLE_ID => Some(Extractor::Dia),
            AGTERM_BUNDLE_ID => Some(Extractor::Agterm),
            _ => None,
        }
    }

    pub async fn details(self) -> Details {
        match self {
            Extractor::Dia => dia::active_tab().await,
            Extractor::Agterm => agterm::active_session().await,
        }
    }
}

pub async fn details_for_focused(bundle_id: &str) -> Details {
    let Some(extractor) = Extractor::for_bundle_id(bundle_id) else {
        return Details::new();
    };
    extractor.details().await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub succeeded: bool,
    pub stdout: String,
    pub stderr: String,
}

pub async fn run_with_deadline(
    program: &Path,
    args: &[&str],
    deadline: Duration,
) -> Option<CommandOutput> {
    let program = program.to_path_buf();
    let mut command = Command::new(&program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::debug!(program = %program.display(), %error, "the extractor could not start");
            return None;
        }
    };

    let Ok(output) = timeout(deadline, child.wait_with_output()).await else {
        tracing::warn!(
            program = %program.display(),
            deadline_ms = deadline.as_millis(),
            "the extractor passed its deadline and was killed"
        );
        return None;
    };
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            tracing::debug!(program = %program.display(), %error, "the extractor did not complete");
            return None;
        }
    };

    Some(CommandOutput {
        succeeded: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn the_registry_answers_only_for_the_bundles_it_knows() {
        assert_eq!(
            Extractor::for_bundle_id(DIA_BUNDLE_ID),
            Some(Extractor::Dia)
        );
        assert_eq!(
            Extractor::for_bundle_id(AGTERM_BUNDLE_ID),
            Some(Extractor::Agterm)
        );
        assert_eq!(Extractor::for_bundle_id("dev.zed.Zed"), None);
        assert_eq!(Extractor::for_bundle_id(""), None);
    }

    #[tokio::test]
    async fn an_unregistered_focused_application_yields_no_details() {
        assert!(details_for_focused("dev.zed.Zed").await.is_empty());
    }

    #[tokio::test]
    async fn a_command_that_completes_carries_its_streams_and_status() {
        let output = run_with_deadline(
            Path::new("/bin/sh"),
            &["-c", "printf out; printf err 1>&2; exit 3"],
            SUBPROCESS_DEADLINE,
        )
        .await
        .expect("the command ran");
        assert_eq!(
            output,
            CommandOutput {
                succeeded: false,
                stdout: "out".to_string(),
                stderr: "err".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn a_missing_program_yields_nothing_rather_than_failing() {
        let output = run_with_deadline(
            Path::new("/nonexistent/agtermctl"),
            &["tree", "--json"],
            SUBPROCESS_DEADLINE,
        )
        .await;
        assert!(output.is_none());
    }

    #[tokio::test]
    async fn a_hung_subprocess_yields_nothing_within_its_deadline() {
        let deadline = Duration::from_millis(200);
        let started = Instant::now();
        let output = run_with_deadline(Path::new("/bin/sleep"), &["30"], deadline).await;
        let elapsed = started.elapsed();

        assert!(output.is_none());
        assert!(
            elapsed < Duration::from_secs(5),
            "the call blocked for {elapsed:?}"
        );
    }
}
