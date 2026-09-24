//! Vault walk shared by the index and the check (ADR-035).
//!
//! Symlinks are not followed: a link pointing outside the vault must not
//! pull foreign files into the index or into the check's fix scope.

use super::exclude::Exclusions;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone)]
pub struct VaultFile {
    /// Vault-relative path with `/` separators, NFC-normalized.
    pub rel: String,
    pub abs: PathBuf,
    pub size: u64,
    /// Modification time, unix seconds.
    pub mtime: i64,
    /// Creation (birth) time, unix seconds, when the filesystem records it.
    pub birthtime: Option<i64>,
}

impl VaultFile {
    pub fn is_markdown(&self) -> bool {
        self.rel.to_lowercase().ends_with(".md")
    }

    pub fn file_name(&self) -> &str {
        self.rel.rsplit('/').next().unwrap_or(&self.rel)
    }
}

#[derive(Debug, Default)]
pub struct Walk {
    /// Files not excluded by a path glob, sorted by path.
    pub files: Vec<VaultFile>,
    /// Paths of files excluded by a path glob, outside dot folders. Kept
    /// only so a link that points at an excluded note still resolves; they
    /// are never read, reported, or indexed.
    pub excluded: Vec<String>,
}

/// Walk `root`, applying the path exclusions. Content exclusion happens
/// later, once a note has been read.
pub fn walk(root: &Path, ex: &Exclusions) -> Result<Walk> {
    let meta = std::fs::metadata(root)
        .with_context(|| format!("cannot read the vault at {}", root.display()))?;
    if !meta.is_dir() {
        anyhow::bail!("vault path {} is not a directory", root.display());
    }
    let mut out = Walk::default();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        let dir = root.join(&rel_dir);
        let entries = std::fs::read_dir(&dir)
            .with_context(|| format!("listing {}", dir.display()))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().nfc().collect::<String>();
            let rel_path = rel_dir.join(&name);
            let rel = rel_path.to_string_lossy().replace('\\', "/");
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            // Dot entries are never content (and never valid link targets).
            if name.starts_with('.') {
                continue;
            }
            if ft.is_dir() {
                if ex.path_excluded(&rel) {
                    collect_excluded(root, &rel_path, &mut out.excluded);
                } else {
                    stack.push(rel_path);
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            if ex.path_excluded(&rel) {
                out.excluded.push(rel);
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            out.files.push(VaultFile {
                rel,
                abs: entry.path(),
                size: meta.len(),
                mtime: unix(meta.modified().ok()),
                birthtime: meta.created().ok().map(|t| unix(Some(t))),
            });
        }
    }
    out.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    out.excluded.sort();
    Ok(out)
}

/// Record the file paths under an excluded folder (names only).
fn collect_excluded(root: &Path, rel_dir: &Path, out: &mut Vec<String>) {
    let mut stack = vec![rel_dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&d)) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().nfc().collect::<String>();
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let rel = d.join(&name);
            if ft.is_dir() {
                stack.push(rel);
            } else if ft.is_file() {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

fn unix(t: Option<std::time::SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
