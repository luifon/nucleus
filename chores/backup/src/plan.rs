//! What one backup run reads: the configured paths expanded to absolute
//! roots, the SQLite databases under them, and the restic exclude patterns.
//!
//! The walk here and restic's own walk must agree on what is excluded, so
//! exclusions are limited to file and directory names (no `/`), matched the
//! same way on both sides: a name pattern excludes any entry with that name,
//! at any depth, and everything below it.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// The values the placeholders and relative paths of `[backup] paths`
/// resolve against.
#[derive(Debug, Clone)]
pub struct ExpandContext {
    pub home: PathBuf,
    pub workspace: PathBuf,
    pub vault: PathBuf,
    pub claude_project: PathBuf,
    pub launchd_prefix: String,
}

/// The configured paths after expansion.
#[derive(Debug, Default, PartialEq)]
pub struct Roots {
    /// Existing absolute paths, sorted, none inside another.
    pub roots: Vec<PathBuf>,
    /// Entries that matched nothing on disk (logged, not an error).
    pub missing: Vec<String>,
}

/// Expand `[backup] paths` into absolute roots.
pub fn expand_paths(entries: &[String], ctx: &ExpandContext) -> Result<Roots> {
    let mut out = Roots::default();
    let mut found: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let path = expand_entry(entry, ctx)?;
        let matches = match_last_component(&path)?;
        if matches.is_empty() {
            out.missing.push(entry.clone());
        }
        found.extend(matches);
    }
    found.sort();
    found.dedup();
    // Drop a root inside another root: restic would read it twice.
    let mut roots: Vec<PathBuf> = Vec::new();
    for p in found {
        if !roots.iter().any(|r| p.starts_with(r)) {
            roots.push(p);
        }
    }
    out.roots = roots;
    Ok(out)
}

/// One entry to an absolute path (the last component may still hold `*`/`?`).
pub fn expand_entry(entry: &str, ctx: &ExpandContext) -> Result<PathBuf> {
    let entry = entry.trim();
    if entry.is_empty() {
        bail!("[backup] paths has an empty entry");
    }
    let entry = entry.replace("{launchd_prefix}", &ctx.launchd_prefix);
    let (base, rest): (&Path, &str) = if let Some(rest) = entry.strip_prefix("~/") {
        (&ctx.home, rest)
    } else if let Some(rest) = placeholder(&entry, "{claude_project}") {
        (&ctx.claude_project, rest)
    } else if let Some(rest) = placeholder(&entry, "{vault}") {
        (&ctx.vault, rest)
    } else if entry.starts_with('/') {
        (Path::new("/"), entry.trim_start_matches('/'))
    } else if entry.contains('{') {
        bail!("[backup] paths entry {entry:?} has an unknown placeholder");
    } else {
        (&ctx.workspace, entry.as_str())
    };
    let rel = Path::new(rest);
    if rel.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("[backup] paths entry {entry:?} contains `..`");
    }
    let parent_has_wildcard = rel.parent().is_some_and(|p| has_wildcard(&p.to_string_lossy()));
    if parent_has_wildcard {
        bail!("[backup] paths entry {entry:?}: `*` and `?` are allowed only in the last component");
    }
    Ok(if rest.is_empty() { base.to_path_buf() } else { base.join(rel) })
}

fn placeholder<'a>(entry: &'a str, token: &str) -> Option<&'a str> {
    let rest = entry.strip_prefix(token)?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix('/')
    }
}

fn has_wildcard(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// The existing paths `path` names: itself, or the directory entries its
/// last component matches when that component has a wildcard.
fn match_last_component(path: &Path) -> Result<Vec<PathBuf>> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !has_wildcard(&name) {
        return Ok(if path.symlink_metadata().is_ok() { vec![path.to_path_buf()] } else { Vec::new() });
    }
    let Some(dir) = path.parent() else { return Ok(Vec::new()) };
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut out = Vec::new();
    for e in entries {
        let e = e?;
        if wildcard_match(&name, &e.file_name().to_string_lossy()) {
            out.push(e.path());
        }
    }
    out.sort();
    Ok(out)
}

/// `*` matches any run of characters (also none), `?` one character. No
/// character classes.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The `[backup] exclude` name patterns.
#[derive(Debug, Clone)]
pub struct Excludes(Vec<String>);

impl Excludes {
    pub fn new(patterns: &[String]) -> Result<Self> {
        for p in patterns {
            if p.trim().is_empty() || p.contains('/') {
                bail!("[backup] exclude entry {p:?} must be a file or directory name (no `/`)");
            }
            if p.contains('[') || p.contains('\\') {
                bail!("[backup] exclude entry {p:?}: only `*` and `?` are supported");
            }
            // In a restic exclude file a leading `#` is a comment and a
            // leading `!` negates the pattern.
            if p.starts_with('#') || p.starts_with('!') {
                bail!("[backup] exclude entry {p:?} cannot start with `#` or `!`");
            }
        }
        Ok(Self(patterns.to_vec()))
    }

    pub fn excludes(&self, name: &str) -> bool {
        self.0.iter().any(|p| wildcard_match(p, name))
    }

    pub fn patterns(&self) -> &[String] {
        &self.0
    }
}

const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
/// Files SQLite keeps next to a database. They are never copied: the
/// snapshot of the database already contains their committed content.
pub const SIDECAR_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Every SQLite database under the roots (a regular file whose first 16
/// bytes are SQLite's header), skipping excluded names and never following
/// symbolic links. Unreadable entries are skipped: restic reports them.
pub fn find_databases(roots: &[PathBuf], excludes: &Excludes) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        walk(root, excludes, &mut out);
    }
    out.sort();
    out
}

fn walk(path: &Path, excludes: &Excludes, out: &mut Vec<PathBuf>) {
    if path.file_name().is_some_and(|n| excludes.excludes(&n.to_string_lossy())) {
        return;
    }
    let Ok(meta) = path.symlink_metadata() else { return };
    if meta.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else { return };
        for e in entries.flatten() {
            walk(&e.path(), excludes, out);
        }
    } else if meta.is_file() && meta.len() >= SQLITE_MAGIC.len() as u64 && is_sqlite(path) {
        out.push(path.to_path_buf());
    }
}

pub fn is_sqlite(path: &Path) -> bool {
    let mut buf = [0u8; 16];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut buf)).is_ok() && &buf == SQLITE_MAGIC
}

/// Where a database's snapshot is written: its absolute path below the
/// staging directory, the same layout the homelab's own backup uses.
pub fn staged_path(staging: &Path, db: &Path) -> PathBuf {
    staging.join(db.strip_prefix("/").unwrap_or(db))
}

/// Escape a literal path for a restic pattern (`*`, `?`, `[` and `\` are
/// pattern syntax there).
pub fn escape_pattern(path: &Path) -> String {
    let mut s = String::new();
    for c in path.to_string_lossy().chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// restic expands environment variables in exclude files (`$$` is a
/// literal `$`), so a line is the pattern with every `$` doubled.
fn exclude_line(pattern: &str) -> String {
    pattern.replace('$', "$$")
}

/// The restic `--exclude-file` content: the name patterns, then each live
/// database and its sidecar files (their snapshots are read instead).
pub fn exclude_file(excludes: &Excludes, databases: &[PathBuf]) -> Result<String> {
    let mut lines: Vec<String> = excludes.patterns().iter().map(|p| exclude_line(p)).collect();
    for db in databases {
        let s = db.to_string_lossy();
        if s.contains('\n') || s.starts_with('#') || s.trim() != s {
            bail!("cannot exclude {s:?}: the name cannot be written to a restic exclude file");
        }
        lines.push(exclude_line(&escape_pattern(db)));
        for suffix in SIDECAR_SUFFIXES {
            lines.push(exclude_line(&escape_pattern(Path::new(&format!("{s}{suffix}")))));
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(tmp: &Path) -> ExpandContext {
        ExpandContext {
            home: tmp.join("home"),
            workspace: tmp.join("ws"),
            vault: tmp.join("home/Documents/Vault"),
            claude_project: tmp.join("home/.claude/projects/-ws"),
            launchd_prefix: "dev.nucleus".into(),
        }
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn entries_expand_against_home_workspace_and_placeholders() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(t.path());
        assert_eq!(expand_entry("memory", &c).unwrap(), t.path().join("ws/memory"));
        assert_eq!(expand_entry("~/.claude/settings.json", &c).unwrap(), t.path().join("home/.claude/settings.json"));
        assert_eq!(expand_entry("{claude_project}", &c).unwrap(), c.claude_project);
        assert_eq!(expand_entry("{vault}/0-Inbox", &c).unwrap(), c.vault.join("0-Inbox"));
        assert_eq!(
            expand_entry("~/Library/LaunchAgents/{launchd_prefix}.*.plist", &c).unwrap(),
            t.path().join("home/Library/LaunchAgents/dev.nucleus.*.plist")
        );
        assert_eq!(expand_entry("/etc/hosts", &c).unwrap(), PathBuf::from("/etc/hosts"));
        assert!(expand_entry("../outside", &c).is_err());
        assert!(expand_entry("{nope}/x", &c).is_err());
        assert!(expand_entry("{vault}x", &c).is_err(), "a placeholder must be a whole component");
        assert!(expand_entry("a*/b", &c).is_err(), "wildcards only in the last component");
        assert!(expand_entry("  ", &c).is_err());
    }

    #[test]
    fn expansion_keeps_existing_paths_globs_and_reports_missing() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(t.path());
        touch(&t.path().join("ws/memory/a.db"));
        touch(&t.path().join("ws/.env"));
        touch(&t.path().join("home/Library/LaunchAgents/dev.nucleus.a.plist"));
        touch(&t.path().join("home/Library/LaunchAgents/dev.nucleus.b.plist"));
        touch(&t.path().join("home/Library/LaunchAgents/com.other.plist"));
        let entries: Vec<String> = [
            "memory",
            "memory/a.db",
            ".env",
            "docs/PRIVATE.md",
            "~/Library/LaunchAgents/{launchd_prefix}.*.plist",
            "~/nothing/*.yaml",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let r = expand_paths(&entries, &c).unwrap();
        assert_eq!(
            r.roots,
            vec![
                t.path().join("home/Library/LaunchAgents/dev.nucleus.a.plist"),
                t.path().join("home/Library/LaunchAgents/dev.nucleus.b.plist"),
                t.path().join("ws/.env"),
                t.path().join("ws/memory"),
            ],
            "memory/a.db is inside memory and is dropped"
        );
        assert_eq!(r.missing, vec!["docs/PRIVATE.md".to_string(), "~/nothing/*.yaml".to_string()]);
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.plist", "dev.nucleus.a.plist"));
        assert!(wildcard_match("dev.nucleus.*.plist", "dev.nucleus.a.plist"));
        assert!(!wildcard_match("dev.nucleus.*.plist", "dev.nucleus.plist"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("a?c", "ac"));
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("node_modules", "node_modules"));
        assert!(!wildcard_match("node_modules", "node_modules2"));
        assert!(wildcard_match("*a*b*", "xxaYYbZ"));
    }

    #[test]
    fn excludes_reject_paths_and_classes() {
        assert!(Excludes::new(&["a/b".into()]).is_err());
        assert!(Excludes::new(&["[ab]".into()]).is_err());
        assert!(Excludes::new(&["".into()]).is_err());
        assert!(Excludes::new(&["#cache".into()]).is_err());
        assert!(Excludes::new(&["!keep".into()]).is_err());
        let e = Excludes::new(&["node_modules".into(), "*.tmp".into()]).unwrap();
        assert!(e.excludes("node_modules") && e.excludes("x.tmp") && !e.excludes("x.db"));
    }

    #[test]
    fn finds_databases_by_header_not_by_name() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("memory");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        let mk = |p: &Path| {
            let c = rusqlite::Connection::open(p).unwrap();
            c.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);").unwrap();
        };
        mk(&root.join("a.db"));
        mk(&root.join("reminders.db.bak-2026"));
        mk(&root.join("sub/c.sqlite"));
        mk(&root.join("node_modules/skip.db"));
        std::fs::write(root.join("not.db"), b"").unwrap();
        std::fs::write(root.join("text.db"), b"hello, this is not sqlite at all").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a.db"), root.join("link.db")).unwrap();
        let ex = Excludes::new(&["node_modules".into()]).unwrap();
        let found = find_databases(std::slice::from_ref(&root), &ex);
        assert_eq!(
            found,
            vec![root.join("a.db"), root.join("reminders.db.bak-2026"), root.join("sub/c.sqlite")]
        );
        // A root that is itself a database file.
        assert_eq!(find_databases(&[root.join("a.db")], &ex), vec![root.join("a.db")]);
    }

    #[test]
    fn staged_paths_mirror_the_absolute_path() {
        assert_eq!(
            staged_path(Path::new("/s/sqlite"), Path::new("/w/memory/a.db")),
            PathBuf::from("/s/sqlite/w/memory/a.db")
        );
    }

    #[test]
    fn exclude_file_lists_names_then_databases_with_sidecars() {
        let ex = Excludes::new(&["node_modules".into()]).unwrap();
        let out = exclude_file(&ex, &[PathBuf::from("/w/memory/a[1].db")]).unwrap();
        assert_eq!(
            out,
            "node_modules\n/w/memory/a\\[1\\].db\n/w/memory/a\\[1\\].db-wal\n/w/memory/a\\[1\\].db-shm\n/w/memory/a\\[1\\].db-journal\n"
        );
        assert!(exclude_file(&ex, &[PathBuf::from("/w/bad\nname.db")]).is_err());
        // restic expands `$NAME` in exclude files; `$$` is a literal `$`
        // (checked against restic 0.19.1: `a$$b` excludes the file `a$b`).
        let ex = Excludes::new(&["x$y".into()]).unwrap();
        let out = exclude_file(&ex, &[PathBuf::from("/w/a$b.db")]).unwrap();
        assert!(out.starts_with("x$$y\n/w/a$$b.db\n/w/a$$b.db-wal\n"), "{out}");
    }
}
