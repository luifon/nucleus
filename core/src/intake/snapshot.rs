//! A private copy of the agent's clone that Nucleus imports (ADR-036).
//!
//! The limits on an import are enforced while reading, not only checked:
//! every path is opened relative to its parent directory's descriptor with
//! `O_NOFOLLOW` (no path component may be a symlink), a regular file is
//! checked on its opened descriptor (`fstat`: regular, one hard link) and
//! copied by streaming with a hard byte limit counted as it is read, and a
//! symlink is recreated as a symlink from `readlinkat`. FIFOs, sockets and
//! devices are refused. Paths stay bytes end to end (Unix `OsStr`), so a
//! file name that is not UTF-8 is imported unchanged.

use super::git::{ImportLimits, ImportRefused};
use anyhow::{Context, Result};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

macro_rules! refuse {
    ($($t:tt)*) => {
        return Err(anyhow::Error::new(ImportRefused(format!($($t)*))))
    };
}

/// Bytes counted across one snapshot.
pub struct Budget {
    pub files: usize,
    pub bytes: u64,
}

fn show(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

/// Copy `r` into `w`, refusing as soon as more than `max_file` bytes, or
/// more than `left_total` bytes, have been read. Returns the bytes copied.
/// The count is of bytes read, so a file that grows while it is copied is
/// refused when it passes the limit.
pub fn copy_limited(mut r: impl Read, mut w: impl Write, name: &[u8], max_file: u64, left_total: u64) -> Result<u64> {
    let mut buf = vec![0u8; 1 << 16];
    let mut copied: u64 = 0;
    loop {
        let n = r.read(&mut buf).with_context(|| format!("reading {}", show(name)))?;
        if n == 0 {
            return Ok(copied);
        }
        copied += n as u64;
        if copied > max_file {
            refuse!("{} is larger than the per-file limit of {max_file} bytes; nothing was imported", show(name));
        }
        if copied > left_total {
            refuse!("the clone's files exceed the total limit; nothing was imported");
        }
        w.write_all(&buf[..n])?;
    }
}

fn open_dir(parent: &OwnedFd, name: &[u8]) -> rustix::io::Result<OwnedFd> {
    rustix::fs::openat(parent, OsStr::from_bytes(name), OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
}

/// Copy one listed path (relative, `/`-separated bytes) from the clone
/// (`root`) into the snapshot (`dest_root`). A path that no longer exists
/// is skipped (a deletion). `gitlink_dir` marks a path that is a submodule
/// of the base: it is recreated as an empty directory when it holds no
/// repository, and refused when it does (a submodule change).
pub fn copy_path(root: &OwnedFd, rel: &[u8], dest_root: &Path, limits: &ImportLimits, budget: &mut Budget, gitlink_dir: bool) -> Result<()> {
    let parts: Vec<&[u8]> = rel.split(|b| *b == b'/').filter(|p| !p.is_empty()).collect();
    let Some((leaf, dirs)) = parts.split_last() else { return Ok(()) };
    if parts.iter().any(|p| *p == b"." || *p == b".." || *p == b".git") {
        refuse!("{} is not a path Nucleus imports; nothing was imported", show(rel));
    }
    let mut cur = open_dir(root, b".").context("opening the clone")?;
    let mut dest = dest_root.to_path_buf();
    for d in dirs {
        cur = match open_dir(&cur, d) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(rustix::io::Errno::LOOP) | Err(rustix::io::Errno::NOTDIR) => {
                refuse!("a parent directory of {} is a symlink or not a directory; nothing was imported", show(rel))
            }
            Err(e) => return Err(e).with_context(|| format!("opening a parent of {}", show(rel))),
        };
        dest.push(OsStr::from_bytes(d));
        std::fs::create_dir_all(&dest)?;
    }
    let target = dest.join(OsStr::from_bytes(leaf));
    let st = match rustix::fs::statat(&cur, OsStr::from_bytes(leaf), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", show(rel))),
    };
    let count = |budget: &mut Budget| -> Result<()> {
        budget.files += 1;
        if budget.files > limits.max_files {
            refuse!("the clone has more than {} files; nothing was imported", limits.max_files);
        }
        Ok(())
    };
    match FileType::from_raw_mode(st.st_mode) {
        FileType::Symlink => {
            count(budget)?;
            let link = rustix::fs::readlinkat(&cur, OsStr::from_bytes(leaf), Vec::new())?;
            let len = link.as_bytes().len() as u64;
            if len > limits.max_file_bytes {
                refuse!("the symlink {} has a target longer than the per-file limit; nothing was imported", show(rel));
            }
            match budget.bytes.checked_add(len) {
                Some(t) if t <= limits.max_total_bytes => budget.bytes = t,
                _ => refuse!("the clone's files exceed the total limit; nothing was imported"),
            }
            std::os::unix::fs::symlink(OsStr::from_bytes(link.as_bytes()), &target)?;
            Ok(())
        }
        FileType::Directory => {
            let fd = open_dir(&cur, leaf)?;
            let has_repo = rustix::fs::statat(&fd, ".git", AtFlags::SYMLINK_NOFOLLOW).is_ok();
            if gitlink_dir && !has_repo {
                std::fs::create_dir_all(&target)?;
                return Ok(());
            }
            if gitlink_dir {
                refuse!("the change touches the submodule {}; Nucleus does not publish submodule changes", show(rel));
            }
            refuse!("the clone contains a nested repository at {}; Nucleus does not publish one", show(rel))
        }
        FileType::Fifo | FileType::Socket | FileType::CharacterDevice | FileType::BlockDevice => {
            refuse!("{} is not a regular file or a symlink (a FIFO, socket or device); nothing was imported", show(rel))
        }
        _ => {
            let fd = match rustix::fs::openat(
                &cur,
                OsStr::from_bytes(leaf),
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT) => return Ok(()),
                Err(e) => refuse!("{} cannot be opened as a regular file ({e}); nothing was imported", show(rel)),
            };
            let fst = rustix::fs::fstat(&fd)?;
            if FileType::from_raw_mode(fst.st_mode) != FileType::RegularFile {
                refuse!("{} is not a regular file or a symlink (a FIFO, socket or device); nothing was imported", show(rel));
            }
            if fst.st_nlink > 1 {
                refuse!("{} has more than one hard link; nothing was imported", show(rel));
            }
            count(budget)?;
            let out = std::fs::File::create(&target)?;
            let left = limits.max_total_bytes.saturating_sub(budget.bytes);
            let n = copy_limited(std::fs::File::from(fd), &out, rel, limits.max_file_bytes, left)?;
            budget.bytes = budget.bytes.checked_add(n).context("byte count overflow")?;
            let exec = fst.st_mode & 0o111 != 0;
            std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(if exec { 0o755 } else { 0o644 }))?;
            Ok(())
        }
    }
}

/// Open the directory `rel` (bytes, `/`-separated; empty for the root)
/// below `root`, one component at a time with `O_NOFOLLOW`. A component
/// that is a symlink or not a directory is refused.
pub fn open_rel_dir(root: &OwnedFd, rel: &[u8]) -> Result<OwnedFd> {
    let mut cur = open_dir(root, b".").context("opening the clone")?;
    for d in rel.split(|b| *b == b'/').filter(|p| !p.is_empty()) {
        cur = match open_dir(&cur, d) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::LOOP) | Err(rustix::io::Errno::NOTDIR) => {
                refuse!("{} is a symlink or not a directory; nothing was imported", show(rel))
            }
            Err(e) => return Err(e).with_context(|| format!("opening {}", show(rel))),
        };
    }
    Ok(cur)
}

/// What the walk found in one directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    Special,
}

/// The entries of directory `dir` (names as bytes; `.`, `..` and `.git`
/// left out), each with its type from `statat` without following symlinks,
/// and whether the directory holds a `.git` entry (a repository).
pub fn read_entries(dir: &OwnedFd) -> Result<(Vec<(Vec<u8>, Kind)>, bool)> {
    let mut out = Vec::new();
    let mut has_git = false;
    let mut d = rustix::fs::Dir::read_from(dir)?;
    while let Some(e) = d.read() {
        let e = e?;
        let name = e.file_name().to_bytes().to_vec();
        if name == b"." || name == b".." {
            continue;
        }
        if name == b".git" {
            has_git = true;
            continue;
        }
        let st = match rustix::fs::statat(dir, OsStr::from_bytes(&name), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(e) => return Err(e).context("reading a directory entry"),
        };
        let kind = match FileType::from_raw_mode(st.st_mode) {
            FileType::Directory => Kind::Dir,
            FileType::Symlink => Kind::Symlink,
            FileType::RegularFile => Kind::File,
            _ => Kind::Special,
        };
        out.push((name, kind));
    }
    Ok((out, has_git))
}

/// Copy the ignore file `name` of directory `dir` (at `rel` in the clone)
/// into the rules directory, with its own byte limit (`max`), counted
/// toward the total budget. Checked on the opened descriptor: a regular
/// file with one hard link.
pub fn copy_ignore_file(dir: &OwnedFd, rel: &[u8], dest: &Path, max: u64, limits: &ImportLimits, budget: &mut Budget) -> Result<()> {
    let name = rel.rsplit(|b| *b == b'/').next().unwrap_or(rel);
    let fd = match rustix::fs::openat(
        dir,
        OsStr::from_bytes(name),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(e) => refuse!("{} cannot be opened as a regular file ({e}); nothing was imported", show(rel)),
    };
    let fst = rustix::fs::fstat(&fd)?;
    if FileType::from_raw_mode(fst.st_mode) != FileType::RegularFile || fst.st_nlink > 1 {
        refuse!("{} is not a regular file with one link; nothing was imported", show(rel));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let out = std::fs::File::create(dest)?;
    let left = limits.max_total_bytes.saturating_sub(budget.bytes);
    let n = copy_limited(std::fs::File::from(fd), &out, rel, max, left)?;
    budget.bytes = budget.bytes.checked_add(n).context("byte count overflow")?;
    Ok(())
}

/// Open the clone's root directory (refused when it is a symlink).
pub fn open_root(wt: &Path) -> Result<OwnedFd> {
    rustix::fs::openat(rustix::fs::CWD, wt, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
        .map_err(|_| anyhow::Error::new(ImportRefused(format!("the item's clone {} is not a directory (a symlink or a file)", wt.display()))))
}

/// A snapshot path for `rel`.
pub fn dest_of(root: &Path, rel: &[u8]) -> PathBuf {
    root.join(OsStr::from_bytes(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that never ends (a file that keeps growing while read).
    struct Growing;
    impl Read for Growing {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            buf.fill(b'g');
            Ok(buf.len())
        }
    }

    #[test]
    fn symlink_targets_count_against_the_limits() {
        let d = tempfile::tempdir().unwrap();
        let clone = d.path().join("clone");
        let snap = d.path().join("snap");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::create_dir_all(&snap).unwrap();
        let long = "x".repeat(900);
        std::os::unix::fs::symlink(&long, clone.join("long")).unwrap();
        std::os::unix::fs::symlink("short", clone.join("short")).unwrap();
        let root = open_root(&clone).unwrap();
        let limits = ImportLimits { max_files: 10, max_file_bytes: 500, max_total_bytes: 1000, max_ignore_bytes: 100 };
        let mut b = Budget { files: 0, bytes: 0 };
        let e = copy_path(&root, b"long", &snap, &limits, &mut b, false).unwrap_err();
        assert!(e.downcast_ref::<ImportRefused>().unwrap().0.contains("longer than the per-file limit"));
        assert!(std::fs::symlink_metadata(snap.join("long")).is_err(), "refused before the symlink was created");
        let mut b = Budget { files: 0, bytes: 998 };
        let e = copy_path(&root, b"short", &snap, &limits, &mut b, false).unwrap_err();
        assert!(e.downcast_ref::<ImportRefused>().unwrap().0.contains("total limit"));
        assert!(std::fs::symlink_metadata(snap.join("short")).is_err());
        let mut b = Budget { files: 0, bytes: 0 };
        copy_path(&root, b"short", &snap, &limits, &mut b, false).unwrap();
        assert_eq!(b.bytes, 5);
    }

    #[test]
    fn a_file_that_grows_while_copied_is_refused() {
        let mut sink = Vec::new();
        let e = copy_limited(Growing, &mut sink, b"log.txt", 100_000, u64::MAX).unwrap_err();
        assert!(e.downcast_ref::<ImportRefused>().unwrap().0.contains("log.txt is larger than the per-file limit"));
        assert!(sink.len() <= 100_000 + (1 << 16), "copying stopped at the limit");
        let e = copy_limited(Growing, Vec::new(), b"x", u64::MAX, 50_000).unwrap_err();
        assert!(e.downcast_ref::<ImportRefused>().unwrap().0.contains("total limit"));
        assert_eq!(copy_limited(&b"small"[..], Vec::new(), b"x", 10, 10).unwrap(), 5);
    }
}
