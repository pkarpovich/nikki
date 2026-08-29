use std::path::{Path, PathBuf};
use std::process::Command;

/// The launchd label of the agent `nikki install` writes.
pub const LABEL: &str = "dev.pkarpovich.nikki";

/// The label Homebrew's `brew services` uses for the same binary.
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
    #[error("{} could not be copied to {}: {source}", from.display(), to.display())]
    Copy {
        from: PathBuf,
        to: PathBuf,
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

/// Where an installed nikki keeps its binary, its agent and its log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub binary: PathBuf,
    pub agent: PathBuf,
    pub log: PathBuf,
    pub errors: PathBuf,
    pub brew_agent: PathBuf,
}

/// Resolves the installed layout under a home directory.
pub fn layout(home: &Path) -> Layout {
    let agents = home.join("Library").join("LaunchAgents");
    let logs = home.join("Library").join("Logs");
    Layout {
        binary: home
            .join("Library")
            .join("Application Support")
            .join("nikki")
            .join("bin")
            .join("nikki"),
        agent: agents.join(format!("{LABEL}.plist")),
        log: logs.join("nikki.log"),
        errors: logs.join("nikki.err.log"),
        brew_agent: agents.join(format!("{BREW_LABEL}.plist")),
    }
}

/// Copies the running binary to a stable path and loads it as a launchd agent.
///
/// The copy is the whole point: macOS ties an Accessibility grant to the resolved path of the
/// program, and Homebrew's own agent points into a versioned Cellar directory, so every upgrade
/// asks for the permission again.
pub fn install() -> Result<Layout, ServiceError> {
    let home = home_dir()?;
    let layout = layout(&home);
    let running = std::env::current_exe().map_err(|source| ServiceError::Executable { source })?;

    unload(BREW_LABEL)?;
    remove_file(&layout.brew_agent)?;
    unload(LABEL)?;

    let Layout {
        binary, agent, log, ..
    } = &layout;
    create_dir(parent_of(binary))?;
    create_dir(parent_of(agent))?;
    create_dir(parent_of(log))?;

    remove_file(binary)?;
    std::fs::copy(&running, binary).map_err(|source| ServiceError::Copy {
        from: running,
        to: binary.clone(),
        source,
    })?;

    let contents = agent_plist(&layout);
    std::fs::write(agent, contents).map_err(|source| ServiceError::Write {
        path: agent.clone(),
        source,
    })?;

    bootstrap(agent)?;
    Ok(layout)
}

/// Unloads the agent and removes what `install` wrote, leaving captured data alone.
pub fn uninstall() -> Result<Layout, ServiceError> {
    let home = home_dir()?;
    let layout = layout(&home);

    unload(LABEL)?;
    remove_file(&layout.agent)?;
    remove_file(&layout.binary)?;
    Ok(layout)
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

fn agent_plist(layout: &Layout) -> String {
    let Layout {
        binary,
        log,
        errors,
        ..
    } = layout;
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{binary}</string>
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
        binary = escape(&binary.display().to_string()),
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
    fn the_layout_keeps_the_binary_outside_the_homebrew_prefix() {
        let Layout {
            binary,
            agent,
            log,
            errors,
            brew_agent,
        } = layout(Path::new("/Users/tester"));
        assert_eq!(
            binary,
            Path::new("/Users/tester/Library/Application Support/nikki/bin/nikki")
        );
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
    fn the_agent_runs_the_copy_and_never_the_homebrew_path() {
        let plist = agent_plist(&layout(Path::new("/Users/tester")));
        assert!(plist.contains("<string>dev.pkarpovich.nikki</string>"));
        assert!(plist.contains(
            "<string>/Users/tester/Library/Application Support/nikki/bin/nikki</string>"
        ));
        assert!(plist.contains("<string>/Users/tester/Library/Logs/nikki.log</string>"));
        assert!(!plist.contains("Cellar"));
        assert!(!plist.contains("/opt/homebrew"));
    }

    #[test]
    fn a_home_directory_that_needs_escaping_still_yields_a_parsable_agent() {
        let plist = agent_plist(&layout(Path::new("/Users/a&b")));
        assert!(plist.contains("/Users/a&amp;b/Library/Application Support/nikki/bin/nikki"));
        assert!(!plist.contains("/Users/a&b/"));
    }

    #[test]
    fn escaping_leaves_an_ordinary_path_untouched() {
        assert_eq!(escape("/Users/tester/Library"), "/Users/tester/Library");
        assert_eq!(escape("a<b>c&d"), "a&lt;b&gt;c&amp;d");
    }

    #[test]
    fn removing_a_file_that_was_never_there_is_not_a_failure() {
        let directory = std::env::temp_dir().join("nikki-service-test");
        assert!(remove_file(&directory.join("absent")).is_ok());
    }
}
