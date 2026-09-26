use std::fmt::Write;

use sha2::{Digest, Sha256};

use super::{Kind, Provider};

pub const UNIT_SEPARATOR: &str = "\u{1f}";
pub const KEY_HEX_CHARS: usize = 16;

pub fn windows_key(device: &str, kind: Kind, ts_millis: i64, seq: u64) -> String {
    key(&[
        device,
        Provider::Windows.as_str(),
        kind.as_str(),
        &ts_millis.to_string(),
        &seq.to_string(),
    ])
}

pub fn browser_key(device: &str, profile: &str, generation: u64, visit_id: i64) -> String {
    key(&[
        device,
        Provider::BrowserHistory.as_str(),
        profile,
        &generation.to_string(),
        &visit_id.to_string(),
    ])
}

pub fn claude_message_key(device: &str, session_id: &str, uuid: &str, block: u32) -> String {
    key(&[
        device,
        Provider::ClaudeCode.as_str(),
        Kind::Message.as_str(),
        session_id,
        uuid,
        &block.to_string(),
    ])
}

pub fn claude_session_key(device: &str, session_id: &str, field: &str, value: &str) -> String {
    key(&[
        device,
        Provider::ClaudeCode.as_str(),
        Kind::Session.as_str(),
        session_id,
        field,
        value,
    ])
}

pub fn claude_touch_key(
    device: &str,
    session_id: &str,
    uuid: &str,
    repo: &str,
    action: &str,
) -> String {
    key(&[
        device,
        Provider::ClaudeCode.as_str(),
        Kind::Touch.as_str(),
        session_id,
        uuid,
        repo,
        action,
    ])
}

pub fn key(fields: &[&str]) -> String {
    let digest = Sha256::digest(fields.join(UNIT_SEPARATOR).as_bytes());
    let mut hex = String::with_capacity(KEY_HEX_CHARS);
    for byte in &digest[..KEY_HEX_CHARS / 2] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected(joined: &str) -> String {
        let digest = Sha256::digest(joined.as_bytes());
        let mut hex = String::new();
        for byte in &digest[..8] {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    #[test]
    fn the_windows_key_hashes_the_unit_separator_joined_fields() {
        assert_eq!(
            windows_key("mbp-21", Kind::Tick, 1_787_666_152_481, 41_207),
            expected("mbp-21\u{1f}windows\u{1f}tick\u{1f}1787666152481\u{1f}41207")
        );
    }

    #[test]
    fn the_browser_key_hashes_the_unit_separator_joined_fields() {
        assert_eq!(
            browser_key("mbp-21", "MBP_21", 1, 929_269),
            expected("mbp-21\u{1f}browser_history\u{1f}MBP_21\u{1f}1\u{1f}929269")
        );
    }

    const SESSION: &str = "8f2c61d0-4b7e-4a51-9d3e-1c0b5e7a2f94";
    const UUID: &str = "d41f0c2a-7e93-4b6d-a8f1-5c2e90b7d316";

    #[test]
    fn the_claude_message_key_hashes_the_unit_separator_joined_fields() {
        assert_eq!(
            claude_message_key("mbp-21", SESSION, UUID, 0),
            expected(&format!(
                "mbp-21\u{1f}claude_code\u{1f}message\u{1f}{SESSION}\u{1f}{UUID}\u{1f}0"
            ))
        );
    }

    #[test]
    fn the_claude_session_key_hashes_the_unit_separator_joined_fields() {
        assert_eq!(
            claude_session_key("mbp-21", SESSION, "ai_title", "Redeploy dev via spot"),
            expected(&format!(
                "mbp-21\u{1f}claude_code\u{1f}session\u{1f}{SESSION}\u{1f}ai_title\u{1f}Redeploy dev via spot"
            ))
        );
    }

    const REPO: &str = "/Users/u/Projects/launchpad";

    #[test]
    fn the_claude_touch_key_hashes_the_unit_separator_joined_fields() {
        assert_eq!(
            claude_touch_key("mbp-21", SESSION, UUID, REPO, "edit"),
            expected(&format!(
                "mbp-21\u{1f}claude_code\u{1f}touch\u{1f}{SESSION}\u{1f}{UUID}\u{1f}{REPO}\u{1f}edit"
            ))
        );
    }

    #[test]
    fn every_claude_touch_field_changes_the_key() {
        let base = claude_touch_key("mbp-21", SESSION, UUID, REPO, "edit");
        let variants = [
            claude_touch_key("mba-22", SESSION, UUID, REPO, "edit"),
            claude_touch_key(
                "mbp-21",
                "0b9e4c71-2d6a-4f38-8e15-7a3c9d0f6b24",
                UUID,
                REPO,
                "edit",
            ),
            claude_touch_key(
                "mbp-21",
                SESSION,
                "5a7e2b91-c04d-4e63-9f18-2d6b0a8c4e75",
                REPO,
                "edit",
            ),
            claude_touch_key("mbp-21", SESSION, UUID, "/Users/u/Projects/nhop", "edit"),
            claude_touch_key("mbp-21", SESSION, UUID, REPO, "run"),
        ];
        for variant in variants {
            assert_ne!(base, variant);
        }
    }

    #[test]
    fn every_claude_message_field_changes_the_key() {
        let base = claude_message_key("mbp-21", SESSION, UUID, 0);
        let variants = [
            claude_message_key("mba-22", SESSION, UUID, 0),
            claude_message_key("mbp-21", "0b9e4c71-2d6a-4f38-8e15-7a3c9d0f6b24", UUID, 0),
            claude_message_key("mbp-21", SESSION, "5a7e2b91-c04d-4e63-9f18-2d6b0a8c4e75", 0),
            claude_message_key("mbp-21", SESSION, UUID, 1),
        ];
        for variant in variants {
            assert_ne!(base, variant);
        }
    }

    #[test]
    fn every_claude_session_field_changes_the_key() {
        let base = claude_session_key("mbp-21", SESSION, "ai_title", "Redeploy dev via spot");
        let variants = [
            claude_session_key("mba-22", SESSION, "ai_title", "Redeploy dev via spot"),
            claude_session_key(
                "mbp-21",
                "0b9e4c71-2d6a-4f38-8e15-7a3c9d0f6b24",
                "ai_title",
                "Redeploy dev via spot",
            ),
            claude_session_key("mbp-21", SESSION, "custom_title", "Redeploy dev via spot"),
            claude_session_key("mbp-21", SESSION, "ai_title", "feud"),
        ];
        for variant in variants {
            assert_ne!(base, variant);
        }
    }

    #[test]
    fn a_key_is_sixteen_lowercase_hex_characters() {
        let key = windows_key("mbp-21", Kind::Focus, 1_787_666_152_481, 7);
        assert_eq!(key.len(), KEY_HEX_CHARS);
        for character in key.chars() {
            assert!(
                character.is_ascii_hexdigit() && !character.is_ascii_uppercase(),
                "`{character}` is not a lowercase hex digit"
            );
        }
    }

    #[test]
    fn every_windows_field_changes_the_key() {
        let base = windows_key("mbp-21", Kind::Tick, 1_787_666_152_481, 41_207);
        let variants = [
            windows_key("mba-22", Kind::Tick, 1_787_666_152_481, 41_207),
            windows_key("mbp-21", Kind::Focus, 1_787_666_152_481, 41_207),
            windows_key("mbp-21", Kind::Tick, 1_787_666_152_482, 41_207),
            windows_key("mbp-21", Kind::Tick, 1_787_666_152_481, 41_208),
        ];
        for variant in variants {
            assert_ne!(base, variant);
        }
    }

    #[test]
    fn every_browser_field_changes_the_key() {
        let base = browser_key("mbp-21", "MBP_21", 1, 929_269);
        let variants = [
            browser_key("mba-22", "MBP_21", 1, 929_269),
            browser_key("mbp-21", "Intapp", 1, 929_269),
            browser_key("mbp-21", "MBP_21", 2, 929_269),
            browser_key("mbp-21", "MBP_21", 1, 929_270),
        ];
        for variant in variants {
            assert_ne!(base, variant);
        }
    }

    #[test]
    fn a_generation_of_one_is_present_rather_than_omitted() {
        assert_ne!(
            browser_key("mbp-21", "MBP_21", 1, 929_269),
            expected("mbp-21\u{1f}browser_history\u{1f}MBP_21\u{1f}929269")
        );
    }

    #[test]
    fn two_records_in_the_same_millisecond_differ_only_by_sequence() {
        let first = windows_key("mbp-21", Kind::Tick, 1_787_666_152_481, 41_207);
        let second = windows_key("mbp-21", Kind::Tick, 1_787_666_152_481, 41_208);
        assert_ne!(first, second);
    }
}
