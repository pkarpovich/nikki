use std::collections::{HashMap, HashSet};

use serde_json::Value;
use url::Url;

use crate::config::{Keep, RedactField, RedactRule};

pub const WILDCARD_HOST: &str = "*";

const DROPPED_WITH_TITLE: [&str; 2] = ["tab", "command"];

pub struct Redactor {
    fallback: Keep,
    hosts: HashMap<String, Keep>,
    titleless_bundles: HashSet<String>,
}

impl Redactor {
    pub fn new(rules: &[RedactRule]) -> Redactor {
        let mut fallback = Keep::Host;
        let mut hosts = HashMap::new();
        let mut titleless_bundles = HashSet::new();

        for RedactRule {
            url_host,
            keep,
            bundle_id,
            drop,
        } in rules
        {
            if let Some(url_host) = url_host {
                let keep = keep.unwrap_or(Keep::Host);
                if url_host == WILDCARD_HOST {
                    fallback = keep;
                } else {
                    hosts.insert(url_host.to_lowercase(), keep);
                }
            }
            let Some(bundle_id) = bundle_id else {
                continue;
            };
            for field in drop {
                match field {
                    RedactField::Title => {
                        titleless_bundles.insert(bundle_id.clone());
                    }
                }
            }
        }

        Redactor {
            fallback,
            hosts,
            titleless_bundles,
        }
    }

    pub fn apply(&self, payload: &mut Value) {
        let Some(payload) = payload.as_object_mut() else {
            return;
        };

        let drops_title = self.drops_title(payload.get("bundle_id"));

        self.redact_url(payload.get_mut("url"));
        if let Some(details) = payload.get_mut("details")
            && let Some(details) = details.as_object_mut()
        {
            self.redact_url(details.get_mut("url"));
            if drops_title {
                for key in DROPPED_WITH_TITLE {
                    if let Some(slot) = details.get_mut(key) {
                        *slot = Value::Null;
                    }
                }
            }
        }

        if drops_title {
            payload.insert("title".to_string(), Value::Null);
        }

        let Some(visible) = payload.get_mut("visible") else {
            return;
        };
        let Some(visible) = visible.as_array_mut() else {
            return;
        };
        for entry in visible {
            let Some(entry) = entry.as_object_mut() else {
                continue;
            };
            if !self.drops_title(entry.get("bundle_id")) {
                continue;
            }
            entry.insert("title".to_string(), Value::Null);
        }
    }

    fn redact_url(&self, slot: Option<&mut Value>) {
        let Some(slot) = slot else {
            return;
        };
        let Some(value) = slot.as_str() else {
            return;
        };
        *slot = Value::String(self.reduce(value));
    }

    fn reduce(&self, value: &str) -> String {
        let Ok(url) = Url::parse(value) else {
            tracing::debug!("a url that does not parse is redacted away entirely");
            return String::new();
        };
        let scheme = url.scheme();
        let host: &str = url.host_str().unwrap_or_default();
        if host.is_empty() {
            if url.cannot_be_a_base() {
                return format!("{scheme}:");
            }
            return format!("{scheme}:///");
        }

        let keep = match self.hosts.get(&host.to_lowercase()) {
            Some(keep) => *keep,
            None => self.fallback,
        };
        match keep {
            Keep::Full => value.to_string(),
            Keep::Host => match url.port() {
                Some(port) => format!("{scheme}://{host}:{port}/"),
                None => format!("{scheme}://{host}/"),
            },
        }
    }

    fn drops_title(&self, bundle_id: Option<&Value>) -> bool {
        let Some(bundle_id) = bundle_id else {
            return false;
        };
        let Some(bundle_id) = bundle_id.as_str() else {
            return false;
        };
        self.titleless_bundles.contains(bundle_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SLACK: &str = "com.tinyspeck.slackmacgap";

    fn host_only() -> Redactor {
        Redactor::new(&[RedactRule {
            url_host: Some(WILDCARD_HOST.to_string()),
            keep: Some(Keep::Host),
            bundle_id: None,
            drop: Vec::new(),
        }])
    }

    fn full_configured() -> Redactor {
        Redactor::new(&[
            RedactRule {
                url_host: Some(WILDCARD_HOST.to_string()),
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
                bundle_id: Some(SLACK.to_string()),
                drop: vec![RedactField::Title],
            },
        ])
    }

    fn reduced(redactor: &Redactor, url: &str) -> String {
        let mut payload = json!({"url": url});
        redactor.apply(&mut payload);
        payload["url"]
            .as_str()
            .expect("the url survives as a string")
            .to_string()
    }

    #[test]
    fn the_default_rule_keeps_the_host_and_drops_the_path_and_query() {
        assert_eq!(
            reduced(
                &host_only(),
                "https://example.com/teams/eng/issue?token=secret#anchor"
            ),
            "https://example.com/"
        );
    }

    #[test]
    fn a_per_host_opt_in_keeps_the_whole_url() {
        let full = "https://linear.app/nikki/issue/ENG-1/some-title?tab=activity";
        assert_eq!(reduced(&full_configured(), full), full);
    }

    #[test]
    fn an_opt_in_for_one_host_leaves_every_other_host_reduced() {
        assert_eq!(
            reduced(&full_configured(), "https://example.com/private/path"),
            "https://example.com/"
        );
    }

    #[test]
    fn the_host_match_ignores_case() {
        let full = "https://LINEAR.app/nikki/issue/ENG-1";
        assert_eq!(reduced(&full_configured(), full), full);
    }

    #[test]
    fn the_port_is_part_of_the_host_and_is_kept() {
        assert_eq!(
            reduced(&host_only(), "http://localhost:3000/admin?token=secret"),
            "http://localhost:3000/"
        );
    }

    #[test]
    fn an_ipv6_host_keeps_the_brackets_that_make_it_a_url_again() {
        let reduced = reduced(&host_only(), "http://[::1]:3000/admin?token=secret");
        assert_eq!(reduced, "http://[::1]:3000/");
        assert_eq!(
            Url::parse(&reduced)
                .expect("the reduction parses back")
                .host(),
            Url::parse("http://[::1]/")
                .expect("the literal parses")
                .host()
        );
    }

    #[test]
    fn a_default_port_is_not_invented() {
        assert_eq!(
            reduced(&host_only(), "https://example.com:443/path"),
            "https://example.com/"
        );
    }

    #[test]
    fn a_file_url_is_reduced_to_its_scheme_rather_than_falling_through() {
        assert_eq!(
            reduced(&host_only(), "file:///Users/pavel.karpovich/secret.pdf"),
            "file:///"
        );
    }

    #[test]
    fn a_data_url_is_reduced_to_its_scheme() {
        assert_eq!(
            reduced(&host_only(), "data:text/plain;base64,c2VjcmV0"),
            "data:"
        );
    }

    #[test]
    fn a_chrome_extension_url_keeps_its_extension_id_and_loses_its_path() {
        assert_eq!(
            reduced(
                &host_only(),
                "chrome-extension://gighmmpiobklfepjocnamgkkbiglidom/options.html?tab=1"
            ),
            "chrome-extension://gighmmpiobklfepjocnamgkkbiglidom/"
        );
    }

    #[test]
    fn credentials_do_not_survive_a_host_only_reduction() {
        assert_eq!(
            reduced(&host_only(), "https://user:hunter2@example.com/path"),
            "https://example.com/"
        );
    }

    #[test]
    fn a_url_that_does_not_parse_is_emptied_rather_than_passed_through() {
        assert_eq!(reduced(&host_only(), "example.com/private/path"), "");
    }

    #[test]
    fn a_document_path_is_never_treated_as_a_url() {
        let mut payload = json!({
            "app": "Zed",
            "path": "file:///Users/pavel.karpovich/Projects/nikki/src/runtime/redact.rs",
        });
        host_only().apply(&mut payload);
        assert_eq!(
            payload["path"],
            "file:///Users/pavel.karpovich/Projects/nikki/src/runtime/redact.rs"
        );
    }

    #[test]
    fn a_url_inside_details_is_redacted_like_a_top_level_one() {
        let mut payload = json!({
            "app": "Dia",
            "details": {"url": "https://example.com/inbox/secret", "profile": "MBP_21"},
        });
        host_only().apply(&mut payload);
        assert_eq!(payload["details"]["url"], "https://example.com/");
        assert_eq!(payload["details"]["profile"], "MBP_21");
    }

    #[test]
    fn dropping_a_title_covers_the_focused_window_and_every_visible_entry() {
        let mut payload = json!({
            "app": "Slack",
            "bundle_id": SLACK,
            "title": "nikki - private channel",
            "visible": [
                {"app": "Slack", "bundle_id": SLACK, "title": "another private channel", "z": 0},
                {"app": "Dia", "bundle_id": "company.thebrowser.dia", "title": "Home Assistant", "z": 1},
            ],
        });
        full_configured().apply(&mut payload);
        assert_eq!(payload["title"], Value::Null);
        assert_eq!(payload["visible"][0]["title"], Value::Null);
        assert_eq!(payload["visible"][1]["title"], "Home Assistant");
    }

    #[test]
    fn dropping_a_title_covers_the_browser_tab_title_in_details() {
        let dia = "company.thebrowser.dia";
        let redactor = Redactor::new(&[RedactRule {
            url_host: None,
            keep: None,
            bundle_id: Some(dia.to_string()),
            drop: vec![RedactField::Title],
        }]);
        let mut payload = json!({
            "app": "Dia",
            "bundle_id": dia,
            "title": "Home – Home Assistant",
            "details": {"url": "https://example.com/private", "tab": "Home – Home Assistant", "profile": "MBP_21"},
        });
        redactor.apply(&mut payload);
        assert_eq!(payload["title"], Value::Null);
        assert_eq!(payload["details"]["tab"], Value::Null);
        assert_eq!(payload["details"]["profile"], "MBP_21");
    }

    #[test]
    fn dropping_a_title_covers_the_terminal_command_line_in_details() {
        let agterm = "com.umputun.agterm";
        let redactor = Redactor::new(&[RedactRule {
            url_host: None,
            keep: None,
            bundle_id: Some(agterm.to_string()),
            drop: vec![RedactField::Title],
        }]);
        let mut payload = json!({
            "app": "agterm",
            "bundle_id": agterm,
            "title": "nikki: revdiff",
            "details": {
                "workspace": "nikki",
                "session": "nikki daemon",
                "surface": "scratch",
                "command": "psql postgres://user:hunter2@example.com/db",
                "cwd": "/Users/pavel.karpovich/Projects/nikki",
            },
        });
        redactor.apply(&mut payload);
        assert_eq!(payload["title"], Value::Null);
        assert_eq!(payload["details"]["command"], Value::Null);
        assert_eq!(payload["details"]["surface"], "scratch");
    }

    #[test]
    fn a_bundle_without_a_drop_rule_keeps_its_command_line() {
        let mut payload = json!({
            "app": "agterm",
            "bundle_id": "com.umputun.agterm",
            "details": {"command": "rx plan.md"},
        });
        full_configured().apply(&mut payload);
        assert_eq!(payload["details"]["command"], "rx plan.md");
    }

    #[test]
    fn a_bundle_without_a_drop_rule_keeps_its_tab_title() {
        let mut payload = json!({
            "app": "Dia",
            "bundle_id": "company.thebrowser.dia",
            "details": {"tab": "Home – Home Assistant"},
        });
        full_configured().apply(&mut payload);
        assert_eq!(payload["details"]["tab"], "Home – Home Assistant");
    }

    #[test]
    fn a_bundle_without_a_drop_rule_keeps_its_title() {
        let mut payload = json!({
            "app": "Zed",
            "bundle_id": "dev.zed.Zed",
            "title": "nikki - redact.rs",
        });
        full_configured().apply(&mut payload);
        assert_eq!(payload["title"], "nikki - redact.rs");
    }

    #[test]
    fn an_absent_wildcard_rule_still_defaults_to_host_only() {
        let redactor = Redactor::new(&[RedactRule {
            url_host: Some("linear.app".to_string()),
            keep: Some(Keep::Full),
            bundle_id: None,
            drop: Vec::new(),
        }]);
        assert_eq!(
            reduced(&redactor, "https://example.com/private/path"),
            "https://example.com/"
        );
    }

    #[test]
    fn a_wildcard_opt_in_keeps_every_url_whole() {
        let redactor = Redactor::new(&[RedactRule {
            url_host: Some(WILDCARD_HOST.to_string()),
            keep: Some(Keep::Full),
            bundle_id: None,
            drop: Vec::new(),
        }]);
        let full = "https://example.com/private/path?token=secret";
        assert_eq!(reduced(&redactor, full), full);
    }

    #[test]
    fn a_payload_without_any_redactable_field_is_untouched() {
        let mut payload = json!({"app": "Zed", "display": 1, "idle_sec": 3});
        let before = payload.clone();
        full_configured().apply(&mut payload);
        assert_eq!(payload, before);
    }
}
