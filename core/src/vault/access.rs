//! What a display surface (the dashboard) may list and open (ADR-035).
//!
//! The exclusion rules that keep credential notes out of the search index
//! apply to every other way of reaching a note as well: opening a note by
//! path, the recently-changed feed, and the bucket counts. A caller passes
//! the rules it just loaded ([`Exclusions::load`]), so a rule added to
//! nucleus.toml applies at the next request.
//!
//! Paths from a caller are vault-relative. An absolute path, `..`, `.` or
//! an empty component is refused before the filesystem is touched. The
//! path is then resolved (symlinks included) and must stay inside the
//! canonical vault root; the exclusion rules are checked on both the
//! requested and the resolved relative path, and a note's text is checked
//! before it is returned. No function here returns an absolute path.

use super::exclude::Exclusions;
use super::{read_note_capped, scan, MAX_NOTE_BYTES};
use std::path::{Component, Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

#[derive(Debug)]
pub enum AccessError {
    /// Not a plain vault-relative path (absolute, `..`, `.`, empty).
    Invalid,
    /// Resolves outside the vault root.
    Outside,
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
            Self::Outside => write!(f, "path is not inside the vault"),
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

/// Resolve `rel` under the vault. Returns the canonical absolute path (for
/// reading only; never returned to a client) and the canonical relative
/// path.
fn resolve(vault: &Path, rel: &Path) -> Result<(PathBuf, String), AccessError> {
    let root = std::fs::canonicalize(vault).map_err(io)?;
    let canonical = std::fs::canonicalize(root.join(rel)).map_err(io)?;
    let inner = canonical.strip_prefix(&root).map_err(|_| AccessError::Outside)?;
    let canon_rel: String = inner
        .components()
        .map(|c| c.as_os_str().to_string_lossy().nfc().collect::<String>())
        .collect::<Vec<_>>()
        .join("/");
    Ok((canonical, canon_rel))
}

pub struct OpenedNote {
    /// Canonical vault-relative path.
    pub rel: String,
    pub text: String,
}

/// Open a note for display. Path rules first (requested and resolved path),
/// then the size ceiling, then the credential detector on the text.
pub fn open_note(vault: &Path, ex: &Exclusions, requested: &str) -> Result<OpenedNote, AccessError> {
    let (rel_path, rel) = parse_rel(requested)?;
    if ex.path_excluded(&rel) {
        return Err(AccessError::Excluded);
    }
    let (abs, canon_rel) = resolve(vault, &rel_path)?;
    if canon_rel.is_empty() {
        return Err(AccessError::NotMarkdown);
    }
    if ex.path_excluded(&canon_rel) {
        return Err(AccessError::Excluded);
    }
    if !canon_rel.to_lowercase().ends_with(".md") {
        return Err(AccessError::NotMarkdown);
    }
    let meta = std::fs::metadata(&abs).map_err(io)?;
    if !meta.is_file() {
        return Err(AccessError::NotMarkdown);
    }
    let text = read_note_capped(&abs).map_err(io)?.ok_or(AccessError::TooLarge)?;
    if ex.content_excluded(&text) {
        return Err(AccessError::Excluded);
    }
    Ok(OpenedNote { rel: canon_rel, text })
}

/// The canonical vault-relative folder for a bucket/folder filter. An
/// excluded folder is refused; a missing one is `NotFound`.
pub fn resolve_folder(vault: &Path, ex: &Exclusions, folder: &str) -> Result<String, AccessError> {
    let (rel_path, rel) = parse_rel(folder)?;
    if ex.path_excluded(&rel) {
        return Err(AccessError::Excluded);
    }
    let (abs, canon_rel) = resolve(vault, &rel_path)?;
    if canon_rel.is_empty() || !std::fs::metadata(&abs).map_err(io)?.is_dir() {
        return Err(AccessError::NotFound);
    }
    if ex.path_excluded(&canon_rel) {
        return Err(AccessError::Excluded);
    }
    Ok(canon_rel)
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
/// canonical relative folder from [`resolve_folder`]; `skip` drops
/// surface-specific paths.
pub fn recent(
    vault: &Path,
    ex: &Exclusions,
    folder: Option<&str>,
    limit: usize,
    skip: impl Fn(&str) -> bool,
) -> anyhow::Result<Vec<RecentNote>> {
    let walk = scan::walk(vault, ex)?;
    let prefix = folder.map(|f| format!("{f}/"));
    let mut files: Vec<scan::VaultFile> = walk
        .files
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
        match read_note_capped(&f.abs) {
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
    for entry in std::fs::read_dir(vault)?.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let name: String = entry.file_name().to_string_lossy().nfc().collect();
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
        // or out of the vault, is refused.
        std::os::unix::fs::symlink(vault.join("4-Areas/Homelab/router.md"), vault.join("6-Slipbox/link.md")).unwrap();
        assert!(matches!(open_note(&vault, &e, "6-Slipbox/link.md"), Err(AccessError::Excluded)));
        std::os::unix::fs::symlink(tmp.path().join("outside.md"), vault.join("6-Slipbox/out.md")).unwrap();
        assert!(matches!(open_note(&vault, &e, "6-Slipbox/out.md"), Err(AccessError::Outside)));

        // Size ceiling.
        write(&vault, "0-Inbox/big.md", &"x".repeat(MAX_NOTE_BYTES as usize + 1));
        assert!(matches!(open_note(&vault, &e, "0-Inbox/big.md"), Err(AccessError::TooLarge)));
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
