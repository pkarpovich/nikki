use std::collections::HashMap;
use std::ptr::null_mut;

const PROC_ALL_PIDS: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessArgs {
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Process {
    pub pid: i32,
    pub pgid: i32,
    pub tdev: i32,
    pub tpgid: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub session: String,
    pub pane: String,
    pub pane_id: String,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
}

impl Process {
    pub fn has_tty(&self) -> bool {
        self.tdev != -1
    }

    pub fn is_foreground(&self) -> bool {
        self.has_tty() && self.pgid == self.tpgid
    }

    pub fn leads_its_group(&self) -> bool {
        self.pid == self.pgid
    }
}

pub fn agterm_panes() -> Vec<Pane> {
    let mut panes = Vec::new();
    for process in list() {
        if !process.is_foreground() || !process.leads_its_group() {
            continue;
        }
        let Some(args) = read_args(process.pid) else {
            continue;
        };
        let Some(mut pane) = pane_of(&process, &args) else {
            continue;
        };
        pane.cwd = cwd(process.pid);
        panes.push(pane);
    }
    panes
}

fn pane_of(process: &Process, args: &ProcessArgs) -> Option<Pane> {
    if args.env.get("AGTERM_ENABLED").map(String::as_str) != Some("1") {
        return None;
    }

    let session = args.env.get("AGTERM_SESSION_ID")?;
    if session.is_empty() {
        return None;
    }
    if !process.is_foreground() || !process.leads_its_group() {
        return None;
    }

    Some(Pane {
        session: session.to_uppercase(),
        pane: args
            .env
            .get("AGTERM_PANE")
            .cloned()
            .unwrap_or_else(|| "left".to_string()),
        pane_id: args.env.get("AGTERM_PANE_ID").cloned().unwrap_or_default(),
        argv: args.argv.clone(),
        cwd: None,
    })
}

pub fn list() -> Vec<Process> {
    let mut processes = Vec::new();
    for pid in pids() {
        let Some(process) = describe(pid) else {
            continue;
        };
        processes.push(process);
    }
    processes
}

fn pids() -> Vec<i32> {
    let size = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }

    let mut pids = vec![0i32; size as usize / size_of::<i32>()];
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<i32>()) as i32,
        )
    };
    if bytes <= 0 {
        return Vec::new();
    }

    pids.truncate(bytes as usize / size_of::<i32>());
    pids.retain(|pid| *pid > 0);
    pids
}

fn describe(pid: i32) -> Option<Process> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    let read =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if read != size {
        return None;
    }

    Some(Process {
        pid,
        pgid: info.pbi_pgid as i32,
        tdev: info.e_tdev as i32,
        tpgid: info.e_tpgid as i32,
    })
}

pub fn cwd(pid: i32) -> Option<String> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_vnodepathinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if read != size {
        return None;
    }

    let mut path = Vec::new();
    'chunks: for chunk in info.pvi_cdir.vip_path {
        for byte in chunk {
            if byte == 0 {
                break 'chunks;
            }
            path.push(byte as u8);
        }
    }
    if path.is_empty() {
        return None;
    }

    Some(String::from_utf8_lossy(&path).into_owned())
}

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

    fn process(pgid: i32, tdev: i32, tpgid: i32) -> Process {
        Process {
            pid: pgid,
            pgid,
            tdev,
            tpgid,
        }
    }

    fn child_of(leader: Process, pid: i32) -> Process {
        Process { pid, ..leader }
    }

    #[test]
    fn a_process_without_a_terminal_is_neither_attached_nor_foreground() {
        let daemon = process(900, -1, -1);

        assert!(!daemon.has_tty());
        assert!(!daemon.is_foreground());
    }

    #[test]
    fn a_process_behind_its_terminals_foreground_group_is_not_foreground() {
        let shell = process(900, 16777222, 1200);

        assert!(shell.has_tty());
        assert!(!shell.is_foreground());
    }

    #[test]
    fn a_process_owning_its_terminals_foreground_group_is_foreground() {
        let editor = process(1200, 16777222, 1200);

        assert!(editor.has_tty());
        assert!(editor.is_foreground());
        assert!(editor.leads_its_group());
    }

    #[test]
    fn a_helper_spawned_inside_the_foreground_group_does_not_lead_it() {
        let helper = child_of(process(1200, 16777222, 1200), 1387);

        assert!(helper.is_foreground());
        assert!(!helper.leads_its_group());
    }

    fn args(argv: &[&str], env: &[(&str, &str)]) -> ProcessArgs {
        let mut args = ProcessArgs::default();
        for argument in argv {
            args.argv.push((*argument).to_string());
        }
        for (key, value) in env {
            args.env.insert((*key).to_string(), (*value).to_string());
        }
        args
    }

    #[test]
    fn a_foreground_agterm_process_becomes_its_pane() {
        let editor = process(1200, 16777222, 1200);
        let args = args(
            &["rx", "plan.md"],
            &[
                ("AGTERM_ENABLED", "1"),
                ("AGTERM_SESSION_ID", "a1b2"),
                ("AGTERM_PANE", "scratch"),
                ("AGTERM_PANE_ID", "p7"),
            ],
        );

        let pane = pane_of(&editor, &args).expect("a foreground agterm process is a pane");

        assert_eq!(pane.session, "A1B2");
        assert_eq!(pane.pane, "scratch");
        assert_eq!(pane.pane_id, "p7");
        assert_eq!(pane.argv, vec!["rx".to_string(), "plan.md".to_string()]);
        assert_eq!(pane.cwd, None);
    }

    #[test]
    fn a_session_id_is_uppercased_so_it_joins_the_tree_case_insensitively() {
        let editor = process(1200, 16777222, 1200);
        let args = args(
            &["rx"],
            &[
                ("AGTERM_ENABLED", "1"),
                ("AGTERM_SESSION_ID", "5e7b21c4-6f30-4d9a"),
            ],
        );

        let pane = pane_of(&editor, &args).expect("a foreground agterm process is a pane");

        assert_eq!(pane.session, "5E7B21C4-6F30-4D9A");
    }

    #[test]
    fn a_process_outside_agterm_is_not_a_pane() {
        let editor = process(1200, 16777222, 1200);
        let args = args(&["vim"], &[("SHELL", "/bin/fish")]);

        assert_eq!(pane_of(&editor, &args), None);
    }

    #[test]
    fn a_terminal_that_is_not_agterm_carries_no_pane_even_with_a_session_id() {
        let editor = process(1200, 16777222, 1200);
        let args = args(
            &["rx"],
            &[
                ("AGTERM_SESSION_ID", "a1b2"),
                ("AGTERM_PANE", "scratch"),
                ("AGTERM_PANE_ID", "p7"),
            ],
        );

        assert_eq!(pane_of(&editor, &args), None);
    }

    #[test]
    fn a_helper_inside_the_foreground_group_is_not_the_pane_its_leader_is() {
        let leader = process(1200, 16777222, 1200);
        let helper = child_of(leader, 1387);
        let args = args(
            &["chrome-devtools-mcp"],
            &[
                ("AGTERM_ENABLED", "1"),
                ("AGTERM_SESSION_ID", "a1b2"),
                ("AGTERM_PANE", "left"),
            ],
        );

        assert_eq!(pane_of(&helper, &args), None);
        assert!(pane_of(&leader, &args).is_some());
    }

    #[test]
    fn a_daemon_inheriting_the_environment_is_not_a_pane() {
        let daemon = process(900, -1, -1);
        let args = args(
            &["op", "daemon"],
            &[
                ("AGTERM_ENABLED", "1"),
                ("AGTERM_SESSION_ID", "a1b2"),
                ("AGTERM_PANE", "left"),
            ],
        );

        assert_eq!(pane_of(&daemon, &args), None);
    }

    #[test]
    fn the_shell_behind_the_foreground_group_is_not_a_pane() {
        let shell = process(900, 16777222, 1200);
        let args = args(
            &["fish"],
            &[
                ("AGTERM_ENABLED", "1"),
                ("AGTERM_SESSION_ID", "a1b2"),
                ("AGTERM_PANE", "left"),
            ],
        );

        assert_eq!(pane_of(&shell, &args), None);
    }

    #[test]
    fn a_session_id_that_is_empty_is_not_a_pane() {
        let editor = process(1200, 16777222, 1200);
        let args = args(
            &["rx"],
            &[("AGTERM_ENABLED", "1"), ("AGTERM_SESSION_ID", "")],
        );

        assert_eq!(pane_of(&editor, &args), None);
    }

    #[test]
    fn a_missing_pane_variable_reads_as_the_left_pane() {
        let editor = process(1200, 16777222, 1200);
        let args = args(
            &["claude"],
            &[("AGTERM_ENABLED", "1"), ("AGTERM_SESSION_ID", "a1b2")],
        );

        let pane = pane_of(&editor, &args).expect("a foreground agterm process is a pane");

        assert_eq!(pane.pane, "left");
        assert_eq!(pane.pane_id, "");
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
