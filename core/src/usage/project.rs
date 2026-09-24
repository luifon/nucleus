//! Working directory → project (ADR-034).
//!
//! A project is a repository root, named by the root directory's name at
//! runtime (nothing is hard-coded). Resolution of one working directory:
//!
//! 1. **Claude scratch directories.** `/tmp/claude-<uid>/<encoded-cwd>/…`
//!    (Claude Code's per-session temp space, where it can also launch other
//!    tools) maps to the session directory it encodes, when a known working
//!    directory has that encoding.
//! 2. **Worktree markers.** A path containing a configured marker
//!    (`/.claude/worktrees/`, `/.worktrees/`) is cut at the marker: the part
//!    before it is the repository, whether the worktree still exists or not.
//! 3. **Git.** Walk up the existing ancestors. A `.git` directory marks the
//!    root. A `.git` file is a linked worktree: its `gitdir:` line points
//!    into `<main repo>/.git/worktrees/<name>`, and the main repository is
//!    the project. This covers worktrees kept outside the repository.
//! 4. **Deleted worktrees.** For a directory that no longer exists, take the
//!    deleted directory (top-most missing component) and the existing
//!    directory that held it:
//!    a. a sibling that earlier resolved through a `.git` file gives the
//!       repository (worktree managers keep a repo's worktrees side by side);
//!    b. a holding directory named like a known repository (and not that
//!       repository) is the `<manager>/<repo>/<workspace>` layout;
//!    c. a deleted `<repo>-<suffix>` next to a known `<repo>` is the
//!       `git worktree add ../<repo>-<branch>` layout.
//!    "Known repository" = a root resolved through git or a marker.
//! 5. **Fallback.** An existing directory is its own project; a missing
//!    one is attributed to the deleted directory.
//!
//! Mappings are sticky: once a directory is gone, the stored mapping is
//! kept instead of re-resolving to a worse answer (see `store.rs`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitEntry {
    None,
    Dir,
    /// Content of a `.git` file.
    File(String),
}

/// Filesystem probe, injectable for tests.
pub trait Probe {
    fn exists(&self, p: &Path) -> bool;
    fn git_entry(&self, dir: &Path) -> GitEntry;
}

pub struct RealFs;

impl Probe for RealFs {
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }
    fn git_entry(&self, dir: &Path) -> GitEntry {
        let g = dir.join(".git");
        match std::fs::metadata(&g) {
            Ok(m) if m.is_dir() => GitEntry::Dir,
            Ok(m) if m.is_file() => GitEntry::File(std::fs::read_to_string(&g).unwrap_or_default()),
            _ => GitEntry::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub root: String,
    pub name: String,
    /// `git`, `worktree`, `marker`, `sibling`, `scratch`, `directory`, `missing`.
    pub method: &'static str,
}

pub struct Resolver<'a, P: Probe> {
    pub probe: &'a P,
    pub home: PathBuf,
    pub markers: &'a [String],
    /// Claude-Code-encoded working directory → that directory.
    pub encoded: HashMap<String, String>,
    /// Parent directory → repository, learned from worktrees that resolved
    /// through a `.git` file.
    pub worktree_parents: HashMap<String, String>,
    /// Repository roots already known (resolved through git or a marker).
    pub repos: Vec<String>,
}

/// Claude Code's project-directory encoding: every character other than an
/// ASCII letter, digit or `-` becomes `-`.
pub fn encode_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

fn strip_trailing(p: &str) -> String {
    let t = p.trim_end_matches('/');
    if t.is_empty() { "/".to_string() } else { t.to_string() }
}

impl<'a, P: Probe> Resolver<'a, P> {
    pub fn name_of(&self, root: &str) -> String {
        if Path::new(root) == self.home {
            return "~".to_string();
        }
        Path::new(root)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string())
    }

    fn done(&self, root: &str, method: &'static str) -> Resolved {
        Resolved { root: root.to_string(), name: self.name_of(root), method }
    }

    pub fn resolve(&self, cwd: &str) -> Resolved {
        self.resolve_depth(cwd, 0)
    }

    fn resolve_depth(&self, cwd: &str, depth: u8) -> Resolved {
        let mut path = strip_trailing(cwd);

        // 1. Claude scratch dir.
        if depth == 0 {
            if let Some(enc) = scratch_encoding(&path) {
                if let Some(real) = self.encoded.get(enc) {
                    let mut r = self.resolve_depth(real, depth + 1);
                    r.method = "scratch";
                    return r;
                }
            }
        }

        // 2. Worktree markers.
        let mut marked = false;
        for m in self.markers {
            if m.is_empty() {
                continue;
            }
            if let Some(i) = path.find(m.as_str()) {
                path = strip_trailing(&path[..i]);
                marked = true;
            }
        }

        // 3. Git, over existing ancestors.
        let mut cur = Some(PathBuf::from(&path));
        while let Some(dir) = cur {
            if dir == self.home || dir.parent().is_none() {
                break;
            }
            if self.probe.exists(&dir) {
                match self.probe.git_entry(&dir) {
                    GitEntry::Dir => {
                        let root = dir.to_string_lossy().into_owned();
                        return self.done(&root, if marked { "marker" } else { "git" });
                    }
                    GitEntry::File(content) => {
                        let root = main_repo_from_gitfile(&content)
                            .unwrap_or_else(|| dir.to_string_lossy().into_owned());
                        return self.done(&root, "worktree");
                    }
                    GitEntry::None => {}
                }
            }
            cur = dir.parent().map(Path::to_path_buf);
        }
        if marked {
            return self.done(&path, "marker");
        }

        let p = Path::new(&path);
        if self.probe.exists(p) {
            return self.done(&path, "directory");
        }

        // The deleted directory: the top-most missing path component, and
        // the existing directory that held it.
        let mut missing = p.to_path_buf();
        while let Some(parent) = missing.parent() {
            if parent.parent().is_none() || self.probe.exists(parent) {
                break;
            }
            missing = parent.to_path_buf();
        }
        let holder = missing.parent().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default();
        let missing_name = missing.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

        // 4a. Siblings that resolved through a `.git` file.
        if let Some(repo) = self.worktree_parents.get(&holder) {
            return self.done(repo, "sibling");
        }
        // 4b. `<manager>/<repo>/<workspace>`: the holding directory is named
        //     like a known repository and is not one itself.
        let holder_name = Path::new(&holder).file_name().map(|n| n.to_string_lossy().into_owned());
        if let Some(hn) = holder_name.filter(|_| Path::new(&holder) != self.home) {
            if let Some(repo) = self.repos.iter().find(|r| self.name_of(r) == hn && **r != holder) {
                return self.done(repo, "repo-dir");
            }
        }
        // 4c. `<parent>/<repo>-<branch>` next to `<parent>/<repo>` (the
        //     `git worktree add ../<repo>-<branch>` layout). Longest name wins.
        if let Some(repo) = self
            .repos
            .iter()
            .filter(|r| {
                Path::new(r).parent().map(|x| x.to_string_lossy() == holder.as_str()).unwrap_or(false)
                    && missing_name.starts_with(&format!("{}-", self.name_of(r)))
            })
            .max_by_key(|r| r.len())
        {
            return self.done(repo, "repo-prefix");
        }

        // 5. The deleted directory itself.
        self.done(&missing.to_string_lossy(), "missing")
    }
}

/// `/tmp/claude-501/-Users-x-repo/<session>/scratchpad/…` → `-Users-x-repo`.
fn scratch_encoding(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/private").unwrap_or(path);
    let rest = rest.strip_prefix("/tmp/claude-")?;
    let mut parts = rest.split('/');
    let uid = parts.next()?;
    if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let enc = parts.next()?;
    enc.starts_with('-').then_some(enc)
}

/// `gitdir: /repo/.git/worktrees/name` → `/repo`.
pub fn main_repo_from_gitfile(content: &str) -> Option<String> {
    let line = content.lines().find_map(|l| l.trim().strip_prefix("gitdir:"))?.trim();
    let i = line.find("/.git/worktrees/")?;
    Some(line[..i].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    struct FakeFs {
        dirs: HashSet<String>,
        git: HashMap<String, GitEntry>,
    }

    impl Probe for FakeFs {
        fn exists(&self, p: &Path) -> bool {
            self.dirs.contains(p.to_string_lossy().as_ref())
        }
        fn git_entry(&self, dir: &Path) -> GitEntry {
            self.git.get(dir.to_string_lossy().as_ref()).cloned().unwrap_or(GitEntry::None)
        }
    }

    fn fs() -> FakeFs {
        let dirs = [
            "/home/op",
            "/home/op/code",
            "/home/op/code/alpha",
            "/home/op/code/alpha/src",
            "/home/op/code/alpha/src/deep",
            "/home/op/code/beta",
            "/home/op/wt",
            "/home/op/wt/alpha",
            "/home/op/wt/alpha/feature-x",
            "/home/op/notes",
            "/tmp",
        ];
        let mut git = HashMap::new();
        git.insert("/home/op/code/alpha".to_string(), GitEntry::Dir);
        git.insert("/home/op/code/beta".to_string(), GitEntry::Dir);
        git.insert(
            "/home/op/wt/alpha/feature-x".to_string(),
            GitEntry::File("gitdir: /home/op/code/alpha/.git/worktrees/feature-x\n".to_string()),
        );
        FakeFs { dirs: dirs.iter().map(|s| s.to_string()).collect(), git }
    }

    fn resolver<'a>(f: &'a FakeFs, markers: &'a [String]) -> Resolver<'a, FakeFs> {
        let mut encoded = HashMap::new();
        encoded.insert(encode_cwd("/home/op/code/beta"), "/home/op/code/beta".to_string());
        let mut worktree_parents = HashMap::new();
        worktree_parents.insert("/home/op/wt/alpha".to_string(), "/home/op/code/alpha".to_string());
        Resolver {
            probe: f,
            home: PathBuf::from("/home/op"),
            markers,
            encoded,
            worktree_parents,
            repos: vec!["/home/op/code/alpha".to_string(), "/home/op/code/beta".to_string()],
        }
    }

    #[test]
    fn deleted_worktrees_in_manager_and_prefix_layouts() {
        let f = fs();
        let m = markers();
        let r = resolver(&f, &m);
        // <manager>/<repo>/<workspace>: /home/op/wt exists, /home/op/wt/beta does not
        // hold a git file, and "beta" is a known repository.
        let mut dirs = f.dirs.clone();
        dirs.insert("/home/op/mgr".to_string());
        dirs.insert("/home/op/mgr/beta".to_string());
        let f2 = FakeFs { dirs, git: f.git.clone() };
        let r2 = Resolver { probe: &f2, ..r };
        let x = r2.resolve("/home/op/mgr/beta/feature-gone/src");
        assert_eq!((x.root.as_str(), x.method), ("/home/op/code/beta", "repo-dir"));
        // <parent>/<repo>-<branch> next to <parent>/<repo>
        let x = r2.resolve("/home/op/code/alpha-fix-login/web");
        assert_eq!((x.root.as_str(), x.method), ("/home/op/code/alpha", "repo-prefix"));
        // an unrelated deleted directory stays itself
        let x = r2.resolve("/home/op/code/gamma/web");
        assert_eq!((x.root.as_str(), x.method), ("/home/op/code/gamma", "missing"));
    }

    fn markers() -> Vec<String> {
        vec!["/.claude/worktrees/".to_string(), "/.worktrees/".to_string()]
    }

    #[test]
    fn subdirectory_maps_to_repo_root() {
        let f = fs();
        let m = markers();
        let r = resolver(&f, &m).resolve("/home/op/code/alpha/src/deep");
        assert_eq!((r.root.as_str(), r.name.as_str(), r.method), ("/home/op/code/alpha", "alpha", "git"));
    }

    #[test]
    fn nested_worktree_marker_even_when_deleted() {
        let f = fs();
        let m = markers();
        let r = resolver(&f, &m).resolve("/home/op/code/alpha/.claude/worktrees/agent-123");
        assert_eq!((r.root.as_str(), r.method), ("/home/op/code/alpha", "marker"));
        let r = resolver(&f, &m).resolve("/home/op/code/beta/.worktrees/gone/sub");
        assert_eq!(r.root, "/home/op/code/beta");
    }

    #[test]
    fn external_worktree_resolves_through_gitfile() {
        let f = fs();
        let m = markers();
        let r = resolver(&f, &m).resolve("/home/op/wt/alpha/feature-x");
        assert_eq!((r.root.as_str(), r.name.as_str(), r.method), ("/home/op/code/alpha", "alpha", "worktree"));
    }

    #[test]
    fn deleted_external_worktree_uses_siblings() {
        let f = fs();
        let m = markers();
        let r = resolver(&f, &m).resolve("/home/op/wt/alpha/feature-gone");
        assert_eq!((r.root.as_str(), r.method), ("/home/op/code/alpha", "sibling"));
    }

    #[test]
    fn scratch_dir_maps_to_the_encoded_session_dir() {
        let f = fs();
        let m = markers();
        let enc = encode_cwd("/home/op/code/beta");
        let r = resolver(&f, &m).resolve(&format!("/private/tmp/claude-501/{enc}/sess-1/scratchpad/x"));
        assert_eq!((r.root.as_str(), r.method), ("/home/op/code/beta", "scratch"));
    }

    #[test]
    fn non_git_and_missing_directories() {
        let f = fs();
        let m = markers();
        let r = resolver(&f, &m).resolve("/home/op/notes");
        assert_eq!((r.name.as_str(), r.method), ("notes", "directory"));
        let r = resolver(&f, &m).resolve("/home/op/code/removed/sub/dir");
        assert_eq!((r.root.as_str(), r.method), ("/home/op/code/removed", "missing"));
        let r = resolver(&f, &m).resolve("/home/op");
        assert_eq!(r.name, "~");
    }

    #[test]
    fn gitfile_parsing() {
        assert_eq!(
            main_repo_from_gitfile("gitdir: /r/.git/worktrees/w"),
            Some("/r".to_string())
        );
        assert_eq!(main_repo_from_gitfile("gitdir: /r/.git/modules/sub"), None);
    }
}
