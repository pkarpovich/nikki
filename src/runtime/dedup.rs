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
