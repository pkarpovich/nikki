use url::Url;

use crate::macos::ax::AxWindow;

pub fn document_path(window: &AxWindow) -> Option<String> {
    let value = window.document()?;
    file_url(&value)
}

pub fn file_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let Ok(url) = Url::parse(value) else {
        tracing::debug!("an accessibility document value that is not a url is dropped");
        return None;
    };
    if url.scheme() != "file" {
        return None;
    }
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_url_survives_whole() {
        let path =
            "file:///Users/pavel.karpovich/Projects/DC/ask-dealcloud/agent-sdk-runtime/main.py";
        assert_eq!(file_url(path), Some(path.to_string()));
    }

    #[test]
    fn a_repository_root_survives_whole() {
        let path = "file:///Users/pavel.karpovich/Projects/nikki/";
        assert_eq!(file_url(path), Some(path.to_string()));
    }

    #[test]
    fn the_empty_string_electron_returns_is_dropped() {
        assert_eq!(file_url(""), None);
        assert_eq!(file_url("   "), None);
    }

    #[test]
    fn an_applications_own_internal_https_url_is_dropped() {
        assert_eq!(
            file_url("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC"),
            None
        );
    }

    #[test]
    fn a_bare_path_without_a_scheme_is_dropped() {
        assert_eq!(
            file_url("/Users/pavel.karpovich/Projects/nikki/Cargo.toml"),
            None
        );
    }

    #[test]
    fn surrounding_whitespace_does_not_defeat_the_scheme_check() {
        assert_eq!(
            file_url("  file:///tmp/notes.md\n"),
            Some("file:///tmp/notes.md".to_string())
        );
    }
}
