use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often the running binary is checked for having been replaced under the daemon.
pub const POLL: Duration = Duration::from_secs(2);

/// Tells one file on disk from another: the device it lives on and its inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    device: u64,
    inode: u64,
}

/// The binary this process is running from, as it was found at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executable {
    pub path: PathBuf,
    pub identity: Identity,
}

impl Executable {
    /// Reads the running binary, symlinks resolved, or nothing when it cannot be located.
    pub fn current() -> Option<Self> {
        let path = std::env::current_exe().ok()?;
        let path = fs::canonicalize(path).ok()?;
        let identity = identity(&path)?;
        Some(Self { path, identity })
    }
}

/// Looks the file up without following links; `None` means nothing is there.
pub fn identity(path: &Path) -> Option<Identity> {
    let found = fs::symlink_metadata(path).ok()?;
    Some(Identity {
        device: found.dev(),
        inode: found.ino(),
    })
}

/// Whether the file now at the path is no longer the one the daemon started from.
pub fn replaced(original: Identity, current: Option<Identity>) -> bool {
    current != Some(original)
}

/// Resolves once the executable has been swapped or removed, and never when there is none to watch.
pub async fn swapped(executable: Option<&Executable>) {
    let Some(Executable {
        path,
        identity: original,
    }) = executable
    else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        tokio::time::sleep(POLL).await;
        if replaced(*original, identity(path)) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("nikki-executable-test-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create the temporary directory");
            Self(path)
        }

        fn binary(&self) -> PathBuf {
            let Self(path) = self;
            let binary = path.join("nikki");
            fs::write(&binary, b"build").expect("write the binary");
            binary
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let Self(path) = self;
            let _ = fs::remove_dir_all(path);
        }
    }

    fn swap_in_a_new_file(path: &Path) {
        let staged = path.with_extension("new");
        fs::write(&staged, b"new build").expect("write the replacement");
        fs::rename(&staged, path).expect("rename it over the original");
    }

    fn watched(path: &Path) -> Executable {
        Executable {
            path: path.to_path_buf(),
            identity: identity(path).expect("the original identity"),
        }
    }

    #[test]
    fn a_file_has_an_identity_and_a_missing_one_has_none() {
        let directory = TempDir::new();
        let binary = directory.binary();

        assert!(identity(&binary).is_some());
        assert_eq!(identity(&binary.with_file_name("absent")), None);
    }

    #[test]
    fn the_same_file_keeps_its_identity_and_a_swapped_one_does_not() {
        let directory = TempDir::new();
        let binary = directory.binary();
        let Executable {
            path: _,
            identity: original,
        } = watched(&binary);

        assert!(!replaced(original, identity(&binary)));
        assert!(
            replaced(original, None),
            "a removed binary counts as replaced"
        );

        swap_in_a_new_file(&binary);
        assert!(
            replaced(original, identity(&binary)),
            "the way Homebrew swaps a bundle is a new inode at the same path"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_watch_resolves_once_the_binary_is_swapped() {
        let directory = TempDir::new();
        let binary = directory.binary();
        let executable = watched(&binary);
        swap_in_a_new_file(&binary);

        let waited = tokio::time::timeout(POLL * 4, swapped(Some(&executable))).await;
        assert!(waited.is_ok(), "a swapped binary ends the watch");
    }

    #[tokio::test(start_paused = true)]
    async fn an_untouched_binary_keeps_the_watch_waiting() {
        let directory = TempDir::new();
        let binary = directory.binary();
        let executable = watched(&binary);

        let waited = tokio::time::timeout(POLL * 100, swapped(Some(&executable))).await;
        assert!(waited.is_err(), "an untouched binary is left alone");
    }

    #[tokio::test(start_paused = true)]
    async fn a_binary_that_could_not_be_located_is_never_watched() {
        let waited = tokio::time::timeout(POLL * 100, swapped(None)).await;
        assert!(waited.is_err());
    }
}
