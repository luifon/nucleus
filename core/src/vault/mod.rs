//! Vault search and vault check (ADR-035).
//!
//! The Obsidian vault (T3, ADR-005) is plain markdown on disk. Before this
//! module, a process could only see it as a truncated folder tree and read
//! notes one by one. This module gives every process two capabilities:
//!
//! - [`index`] — an FTS5 index over the notes, owned by core at
//!   `memory/vault_index.db` (ADR-020 DB ownership: only this module writes
//!   it). Updated incrementally by mtime/size before every query.
//! - [`check`] — a deterministic structural check (duplicates, broken
//!   links, orphans, stale inbox, frontmatter, source vocabulary, empty
//!   files) with a small set of safe fixes, and a run history at
//!   `memory/vault_check.db` (also written only by this module).
//!
//! Both start from the same walk ([`scan`]) and the same exclusion rules
//! ([`exclude`]). Excluded files are never indexed, never returned by a
//! search, never listed in a check report, and never modified. The
//! exclusion floor keeps credential notes out: credentials reach the
//! operator only through the authenticated DM, never through a search
//! result that a session or the dashboard could display.

pub mod check;
pub mod exclude;
pub mod index;
pub mod note;
pub mod scan;

/// Top-level PARA bucket of a vault-relative path (`3-Projects/X/y.md` →
/// `3-Projects`). Empty for files at the vault root.
pub fn bucket_of(rel: &str) -> &str {
    match rel.split_once('/') {
        Some((top, _)) => top,
        None => "",
    }
}

/// Human label for a note path that disambiguates generic file names:
/// `3-Projects/Foo/index.md` → `Foo/index.md`. Other paths are returned
/// as the file name.
pub fn display_name(rel: &str) -> String {
    let mut parts = rel.rsplitn(3, '/');
    let file = parts.next().unwrap_or(rel);
    let parent = parts.next();
    if is_generic_name(file) {
        if let Some(parent) = parent {
            return format!("{parent}/{file}");
        }
    }
    file.to_string()
}

/// File names that many folders share (67 `index.md` files in one real
/// vault). Results and findings show their parent folder.
pub fn is_generic_name(file: &str) -> bool {
    let lower = file.to_lowercase();
    matches!(lower.as_str(), "index.md" | "readme.md" | "_index.md" | "overview.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_shows_parent_for_generic_names() {
        assert_eq!(display_name("3-Projects/Alpha/index.md"), "Alpha/index.md");
        assert_eq!(display_name("3-Projects/Alpha/notes.md"), "notes.md");
        assert_eq!(display_name("index.md"), "index.md");
        assert_eq!(bucket_of("3-Projects/Alpha/index.md"), "3-Projects");
        assert_eq!(bucket_of("Home.md"), "");
    }
}
