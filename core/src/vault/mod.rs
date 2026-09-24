//! Vault search and vault check (ADR-035).
//!
//! The Obsidian vault (T3, ADR-005) is plain markdown on disk. Before this
//! module, a process could only see it as a truncated folder tree and read
//! notes one by one. This module gives every process two capabilities:
//!
//! - [`index`] — an FTS5 index over the notes at `memory/vault_index.db`,
//!   written only by the `vault-search` command through
//!   [`index::Writer`] (ADR-020). Updated incrementally by mtime/size
//!   before every query.
//! - [`check`] — a deterministic structural check (duplicates, broken
//!   links, orphans, stale inbox, frontmatter, source vocabulary, empty
//!   files) with one safe fix (empty untitled files to a quarantine), and a run history at
//!   `memory/vault_check.db` (also written only by this module).
//!
//! - [`access`] — what the dashboard may open and list.
//!
//! All start from the same walk ([`scan`]) and the same exclusion rules
//! ([`exclude`]). Excluded files are never indexed, never returned by a
//! search, never listed in a check report, never modified, and never opened
//! or listed by the dashboard. The
//! exclusion floor keeps credential notes out: credentials reach the
//! operator only through the authenticated DM, never through a search
//! result that a session or the dashboard could display.

pub mod access;
pub mod check;
pub mod exclude;
pub mod fsx;
pub mod index;
pub mod note;
pub mod scan;

/// Largest note the index, the check and the dashboard read. A larger
/// Markdown file (an export or a pasted log synced into the vault) is
/// skipped and counted, not read into memory; the dashboard refuses to
/// open it.
pub const MAX_NOTE_BYTES: u64 = 2 * 1024 * 1024;

/// Read an open note of at most [`MAX_NOTE_BYTES`]. `Ok(None)` when the
/// file is larger (checked before and while reading, so a file that grows
/// is still capped); an error when it cannot be read or is not UTF-8.
/// Notes are opened through [`fsx::Root`], never by path.
pub fn read_capped(mut file: std::fs::File) -> std::io::Result<Option<String>> {
    use std::io::Read;
    if file.metadata()?.len() > MAX_NOTE_BYTES {
        return Ok(None);
    }
    let mut buf = Vec::new();
    (&mut file).take(MAX_NOTE_BYTES + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_NOTE_BYTES {
        return Ok(None);
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

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

/// File names that many folders share. Results and findings show their
/// parent folder.
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
