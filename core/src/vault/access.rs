//! What a display surface (the dashboard) may list and open (ADR-035).
//!
//! The exclusion rules that keep credential notes out of the search index
//! apply to every other way of reaching a note as well: opening a note by
//! path, the recently-changed feed, and the bucket counts. A caller passes
//! the rules it just loaded ([`Exclusions::load`]), so a rule added to
//! nucleus.toml applies at the next request.
//!
//! Paths from a caller are vault-relative. An absolute path, `..`, `.` or
//! an empty component is refused before the filesystem is touched, and the
//! exclusion rules are checked on the path. The note is then opened
//! relative to the vault root descriptor, one component at a time with
//! `O_NOFOLLOW` ([`super::fsx`]): a symlink anywhere below the root is
//! refused (answered as a missing note, the same as the walk, which never
//! follows one), so the path that was checked is the file that is read.
//! The opened descriptor is checked with `fstat` to be a regular file and
//! read directly; the path is never opened a second time. The text is
//! checked by the credential detector before it is returned. No function
//! here returns an absolute path.

use super::exclude::Exclusions;
use super::fsx::{self, Kind, Root};
use super::{read_capped, scan, MAX_NOTE_BYTES};
use std::path::{Component, Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

#[derive(Debug)]
pub enum AccessError {
    /// Not a plain vault-relative path (absolute, `..`, `.`, empty).
    Invalid,
    /// Excluded by a path glob or by the credential detector. Callers
    /// answer exactly as for a missing file, so the response does not
    /// confirm that an excluded note exists.
    Excluded,
    NotMarkdown,
    /// Larger than [`MAX_NOTE_BYTES`].
    TooLarge,
    NotFound,
    Io(std::io::Error),
}

impl std::fmt::Display for AccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => write!(f, "not a vault-relative path"),
            Self::Excluded | Self::NotFound => write!(f, "no such note"),
            Self::NotMarkdown => write!(f, "not a markdown note"),
            Self::TooLarge => write!(f, "note is larger than {MAX_NOTE_BYTES} bytes"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AccessError {}

fn io(e: std::io::Error) -> AccessError {
    if e.kind() == std::io::ErrorKind::NotFound {
        AccessError::NotFound
    } else {
        AccessError::Io(e)
    }
}

/// Parse a caller-supplied vault-relative path. Only normal components are
/// accepted. Returns the path and its `/`-joined NFC string.
pub fn parse_rel(input: &str) -> Result<(PathBuf, String), AccessError> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.contains('\0') || trimmed.contains('\\') {
        return Err(AccessError::Invalid);
    }
    let path = Path::new(trimmed);
    let mut parts: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_string_lossy().nfc().collect()),
            _ => return Err(AccessError::Invalid),
        }
    }
    // `a//b` and a trailing `/` normalize away in `components()`; an input
    // with an explicit `.` segment is refused above.
    if parts.is_empty() || trimmed.split('/').any(|s| s == "." || s == "..") {
        return Err(AccessError::Invalid);
    }
    let rel = parts.join("/");
    Ok((PathBuf::from(&rel), rel))
}

/// Map an error from a descriptor-relative open. A refused symlink
/// component answers as a missing note; an entry that is not a regular
/// file as not a note.
fn open_error(e: std::io::Error, not_regular: AccessError) -> AccessError {
    if fsx::is_symlink_refusal(&e) {
        AccessError::NotFound
    } else if e.kind() == std::io::ErrorKind::InvalidInput {
        not_regular
    } else {
        io(e)
    }
}

pub struct OpenedNote {
    /// Vault-relative path (`/`-separated, NFC).
    pub rel: String,
    pub text: String,
}

/// Open a note for display. Path rules first, then a descriptor-relative
/// open that follows no symlink, then the size ceiling, then the
/// credential detector on the text read from the opened descriptor.
pub fn open_note(vault: &Path, ex: &Exclusions, requested: &str) -> Result<OpenedNote, AccessError> {
    let (rel_path, rel) = parse_rel(requested)?;
    if ex.path_excluded(&rel) {
        return Err(AccessError::Excluded);
    }
    if !rel.to_lowercase().ends_with(".md") {
        return Err(AccessError::NotMarkdown);
    }
    let root = Root::open(vault).map_err(io)?;
    let (file, _) = root.open_file(&rel_path).map_err(|e| open_error(e, AccessError::NotMarkdown))?;
    let text = read_capped(file).map_err(io)?.ok_or(AccessError::TooLarge)?;
    if ex.content_excluded(&text) {
        return Err(AccessError::Excluded);
    }
    Ok(OpenedNote { rel, text })
}

/// The vault-relative folder for a bucket/folder filter. An excluded
/// folder is refused; a missing one, a file, or a path through a symlink
/// is `NotFound`.
pub fn resolve_folder(vault: &Path, ex: &Exclusions, folder: &str) -> Result<String, AccessError> {
    let (rel_path, rel) = parse_rel(folder)?;
    if ex.path_excluded(&rel) {
        return Err(AccessError::Excluded);
    }
    let root = Root::open(vault).map_err(io)?;
    root.open_dir(&rel_path).map_err(|e| open_error(e, AccessError::NotFound))?;
    Ok(rel)
}

#[derive(Debug, Clone)]
pub struct RecentNote {
    /// Vault-relative path.
    pub rel: String,
    pub size: u64,
    /// Unix seconds.
    pub mtime: i64,
}

/// The most recently modified notes that may be shown, newest first. The
/// walk applies the path rules; every returned note is also read and
/// passed through the credential detector, and a note over the size
/// ceiling is left out because its text cannot be checked. `folder` is a
/// relative folder from [`resolve_folder`]; `skip` drops
/// surface-specific paths.
pub fn recent(
    vault: &Path,
    ex: &Exclusions,
    folder: Option<&str>,
    limit: usize,
    skip: impl Fn(&str) -> bool,
) -> anyhow::Result<Vec<RecentNote>> {
    let mut walk = scan::walk(vault, ex)?;
    let prefix = folder.map(|f| format!("{f}/"));
    let mut files: Vec<scan::VaultFile> = std::mem::take(&mut walk.files)
        .into_iter()
        .filter(|f| f.is_markdown())
        .filter(|f| prefix.as_deref().is_none_or(|p| f.rel.starts_with(p)))
        .filter(|f| !skip(&f.rel))
        .collect();
    files.sort_by(|a, b| b.mtime_ns.cmp(&a.mtime_ns).then_with(|| a.rel.cmp(&b.rel)));
    let mut out = Vec::new();
    for f in files {
        if out.len() >= limit {
            break;
        }
        if f.size > MAX_NOTE_BYTES {
            continue;
        }
        match walk.root.read_note(&f.raw_rel) {
            Ok(Some(text)) if !ex.content_excluded(&text) => {
                out.push(RecentNote { rel: f.rel, size: f.size, mtime: f.mtime })
            }
            _ => continue,
        }
    }
    Ok(out)
}

/// Top-level folders that may be shown, with their count of markdown files
/// the walk admits, sorted by name. Dot folders and excluded folders are
/// left out.
pub fn buckets(vault: &Path, ex: &Exclusions) -> anyhow::Result<Vec<(String, usize)>> {
    let walk = scan::walk(vault, ex)?;
    let mut out: Vec<(String, usize)> = Vec::new();
    for (raw, id) in fsx::list(walk.root.fd())? {
        if id.kind != Kind::Dir {
            continue;
        }
        let name: String = raw.to_string_lossy().nfc().collect();
        if name.starts_with('.') || name == "node_modules" || ex.path_excluded(&name) {
            continue;
        }
        let prefix = format!("{name}/");
        let count = walk.files.iter().filter(|f| f.is_markdown() && f.rel.starts_with(&prefix)).count();
        out.push((name, count));
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    /// Synthetic vault; names and bodies are invented.
    fn fixture(root: &Path) {
        write(root, "3-Projects/Alpha/index.md", "# Alpha\n\nrocket\n");
        write(root, "4-Areas/Homelab/router.md", "# Router\n");
        write(root, "6-Slipbox/keys.md", "api_key: abc\n");
        write(root, "6-Slipbox/idea.md", "# Idea\n");
        write(root, "secrets/list.md", "# x\n");
        write(root, ".obsidian/app.md", "x");
    }

    fn ex() -> Exclusions {
        Exclusions::new(&[], "").unwrap()
    }

    #[test]
    fn parse_rel_accepts_only_normal_components() {
        assert_eq!(parse_rel("3-Projects/Alpha/index.md").unwrap().1, "3-Projects/Alpha/index.md");
        for bad in ["", "/etc/passwd", "../x.md", "a/../b.md", "./a.md", "a/./b.md", "a\\b.md", "a\0.md"] {
            assert!(matches!(parse_rel(bad), Err(AccessError::Invalid)), "{bad:?}");
        }
    }

    #[test]
    fn open_note_applies_path_and_content_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        fixture(&vault);
        write(tmp.path(), "outside.md", "# outside\n");
        let e = ex();

        let n = open_note(&vault, &e, "3-Projects/Alpha/index.md").unwrap();
        assert_eq!(n.rel, "3-Projects/Alpha/index.md");
        assert!(matches!(open_note(&vault, &e, "4-Areas/Homelab/router.md"), Err(AccessError::Excluded)));
        assert!(matches!(open_note(&vault, &e, "6-Slipbox/keys.md"), Err(AccessError::Excluded)));
        assert!(matches!(open_note(&vault, &e, ".obsidian/app.md"), Err(AccessError::Excluded)));
        assert!(matches!(open_note(&vault, &e, "../outside.md"), Err(AccessError::Invalid)));
        let abs = vault.join("3-Projects/Alpha/index.md");
        assert!(matches!(open_note(&vault, &e, abs.to_str().unwrap()), Err(AccessError::Invalid)));
        assert!(matches!(open_note(&vault, &e, "3-Projects/nope.md"), Err(AccessError::NotFound)));

        // A symlink with an innocent name that points at an excluded note,
        // or out of the vault, is not followed: it answers as missing.
        std::os::unix::fs::symlink(vault.join("4-Areas/Homelab/router.md"), vault.join("6-Slipbox/link.md")).unwrap();
        assert!(matches!(open_note(&vault, &e, "6-Slipbox/link.md"), Err(AccessError::NotFound)));
        std::os::unix::fs::symlink(tmp.path().join("outside.md"), vault.join("6-Slipbox/out.md")).unwrap();
        assert!(matches!(open_note(&vault, &e, "6-Slipbox/out.md"), Err(AccessError::NotFound)));
        // A symlinked folder is not followed either.
        std::os::unix::fs::symlink(tmp.path(), vault.join("6-Slipbox/up")).unwrap();
        assert!(matches!(open_note(&vault, &e, "6-Slipbox/up/outside.md"), Err(AccessError::NotFound)));
        assert!(matches!(resolve_folder(&vault, &e, "6-Slipbox/up"), Err(AccessError::NotFound)));
        // A folder named like a note is not a note.
        fs::create_dir_all(vault.join("6-Slipbox/dir.md")).unwrap();
        assert!(matches!(open_note(&vault, &e, "6-Slipbox/dir.md"), Err(AccessError::NotMarkdown)));

        // Size ceiling.
        write(&vault, "0-Inbox/big.md", &"x".repeat(MAX_NOTE_BYTES as usize + 1));
        assert!(matches!(open_note(&vault, &e, "0-Inbox/big.md"), Err(AccessError::TooLarge)));
    }

    /// Replace `path` atomically (rename over it) with a regular file
    /// holding `text` or with a symlink to `target`.
    fn swap_to_file(path: &Path, text: &str) {
        let tmp = path.with_extension("swap-f");
        fs::write(&tmp, text).unwrap();
        fs::rename(&tmp, path).unwrap();
    }

    fn swap_to_link(path: &Path, target: &Path) {
        let tmp = path.with_extension("swap-l");
        let _ = fs::remove_file(&tmp);
        std::os::unix::fs::symlink(target, &tmp).unwrap();
        fs::rename(&tmp, path).unwrap();
    }

    /// While another thread keeps swapping a note (and, separately, its
    /// folder) between the real entry and a symlink to a file outside the
    /// vault, `open_note` never returns the outside file's text. Before the
    /// descriptor-relative open, the check (canonicalize) and the read
    /// (path open) were separate steps, and a swap between them was
    /// followed.
    #[test]
    fn concurrent_symlink_swap_never_reads_outside() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        write(&vault, "6-Slipbox/n.md", "inside\n");
        write(&vault, "6-Slipbox/d/m.md", "inside\n");
        write(tmp.path(), "outside/secret.md", "OUTSIDE\n");
        write(tmp.path(), "outside/m.md", "OUTSIDE\n");
        let e = ex();

        // File swap.
        let stop = Arc::new(AtomicBool::new(false));
        let (note, secret, s2) = (vault.join("6-Slipbox/n.md"), tmp.path().join("outside/secret.md"), stop.clone());
        let t = std::thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                swap_to_link(&note, &secret);
                swap_to_file(&note, "inside\n");
            }
        });
        let mut inside = 0;
        for _ in 0..3000 {
            match open_note(&vault, &e, "6-Slipbox/n.md") {
                Ok(n) => {
                    assert_eq!(n.text, "inside\n");
                    inside += 1;
                }
                Err(AccessError::NotFound) => {}
                Err(other) => panic!("{other:?}"),
            }
        }
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        assert!(inside > 0, "the real note was never readable");

        // Folder swap: `6-Slipbox/d` alternates between the real folder
        // and a symlink to the outside folder.
        let real = vault.join("6-Slipbox/d-real");
        fs::rename(vault.join("6-Slipbox/d"), &real).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (d, out_dir, real2, s2) = (vault.join("6-Slipbox/d"), tmp.path().join("outside"), real.clone(), stop.clone());
        let t = std::thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                swap_to_link(&d, &out_dir);
                swap_to_link(&d, &real2);
            }
        });
        for _ in 0..3000 {
            match open_note(&vault, &e, "6-Slipbox/d/m.md") {
                Ok(n) => panic!("read through a symlinked folder: {:?}", n.text),
                Err(AccessError::NotFound) => {}
                Err(other) => panic!("{other:?}"),
            }
        }
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn recent_and_buckets_hide_excluded_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        fixture(&vault);
        let e = ex();
        let all: Vec<String> = recent(&vault, &e, None, 50, |_| false).unwrap().into_iter().map(|n| n.rel).collect();
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["3-Projects/Alpha/index.md", "6-Slipbox/idea.md"]);

        let f = resolve_folder(&vault, &e, "6-Slipbox").unwrap();
        let only: Vec<String> = recent(&vault, &e, Some(&f), 50, |_| false).unwrap().into_iter().map(|n| n.rel).collect();
        assert_eq!(only, vec!["6-Slipbox/idea.md"]);
        assert!(matches!(resolve_folder(&vault, &e, ".."), Err(AccessError::Invalid)));
        assert!(matches!(resolve_folder(&vault, &e, "/"), Err(AccessError::Invalid)));
        assert!(matches!(resolve_folder(&vault, &e, "4-Areas/Homelab"), Err(AccessError::Excluded)));
        assert!(matches!(resolve_folder(&vault, &e, "3-Projects/Alpha/index.md"), Err(AccessError::NotFound)));

        let b = buckets(&vault, &e).unwrap();
        assert_eq!(
            b,
            vec![("3-Projects".to_string(), 1), ("4-Areas".to_string(), 0), ("6-Slipbox".to_string(), 2)]
        );
    }
}
