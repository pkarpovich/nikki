use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const HOME_PREFIXES: [&str; 2] = ["~/", "$HOME/"];
const CACHE_LIMIT: usize = 50_000;

pub trait RepoLookup {
    fn repo_of(&self, path: &str) -> Option<String>;
}

pub struct GitRoots {
    home: Option<PathBuf>,
    known: RefCell<HashMap<PathBuf, Option<String>>>,
}

impl GitRoots {
    pub fn new(home: Option<PathBuf>) -> GitRoots {
        GitRoots {
            home,
            known: RefCell::new(HashMap::new()),
        }
    }

    pub fn from_env() -> GitRoots {
        GitRoots::new(std::env::var_os("HOME").map(PathBuf::from))
    }

    fn expand(&self, path: &str) -> Option<PathBuf> {
        for prefix in HOME_PREFIXES {
            let Some(rest) = path.strip_prefix(prefix) else {
                continue;
            };
            let home = self.home.as_ref()?;
            return Some(home.join(rest));
        }
        let path = Path::new(path);
        if !path.is_absolute() {
            return None;
        }
        Some(path.to_path_buf())
    }

    fn is_boundary(&self, dir: &Path) -> bool {
        dir.parent().is_none() || self.home.as_deref() == Some(dir)
    }
}

impl RepoLookup for GitRoots {
    fn repo_of(&self, path: &str) -> Option<String> {
        let path = self.expand(path)?;
        let mut visited = Vec::new();
        let mut found = None;
        for dir in path.ancestors() {
            if let Some(known) = self.known.borrow().get(dir) {
                found = known.clone();
                break;
            }
            visited.push(dir.to_path_buf());
            if self.is_boundary(dir) {
                break;
            }
            if dir.join(".git").exists() {
                found = Some(dir.to_string_lossy().into_owned());
                break;
            }
        }

        let mut known = self.known.borrow_mut();
        if known.len() > CACHE_LIMIT {
            known.clear();
        }
        for dir in visited {
            known.insert(dir, found.clone());
        }
        found
    }
}

#[cfg(test)]
pub struct NoRepos;

#[cfg(test)]
impl RepoLookup for NoRepos {
    fn repo_of(&self, _path: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(name: &str) -> TempTree {
            let root = std::env::temp_dir().join(format!("nikki-repo-roots-{name}"));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("the temporary tree could not be created");
            let root = root
                .canonicalize()
                .expect("the temporary tree canonicalises");
            TempTree { root }
        }

        fn dir(&self, relative: &str) -> PathBuf {
            let dir = self.root.join(relative);
            fs::create_dir_all(&dir).expect("the directory could not be created");
            dir
        }

        fn text(&self, relative: &str) -> String {
            self.root.join(relative).to_string_lossy().into_owned()
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_file_inside_a_repository_resolves_to_the_repository_root() {
        let tree = TempTree::new("root");
        tree.dir("home/Projects/alpha/.git");
        tree.dir("home/Projects/alpha/src/app");
        let roots = GitRoots::new(Some(tree.root.join("home")));

        assert_eq!(
            roots.repo_of(&tree.text("home/Projects/alpha/src/app/main.rs")),
            Some(tree.text("home/Projects/alpha"))
        );
    }

    #[test]
    fn a_worktree_whose_git_is_a_file_is_its_own_root() {
        let tree = TempTree::new("worktree");
        tree.dir("home/Projects/gamma");
        fs::write(
            tree.root.join("home/Projects/gamma/.git"),
            "gitdir: /elsewhere",
        )
        .expect("the .git file could not be written");
        let roots = GitRoots::new(Some(tree.root.join("home")));

        assert_eq!(
            roots.repo_of(&tree.text("home/Projects/gamma/README.md")),
            Some(tree.text("home/Projects/gamma"))
        );
    }

    #[test]
    fn a_path_that_no_longer_exists_still_resolves_through_its_ancestors() {
        let tree = TempTree::new("gone");
        tree.dir("home/Projects/nikki/.git");
        let roots = GitRoots::new(Some(tree.root.join("home")));

        assert_eq!(
            roots.repo_of(&tree.text("home/Projects/nikki/deleted/dir/file.rs")),
            Some(tree.text("home/Projects/nikki"))
        );
    }

    #[test]
    fn home_relative_paths_expand_against_home() {
        let tree = TempTree::new("home-relative");
        tree.dir("home/Projects/alpha/.git");
        let roots = GitRoots::new(Some(tree.root.join("home")));

        for path in ["~/Projects/alpha/go.mod", "$HOME/Projects/alpha/go.mod"] {
            assert_eq!(
                roots.repo_of(path),
                Some(tree.text("home/Projects/alpha")),
                "{path}"
            );
        }
    }

    #[test]
    fn a_path_outside_any_repository_resolves_to_nothing() {
        let tree = TempTree::new("outside");
        tree.dir("home/scratch");
        let roots = GitRoots::new(Some(tree.root.join("home")));

        assert_eq!(roots.repo_of(&tree.text("home/scratch/notes.txt")), None);
        assert_eq!(roots.repo_of("relative/path.rs"), None);
    }

    #[test]
    fn a_repository_at_home_itself_is_never_the_answer() {
        let tree = TempTree::new("home-repo");
        tree.dir("home/.git");
        tree.dir("home/Documents");
        let roots = GitRoots::new(Some(tree.root.join("home")));

        assert_eq!(roots.repo_of(&tree.text("home/Documents/letter.txt")), None);
    }

    #[test]
    fn a_cached_answer_is_reused_for_siblings() {
        let tree = TempTree::new("cache");
        tree.dir("home/Projects/alpha/.git");
        tree.dir("home/Projects/alpha/src");
        let roots = GitRoots::new(Some(tree.root.join("home")));

        assert_eq!(
            roots.repo_of(&tree.text("home/Projects/alpha/src/a.rs")),
            Some(tree.text("home/Projects/alpha"))
        );
        fs::remove_dir_all(tree.root.join("home/Projects/alpha/.git"))
            .expect("the .git directory could not be removed");
        assert_eq!(
            roots.repo_of(&tree.text("home/Projects/alpha/src/b.rs")),
            Some(tree.text("home/Projects/alpha"))
        );
    }
}
