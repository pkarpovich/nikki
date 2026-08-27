use std::collections::HashMap;
use std::ptr::null_mut;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessArgs {
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
}

#[expect(dead_code)]
pub fn read_args(pid: i32) -> Option<ProcessArgs> {
    let mut name = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    let probe = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            name.len() as u32,
            null_mut(),
            &raw mut size,
            null_mut(),
            0,
        )
    };
    if probe != 0 || size == 0 {
        return None;
    }

    let mut buffer = vec![0u8; size];
    let status = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            name.len() as u32,
            buffer.as_mut_ptr().cast(),
            &raw mut size,
            null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }

    buffer.truncate(size);
    Some(parse_procargs(&buffer))
}

fn parse_procargs(buffer: &[u8]) -> ProcessArgs {
    let mut args = ProcessArgs::default();
    if buffer.len() < 4 {
        return args;
    }

    let argc = i32::from_ne_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
    let mut cursor = 4;
    while cursor < buffer.len() && buffer[cursor] != 0 {
        cursor += 1;
    }
    while cursor < buffer.len() && buffer[cursor] == 0 {
        cursor += 1;
    }

    for _ in 0..argc.max(0) {
        let Some(argument) = next_string(buffer, &mut cursor) else {
            return args;
        };
        args.argv.push(argument);
    }

    loop {
        let Some(entry) = next_string(buffer, &mut cursor) else {
            return args;
        };
        if entry.is_empty() {
            return args;
        }
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        args.env.insert(key.to_string(), value.to_string());
    }
}

fn next_string(buffer: &[u8], cursor: &mut usize) -> Option<String> {
    let start = *cursor;
    if start >= buffer.len() {
        return None;
    }

    let mut end = start;
    while end < buffer.len() && buffer[end] != 0 {
        end += 1;
    }
    if end == buffer.len() {
        *cursor = buffer.len();
        return None;
    }

    *cursor = end + 1;
    Some(String::from_utf8_lossy(&buffer[start..end]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procargs(argc: i32, executable: &str, padding: usize, strings: &[&str]) -> Vec<u8> {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&argc.to_ne_bytes());
        buffer.extend_from_slice(executable.as_bytes());
        buffer.push(0);
        buffer.resize(buffer.len() + padding, 0);
        for value in strings {
            buffer.extend_from_slice(value.as_bytes());
            buffer.push(0);
        }
        buffer
    }

    #[test]
    fn argv_and_environment_are_both_recovered() {
        let buffer = procargs(
            2,
            "/usr/bin/rx",
            3,
            &["rx", "plan.md", "AGTERM_PANE=scratch", "SHELL=/bin/fish"],
        );

        let args = parse_procargs(&buffer);

        assert_eq!(args.argv, vec!["rx".to_string(), "plan.md".to_string()]);
        assert_eq!(args.env.get("AGTERM_PANE"), Some(&"scratch".to_string()));
        assert_eq!(args.env.get("SHELL"), Some(&"/bin/fish".to_string()));
        assert_eq!(args.env.len(), 2);
    }

    #[test]
    fn a_buffer_shorter_than_the_count_yields_nothing() {
        assert_eq!(parse_procargs(&[]), ProcessArgs::default());
        assert_eq!(parse_procargs(&[1, 0, 0]), ProcessArgs::default());
    }

    #[test]
    fn a_count_larger_than_the_strings_present_keeps_what_is_there() {
        let buffer = procargs(4, "/usr/bin/rx", 1, &["rx", "plan.md"]);

        let args = parse_procargs(&buffer);

        assert_eq!(args.argv, vec!["rx".to_string(), "plan.md".to_string()]);
        assert!(args.env.is_empty());
    }

    #[test]
    fn an_environment_entry_without_an_equals_sign_is_skipped() {
        let buffer = procargs(
            1,
            "/usr/bin/rx",
            1,
            &["rx", "_", "AGTERM_ENABLED=1", "OTHER=2"],
        );

        let args = parse_procargs(&buffer);

        assert_eq!(args.argv, vec!["rx".to_string()]);
        assert_eq!(args.env.get("AGTERM_ENABLED"), Some(&"1".to_string()));
        assert_eq!(args.env.get("OTHER"), Some(&"2".to_string()));
        assert_eq!(args.env.len(), 2);
    }

    #[test]
    fn an_empty_string_ends_the_environment() {
        let buffer = procargs(
            1,
            "/usr/bin/rx",
            1,
            &["rx", "AGTERM_PANE=left", "", "APPLE_TRAILER=1"],
        );

        let args = parse_procargs(&buffer);

        assert_eq!(args.env.get("AGTERM_PANE"), Some(&"left".to_string()));
        assert_eq!(args.env.len(), 1);
    }

    #[test]
    fn an_unterminated_trailing_string_is_dropped() {
        let mut buffer = procargs(1, "/usr/bin/rx", 1, &["rx", "AGTERM_PANE=left"]);
        buffer.extend_from_slice(b"TRUNCATED=y");

        let args = parse_procargs(&buffer);

        assert_eq!(args.env.get("AGTERM_PANE"), Some(&"left".to_string()));
        assert_eq!(args.env.len(), 1);
    }
}
