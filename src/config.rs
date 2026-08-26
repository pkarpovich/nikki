use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use url::Url;

const TICK_INTERVAL_MIN: i64 = 1;
const TICK_INTERVAL_MAX: i64 = 3600;

const DEFAULT_CONFIG_RELATIVE: &str = ".config/nikki/config.toml";
const DEFAULT_STATE_RELATIVE: &str = "Library/Application Support/nikki";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("neither {var} nor HOME is set, so the {what} path cannot be resolved")]
    NoHome {
        var: &'static str,
        what: &'static str,
    },
    #[error("config file {} could not be read: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("config file {} is not valid toml: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("config field `{field}` is required and has no default")]
    Missing { field: &'static str },
    #[error("config field `{field}` is invalid: {reason}")]
    Invalid { field: &'static str, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Paths, ConfigError> {
        Paths::resolve(
            var("NIKKI_CONFIG"),
            var("NIKKI_STATE_DIR"),
            var("HOME").map(PathBuf::from),
        )
    }

    fn resolve(
        config: Option<String>,
        state_dir: Option<String>,
        home: Option<PathBuf>,
    ) -> Result<Paths, ConfigError> {
        let config = match config {
            Some(config) => PathBuf::from(config),
            None => {
                let Some(home) = home.clone() else {
                    return Err(ConfigError::NoHome {
                        var: "NIKKI_CONFIG",
                        what: "config file",
                    });
                };
                home.join(DEFAULT_CONFIG_RELATIVE)
            }
        };
        let state_dir = match state_dir {
            Some(state_dir) => PathBuf::from(state_dir),
            None => {
                let Some(home) = home else {
                    return Err(ConfigError::NoHome {
                        var: "NIKKI_STATE_DIR",
                        what: "state directory",
                    });
                };
                home.join(DEFAULT_STATE_RELATIVE)
            }
        };
        Ok(Paths { config, state_dir })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub service_url: Url,
    pub device: String,
    pub tick_interval: u64,
    pub history_poll_interval: u64,
    pub revisit_window: u32,
    pub browser: Browser,
    pub buffer: Buffer,
    pub redact: Vec<RedactRule>,
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Browser {
    pub profile: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Buffer {
    #[serde(default = "default_max_rows")]
    pub max_rows: u64,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedactRule {
    pub url_host: Option<String>,
    pub keep: Option<Keep>,
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub drop: Vec<RedactField>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Keep {
    Host,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RedactField {
    Title,
}

pub fn load() -> Result<Config, ConfigError> {
    let paths = Paths::from_env()?;
    load_from(&paths)
}

pub fn load_from(paths: &Paths) -> Result<Config, ConfigError> {
    let Paths { config, state_dir } = paths;
    let text = match fs::read_to_string(config) {
        Ok(text) => text,
        Err(source) => {
            return Err(ConfigError::Read {
                path: config.clone(),
                source,
            });
        }
    };
    parse(&text, config, state_dir)
}

fn parse(text: &str, path: &Path, state_dir: &Path) -> Result<Config, ConfigError> {
    let file: FileConfig = match toml::from_str(text) {
        Ok(file) => file,
        Err(source) => {
            return Err(ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let FileConfig {
        service_url,
        device,
        tick_interval,
        history_poll_interval,
        revisit_window,
        browser,
        buffer,
        redact,
    } = file;
    let FileBrowser { profile } = browser;

    let service_url = required(service_url, "service_url")?;
    let service_url = service_url_from(&service_url)?;
    let device = required(device, "device")?;
    let profile = required(profile, "browser.profile")?;

    if !(TICK_INTERVAL_MIN..=TICK_INTERVAL_MAX).contains(&tick_interval) {
        return Err(ConfigError::Invalid {
            field: "tick_interval",
            reason: format!(
                "{tick_interval} is outside [{TICK_INTERVAL_MIN}, {TICK_INTERVAL_MAX}] seconds, and the service rejects such ticks"
            ),
        });
    }

    Ok(Config {
        service_url,
        device,
        tick_interval: tick_interval as u64,
        history_poll_interval,
        revisit_window,
        browser: Browser { profile },
        buffer,
        redact,
        state_dir: state_dir.to_path_buf(),
    })
}

fn required(value: Option<String>, field: &'static str) -> Result<String, ConfigError> {
    let Some(value) = value else {
        return Err(ConfigError::Missing { field });
    };
    if value.trim().is_empty() {
        return Err(ConfigError::Invalid {
            field,
            reason: "must not be empty".to_string(),
        });
    }
    Ok(value)
}

fn service_url_from(value: &str) -> Result<Url, ConfigError> {
    let field = "service_url";
    let Ok(url) = Url::parse(value) else {
        return Err(ConfigError::Invalid {
            field,
            reason: format!("`{value}` is not an absolute url"),
        });
    };
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(ConfigError::Invalid {
            field,
            reason: format!("scheme `{scheme}` is not http or https"),
        });
    }
    let Some(host) = url.host_str() else {
        return Err(ConfigError::Invalid {
            field,
            reason: format!("`{value}` has no host"),
        });
    };
    if host.is_empty() {
        return Err(ConfigError::Invalid {
            field,
            reason: format!("`{value}` has an empty host"),
        });
    }
    Ok(url)
}

fn var(key: &str) -> Option<String> {
    let Ok(value) = env::var(key) else {
        return None;
    };
    if value.is_empty() {
        return None;
    }
    Some(value)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    service_url: Option<String>,
    device: Option<String>,
    #[serde(default = "default_tick_interval")]
    tick_interval: i64,
    #[serde(default = "default_history_poll_interval")]
    history_poll_interval: u64,
    #[serde(default = "default_revisit_window")]
    revisit_window: u32,
    #[serde(default)]
    browser: FileBrowser,
    #[serde(default)]
    buffer: Buffer,
    #[serde(default = "default_redact")]
    redact: Vec<RedactRule>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileBrowser {
    profile: Option<String>,
}

impl Default for Buffer {
    fn default() -> Buffer {
        Buffer {
            max_rows: default_max_rows(),
            max_bytes: default_max_bytes(),
        }
    }
}

fn default_tick_interval() -> i64 {
    30
}

fn default_history_poll_interval() -> u64 {
    300
}

fn default_revisit_window() -> u32 {
    500
}

fn default_max_rows() -> u64 {
    200_000
}

fn default_max_bytes() -> u64 {
    536_870_912
}

fn default_redact() -> Vec<RedactRule> {
    vec![RedactRule {
        url_host: Some("*".to_string()),
        keep: Some(Keep::Host),
        bundle_id: None,
        drop: Vec::new(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "service_url = \"http://alpha:8080\"\ndevice = \"mbp-21\"\n";
    const BROWSER: &str = "\n[browser]\nprofile = \"MBP_21\"\n";

    fn parse_text(text: &str) -> Result<Config, ConfigError> {
        parse(
            text,
            Path::new("config.toml"),
            Path::new("/tmp/nikki-state"),
        )
    }

    fn minimal() -> String {
        format!("{HEAD}{BROWSER}")
    }

    fn with_line(line: &str) -> String {
        format!("{HEAD}{line}\n{BROWSER}")
    }

    #[test]
    fn defaults_apply_when_only_required_fields_are_present() {
        let config = parse_text(&minimal()).expect("minimal config parses");
        assert_eq!(config.tick_interval, 30);
        assert_eq!(config.history_poll_interval, 300);
        assert_eq!(config.revisit_window, 500);
        assert_eq!(config.buffer.max_rows, 200_000);
        assert_eq!(config.buffer.max_bytes, 536_870_912);
        assert_eq!(config.redact, default_redact());
        assert_eq!(config.state_dir, PathBuf::from("/tmp/nikki-state"));
        assert_eq!(config.service_url.as_str(), "http://alpha:8080/");
        assert_eq!(config.device, "mbp-21");
        assert_eq!(config.browser.profile, "MBP_21");
    }

    #[test]
    fn full_config_parses_every_field() {
        let text = r#"
service_url = "https://alpha.example.com:8443/ingest"
device = "mbp-21"
tick_interval = 45
history_poll_interval = 120
revisit_window = 250

[browser]
profile = "MBP_21"

[buffer]
max_rows = 1000
max_bytes = 2048

[[redact]]
url_host = "*"
keep = "host"

[[redact]]
url_host = "linear.app"
keep = "full"

[[redact]]
bundle_id = "com.tinyspeck.slackmacgap"
drop = ["title"]
"#;
        let config = parse_text(text).expect("full config parses");
        assert_eq!(
            config.service_url.as_str(),
            "https://alpha.example.com:8443/ingest"
        );
        assert_eq!(config.tick_interval, 45);
        assert_eq!(config.history_poll_interval, 120);
        assert_eq!(config.revisit_window, 250);
        assert_eq!(config.buffer.max_rows, 1000);
        assert_eq!(config.buffer.max_bytes, 2048);
        assert_eq!(
            config.redact,
            vec![
                RedactRule {
                    url_host: Some("*".to_string()),
                    keep: Some(Keep::Host),
                    bundle_id: None,
                    drop: Vec::new(),
                },
                RedactRule {
                    url_host: Some("linear.app".to_string()),
                    keep: Some(Keep::Full),
                    bundle_id: None,
                    drop: Vec::new(),
                },
                RedactRule {
                    url_host: None,
                    keep: None,
                    bundle_id: Some("com.tinyspeck.slackmacgap".to_string()),
                    drop: vec![RedactField::Title],
                },
            ]
        );
    }

    #[test]
    fn tick_interval_accepts_both_ends_of_the_bound() {
        let low = parse_text(&with_line("tick_interval = 1")).expect("lower bound accepted");
        assert_eq!(low.tick_interval, 1);
        let high = parse_text(&with_line("tick_interval = 3600")).expect("upper bound accepted");
        assert_eq!(high.tick_interval, 3600);
    }

    #[test]
    fn tick_interval_outside_the_bound_names_the_field() {
        for line in [
            "tick_interval = 0",
            "tick_interval = 3601",
            "tick_interval = -1",
        ] {
            let error = parse_text(&with_line(line)).expect_err("out of bounds is rejected");
            match error {
                ConfigError::Invalid { field, reason } => {
                    assert_eq!(field, "tick_interval");
                    assert!(reason.contains("[1, 3600]"), "reason was {reason}");
                }
                other => panic!("expected an invalid tick_interval, got {other}"),
            }
        }
    }

    #[test]
    fn each_required_field_is_reported_by_name_when_absent() {
        let cases = [
            (
                "service_url",
                "device = \"mbp-21\"\n[browser]\nprofile = \"MBP_21\"\n",
            ),
            (
                "device",
                "service_url = \"http://alpha:8080\"\n[browser]\nprofile = \"MBP_21\"\n",
            ),
            (
                "browser.profile",
                "service_url = \"http://alpha:8080\"\ndevice = \"mbp-21\"\n",
            ),
        ];
        for (field, text) in cases {
            let error = parse_text(text).expect_err("missing field is rejected");
            match error {
                ConfigError::Missing { field: named } => assert_eq!(named, field),
                other => panic!("expected `{field}` to be reported missing, got {other}"),
            }
        }
    }

    #[test]
    fn each_required_field_is_reported_by_name_when_empty() {
        let cases = [
            (
                "service_url",
                "service_url = \"\"\ndevice = \"mbp-21\"\n[browser]\nprofile = \"MBP_21\"\n",
            ),
            (
                "device",
                "service_url = \"http://alpha:8080\"\ndevice = \"\"\n[browser]\nprofile = \"MBP_21\"\n",
            ),
            (
                "browser.profile",
                "service_url = \"http://alpha:8080\"\ndevice = \"mbp-21\"\n[browser]\nprofile = \"   \"\n",
            ),
        ];
        for (field, text) in cases {
            let error = parse_text(text).expect_err("empty field is rejected");
            match error {
                ConfigError::Invalid { field: named, .. } => assert_eq!(named, field),
                other => panic!("expected `{field}` to be reported empty, got {other}"),
            }
        }
    }

    #[test]
    fn service_url_must_be_an_absolute_http_url() {
        let cases = [
            "service_url = \"alpha:8080\"",
            "service_url = \"/api/v1/records\"",
            "service_url = \"ftp://alpha:8080\"",
            "service_url = \"file:///tmp/records\"",
        ];
        for case in cases {
            let text = format!("{case}\ndevice = \"mbp-21\"\n[browser]\nprofile = \"MBP_21\"\n");
            let error = parse_text(&text).expect_err("non-http url is rejected");
            match error {
                ConfigError::Invalid { field, .. } => assert_eq!(field, "service_url"),
                other => panic!("expected `service_url` to be reported invalid, got {other}"),
            }
        }
    }

    #[test]
    fn an_unparseable_file_names_the_offending_key() {
        let error = parse_text("service_url = ").expect_err("broken toml is rejected");
        match error {
            ConfigError::Parse { path, source } => {
                assert_eq!(path, PathBuf::from("config.toml"));
                assert!(
                    source.to_string().contains("service_url"),
                    "message was {source}"
                );
            }
            other => panic!("expected a parse error, got {other}"),
        }
    }

    #[test]
    fn an_unknown_key_is_rejected_rather_than_ignored() {
        let error = parse_text(&with_line("tick_intrval = 30")).expect_err("a typo is rejected");
        match error {
            ConfigError::Parse { source, .. } => assert!(
                source.to_string().contains("tick_intrval"),
                "message was {source}"
            ),
            other => panic!("expected a parse error, got {other}"),
        }
    }

    #[test]
    fn a_missing_file_is_reported_with_its_path() {
        let paths = Paths {
            config: PathBuf::from("/nonexistent/nikki/config.toml"),
            state_dir: PathBuf::from("/tmp/nikki-state"),
        };
        let error = load_from(&paths).expect_err("a missing file is rejected");
        match error {
            ConfigError::Read { path, .. } => assert_eq!(path, paths.config),
            other => panic!("expected a read error, got {other}"),
        }
    }

    #[test]
    fn nikki_config_and_nikki_state_dir_override_the_defaults() {
        let paths = Paths::resolve(
            Some("/tmp/harness/config.toml".to_string()),
            Some("/tmp/harness/state".to_string()),
            Some(PathBuf::from("/Users/someone")),
        )
        .expect("overrides resolve");
        assert_eq!(paths.config, PathBuf::from("/tmp/harness/config.toml"));
        assert_eq!(paths.state_dir, PathBuf::from("/tmp/harness/state"));
    }

    #[test]
    fn without_overrides_both_paths_come_from_home() {
        let paths = Paths::resolve(None, None, Some(PathBuf::from("/Users/someone")))
            .expect("defaults resolve");
        assert_eq!(
            paths.config,
            PathBuf::from("/Users/someone/.config/nikki/config.toml")
        );
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/Users/someone/Library/Application Support/nikki")
        );
    }

    #[test]
    fn each_override_is_independent_of_the_other() {
        let paths = Paths::resolve(
            Some("/tmp/harness/config.toml".to_string()),
            None,
            Some(PathBuf::from("/Users/someone")),
        )
        .expect("one override resolves");
        assert_eq!(paths.config, PathBuf::from("/tmp/harness/config.toml"));
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/Users/someone/Library/Application Support/nikki")
        );
    }

    #[test]
    fn without_home_the_missing_variable_is_named() {
        let error = Paths::resolve(None, Some("/tmp/state".to_string()), None)
            .expect_err("no home is rejected");
        match error {
            ConfigError::NoHome { var, .. } => assert_eq!(var, "NIKKI_CONFIG"),
            other => panic!("expected a home error, got {other}"),
        }
    }
}
