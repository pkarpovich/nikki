use std::path::{Path, PathBuf};
use std::process::Command;

/// The launchd label of the agent `nikki install` writes.
pub const LABEL: &str = "dev.pkarpovich.nikki";

/// The label Homebrew's `brew services` used for the same binary.
pub const BREW_LABEL: &str = "homebrew.mxcl.nikki";

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("HOME is not set, so the service paths cannot be resolved")]
    NoHome,
    #[error("the running binary could not be located: {source}")]
    Executable { source: std::io::Error },
    #[error("{} could not be created: {source}", path.display())]
    Directory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} could not be written: {source}", path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} could not be removed: {source}", path.display())]
    Remove {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("launchctl could not be run: {source}")]
    Launchctl { source: std::io::Error },
    #[error("launchctl refused to load {}", path.display())]
    Bootstrap { path: PathBuf },
}

/// Whether the running binary sits inside an application bundle.
///
/// It decides whether a macOS permission granted to it survives an upgrade: a bundle is identified
/// by its bundle id at a path that does not move, while a loose binary is identified by its path
/// alone, and Homebrew gives every version a path of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Housing {
    Bundle,
    Loose,
}

/// Where the agent, its logs and the agent Homebrew used to write live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub agent: PathBuf,
    pub log: PathBuf,
    pub errors: PathBuf,
    pub brew_agent: PathBuf,
}

/// What `install` loaded, for the caller to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub program: PathBuf,
    pub agent: PathBuf,
    pub housing: Housing,
}

/// Resolves the agent layout under a home directory.
pub fn layout(home: &Path) -> Layout {
    let agents = home.join("Library").join("LaunchAgents");
    let logs = home.join("Library").join("Logs");
    Layout {
        agent: agents.join(format!("{LABEL}.plist")),
        log: logs.join("nikki.log"),
        errors: logs.join("nikki.err.log"),
        brew_agent: agents.join(format!("{BREW_LABEL}.plist")),
    }
}

/// Loads the running binary as a launchd agent, replacing the one Homebrew's services wrote.
pub fn install() -> Result<Installed, ServiceError> {
    let home = home_dir()?;
    let layout = layout(&home);
    let program = std::env::current_exe().map_err(|source| ServiceError::Executable { source })?;

    unload(BREW_LABEL)?;
    remove_file(&layout.brew_agent)?;
    unload(LABEL)?;

    let Layout {
        agent, log, errors, ..
    } = &layout;
    create_dir(parent_of(agent))?;
    create_dir(parent_of(log))?;

    let contents = agent_plist(&program, log, errors);
    std::fs::write(agent, contents).map_err(|source| ServiceError::Write {
        path: agent.clone(),
        source,
    })?;

    bootstrap(agent)?;
    Ok(Installed {
        program: program.clone(),
        agent: agent.clone(),
        housing: housing(&program),
    })
}

/// Unloads the agent and removes it, leaving the binary and the captured data alone.
pub fn uninstall() -> Result<Layout, ServiceError> {
    let home = home_dir()?;
    let layout = layout(&home);

    unload(LABEL)?;
    remove_file(&layout.agent)?;
    Ok(layout)
}

fn housing(program: &Path) -> Housing {
    for component in program.ancestors() {
        let Some(name) = component.file_name() else {
            continue;
        };
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.ends_with(".app") {
            return Housing::Bundle;
        }
    }
    Housing::Loose
}

fn home_dir() -> Result<PathBuf, ServiceError> {
    let Some(home) = std::env::var_os("HOME") else {
        return Err(ServiceError::NoHome);
    };
    Ok(PathBuf::from(home))
}

fn parent_of(path: &Path) -> &Path {
    let Some(parent) = path.parent() else {
        return path;
    };
    parent
}

fn create_dir(path: &Path) -> Result<(), ServiceError> {
    std::fs::create_dir_all(path).map_err(|source| ServiceError::Directory {
        path: path.to_path_buf(),
        source,
    })
}

fn remove_file(path: &Path) -> Result<(), ServiceError> {
    let removed = std::fs::remove_file(path);
    let Err(source) = removed else {
        return Ok(());
    };
    if source.kind() == std::io::ErrorKind::NotFound {
        return Ok(());
    }
    Err(ServiceError::Remove {
        path: path.to_path_buf(),
        source,
    })
}

fn unload(label: &str) -> Result<(), ServiceError> {
    let target = format!("gui/{}/{label}", uid());
    let status = Command::new("launchctl")
        .args(["bootout", &target])
        .status()
        .map_err(|source| ServiceError::Launchctl { source })?;
    tracing::debug!(label, code = status.code(), "asked launchctl to unload");
    Ok(())
}

fn bootstrap(agent: &Path) -> Result<(), ServiceError> {
    let target = format!("gui/{}", uid());
    let Some(agent_arg) = agent.to_str() else {
        return Err(ServiceError::Bootstrap {
            path: agent.to_path_buf(),
        });
    };
    let status = Command::new("launchctl")
        .args(["bootstrap", &target, agent_arg])
        .status()
        .map_err(|source| ServiceError::Launchctl { source })?;
    if status.success() {
        return Ok(());
    }
    Err(ServiceError::Bootstrap {
        path: agent.to_path_buf(),
    })
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn agent_plist(program: &Path, log: &Path, errors: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{program}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{errors}</string>
</dict>
</plist>
"#,
        label = LABEL,
        program = escape(&program.display().to_string()),
        log = escape(&log.display().to_string()),
        errors = escape(&errors.display().to_string()),
    )
}

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_and_its_logs_live_under_the_home_directory() {
        let Layout {
            agent,
            log,
            errors,
            brew_agent,
        } = layout(Path::new("/Users/tester"));
        assert_eq!(
            agent,
            Path::new("/Users/tester/Library/LaunchAgents/dev.pkarpovich.nikki.plist")
        );
        assert_eq!(log, Path::new("/Users/tester/Library/Logs/nikki.log"));
        assert_eq!(
            errors,
            Path::new("/Users/tester/Library/Logs/nikki.err.log")
        );
        assert_eq!(
            brew_agent,
            Path::new("/Users/tester/Library/LaunchAgents/homebrew.mxcl.nikki.plist")
        );
    }

    #[test]
    fn a_binary_inside_an_application_bundle_keeps_its_permissions() {
        assert_eq!(
            housing(Path::new("/Applications/Nikki.app/Contents/MacOS/nikki")),
            Housing::Bundle
        );
    }

    #[test]
    fn a_binary_in_the_homebrew_cellar_does_not() {
        assert_eq!(
            housing(Path::new("/opt/homebrew/Cellar/nikki/0.3.0/bin/nikki")),
            Housing::Loose
        );
        assert_eq!(housing(Path::new("target/debug/nikki")), Housing::Loose);
    }

    #[test]
    fn a_directory_merely_containing_app_is_not_a_bundle() {
        assert_eq!(
            housing(Path::new("/Users/tester/apps/nikki/bin/nikki")),
            Housing::Loose
        );
    }

    #[test]
    fn the_agent_runs_the_program_it_was_given() {
        let plist = agent_plist(
            Path::new("/Applications/Nikki.app/Contents/MacOS/nikki"),
            Path::new("/Users/tester/Library/Logs/nikki.log"),
            Path::new("/Users/tester/Library/Logs/nikki.err.log"),
        );
        assert!(plist.contains("<string>dev.pkarpovich.nikki</string>"));
        assert!(plist.contains("<string>/Applications/Nikki.app/Contents/MacOS/nikki</string>"));
        assert!(plist.contains("<string>/Users/tester/Library/Logs/nikki.log</string>"));
        assert!(!plist.contains("Cellar"));
    }

    #[test]
    fn a_path_that_needs_escaping_still_yields_a_parsable_agent() {
        let plist = agent_plist(
            Path::new("/Users/a&b/Nikki.app/Contents/MacOS/nikki"),
            Path::new("/Users/a&b/log"),
            Path::new("/Users/a&b/err"),
        );
        assert!(plist.contains("/Users/a&amp;b/Nikki.app/Contents/MacOS/nikki"));
        assert!(!plist.contains("/Users/a&b/"));
    }

    #[test]
    fn escaping_leaves_an_ordinary_path_untouched() {
        assert_eq!(escape("/Applications/Nikki.app"), "/Applications/Nikki.app");
        assert_eq!(escape("a<b>c&d"), "a&lt;b&gt;c&amp;d");
    }

    #[test]
    fn removing_a_file_that_was_never_there_is_not_a_failure() {
        let directory = std::env::temp_dir().join("nikki-service-test");
        assert!(remove_file(&directory.join("absent")).is_ok());
    }
}
