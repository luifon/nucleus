//! Vault walk shared by the index and the check (ADR-035).
//!
//! Symlinks are not followed: a link pointing outside the vault must not
//! pull foreign files into the index or into the check's fix scope.

use super::exclude::Exclusions;
use super::fsx::{self, Ident, Kind, Root};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone)]
pub struct VaultFile {
    /// Vault-relative path with `/` separators, NFC-normalized.
    pub rel: String,
    /// Vault-relative path as the names are stored on disk. Opened through
    /// [`Root`], never joined to the root and opened by path.
    pub raw_rel: PathBuf,
    /// Absolute path, for messages and tests. Not used to open the file.
    pub abs: PathBuf,
    pub size: u64,
    /// Modification time, unix seconds.
    pub mtime: i64,
    /// Modification time with nanoseconds, for the pre-fix identity check.
    pub mtime_ns: i128,
    /// Creation (birth) time, unix seconds, when the filesystem records it.
    pub birthtime: Option<i64>,
    /// Device and inode at scan time. A fix acts only on the same file.
    pub dev: u64,
    pub ino: u64,
}

impl VaultFile {
    pub fn from_ident(rel: String, raw_rel: PathBuf, abs: PathBuf, id: &Ident) -> Self {
        Self {
            rel,
            raw_rel,
            abs,
            size: id.size,
            mtime: id.mtime_ns.div_euclid(1_000_000_000) as i64,
            mtime_ns: id.mtime_ns,
            birthtime: id.birthtime,
            dev: id.dev,
            ino: id.ino,
        }
    }

    /// True when `id` describes the same file, unchanged, as this scan
    /// entry: a regular file with the same device and inode, size and
    /// nanosecond mtime.
    pub fn same_file(&self, id: &Ident) -> bool {
        id.kind == Kind::File
            && id.dev == self.dev
            && id.ino == self.ino
            && id.size == self.size
            && id.mtime_ns == self.mtime_ns
    }

    pub fn is_markdown(&self) -> bool {
        self.rel.to_lowercase().ends_with(".md")
    }

    pub fn file_name(&self) -> &str {
        self.rel.rsplit('/').next().unwrap_or(&self.rel)
    }
}

#[derive(Debug)]
pub struct Walk {
    /// The vault root the walk opened. Read the files through it
    /// ([`Root::read_note`] with [`VaultFile::raw_rel`]).
    pub root: Root,
    /// Files not excluded by a path glob, sorted by path.
    pub files: Vec<VaultFile>,
    /// Paths of files excluded by a path glob, outside dot folders. Kept
    /// only so a link that points at an excluded note still resolves; they
    /// are never read, reported, or indexed.
    pub excluded: Vec<String>,
}

impl Walk {
    /// Read a walked note through the root descriptor (no symlink is
    /// followed; see [`fsx`]).
    pub fn read_note(&self, f: &VaultFile) -> std::io::Result<Option<String>> {
        self.root.read_note(&f.raw_rel)
    }
}

/// Walk `root`, applying the path exclusions. Content exclusion happens
/// later, once a note has been read. Every folder is opened relative to
/// the root descriptor without following symlinks, and entries are listed
/// from the opened folder, so a folder swapped for a symlink during the
/// walk is skipped rather than followed.
pub fn walk(root: &Path, ex: &Exclusions) -> Result<Walk> {
    let dir = Root::open(root).with_context(|| format!("cannot read the vault at {}", root.display()))?;
    let mut out = Walk { root: dir, files: Vec::new(), excluded: Vec::new() };
    // (raw relative folder, NFC relative folder)
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(PathBuf::new(), PathBuf::new())];
    while let Some((raw_dir, rel_dir)) = stack.pop() {
        let entries = match out.root.open_dir(&raw_dir).and_then(fsx::list) {
            Ok(e) => e,
            // The root itself must be listable; a sub-folder that became a
            // symlink or vanished since it was listed is skipped.
            Err(e) if raw_dir.as_os_str().is_empty() => {
                return Err(e).with_context(|| format!("listing {}", root.display()));
            }
            Err(_) => continue,
        };
        for (raw_name, id) in entries {
            let name = raw_name.to_string_lossy().nfc().collect::<String>();
            let rel_path = rel_dir.join(&name);
            let raw_path = raw_dir.join(&raw_name);
            let rel = rel_path.to_string_lossy().replace('\\', "/");
            // Symlinks are never followed; dot entries are never content
            // (and never valid link targets).
            if id.kind == Kind::Symlink || name.starts_with('.') {
                continue;
            }
            match id.kind {
                Kind::Dir => {
                    if ex.path_excluded(&rel) {
                        collect_excluded(&out.root, &raw_path, &rel_path, &mut out.excluded);
                    } else {
                        stack.push((raw_path, rel_path));
                    }
                }
                Kind::File => {
                    if ex.path_excluded(&rel) {
                        out.excluded.push(rel);
                    } else {
                        let abs = root.join(&raw_path);
                        out.files.push(VaultFile::from_ident(rel, raw_path, abs, &id));
                    }
                }
                _ => {}
            }
        }
    }
    out.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    out.excluded.sort();
    Ok(out)
}

/// A value that changes when the search index would change: every walked
/// file's path, size, nanosecond mtime and inode, and the exclusion rules'
/// fingerprint. Reads no file content. Comparable only within one process
/// (the hash is not stable across builds).
pub fn watermark(root: &Path, ex: &Exclusions) -> Result<u64> {
    use std::hash::{Hash, Hasher};
    let w = walk(root, ex)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ex.fingerprint().hash(&mut h);
    for f in &w.files {
        (&f.rel, f.size, f.mtime_ns, f.dev, f.ino).hash(&mut h);
    }
    Ok(h.finish())
}

/// Record the file paths under an excluded folder (names only).
fn collect_excluded(root: &Root, raw_dir: &Path, rel_dir: &Path, out: &mut Vec<String>) {
    let mut stack = vec![(raw_dir.to_path_buf(), rel_dir.to_path_buf())];
    while let Some((raw, rel)) = stack.pop() {
        let Ok(entries) = root.open_dir(&raw).and_then(fsx::list) else { continue };
        for (raw_name, id) in entries {
            let name = raw_name.to_string_lossy().nfc().collect::<String>();
            if name.starts_with('.') {
                continue;
            }
            match id.kind {
                Kind::Dir => stack.push((raw.join(&raw_name), rel.join(&name))),
                Kind::File => out.push(rel.join(&name).to_string_lossy().replace('\\', "/")),
                _ => {}
            }
        }
    }
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

    /// A note or folder swapped for a symlink after the walk listed it is
    /// not read through the symlink: the read goes through the root
    /// descriptor with `O_NOFOLLOW` at every component.
    #[test]
    fn swap_after_walk_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        write(&vault, "a/n.md", "inside\n");
        write(&vault, "b/m.md", "inside\n");
        write(tmp.path(), "out/n.md", "OUTSIDE\n");
        write(tmp.path(), "out/m.md", "OUTSIDE\n");
        std::os::unix::fs::symlink(tmp.path().join("out"), vault.join("linked")).unwrap();
        let ex = Exclusions::new(&[], "").unwrap();
        let w = walk(&vault, &ex).unwrap();
        let rels: Vec<&str> = w.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["a/n.md", "b/m.md"], "symlinked folder listed");
        assert_eq!(w.read_note(&w.files[0]).unwrap().as_deref(), Some("inside\n"));

        // File swapped for a symlink to an outside file.
        fs::remove_file(vault.join("a/n.md")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("out/n.md"), vault.join("a/n.md")).unwrap();
        let e = w.read_note(&w.files[0]).unwrap_err();
        assert!(fsx::is_symlink_refusal(&e), "{e}");

        // Folder swapped for a symlink to an outside folder.
        fs::rename(vault.join("b"), vault.join("b-real")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("out"), vault.join("b")).unwrap();
        let e = w.read_note(&w.files[1]).unwrap_err();
        assert!(fsx::is_symlink_refusal(&e), "{e}");
    }

    #[test]
    fn watermark_tracks_notes_and_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path();
        write(vault, "a/n.md", "one\n");
        let ex = Exclusions::new(&[], "").unwrap();
        let w0 = watermark(vault, &ex).unwrap();
        assert_eq!(watermark(vault, &ex).unwrap(), w0);
        write(vault, "a/n.md", "one, edited\n");
        let w1 = watermark(vault, &ex).unwrap();
        assert_ne!(w1, w0);
        write(vault, "b/new.md", "x\n");
        let w2 = watermark(vault, &ex).unwrap();
        assert_ne!(w2, w1);
        let ex2 = Exclusions::new(&["b/**".to_string()], "").unwrap();
        assert_ne!(watermark(vault, &ex2).unwrap(), w2);
    }
}
