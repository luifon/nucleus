//! Descriptor-relative file access below the vault root (ADR-035).
//!
//! A path check followed by a path-based open has a gap: between the two,
//! another process can replace a file or a folder with a symlink, and the
//! open then follows it out of the vault. Everything here avoids that gap.
//! The vault root is opened once ([`Root::open`]); every path below it is
//! opened one component at a time with `openat(.., O_NOFOLLOW)` relative to
//! the descriptor of its parent folder. A symlink at any component below
//! the root is refused (`ELOOP` or `ENOTDIR`), never followed. A file is
//! checked with `fstat` on the descriptor that was opened, and read from
//! that descriptor; the path is not opened a second time.
//!
//! The root path itself is trusted configuration and may be a symlink.

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

/// Identity and size of a directory entry, read without following a
/// symlink (`fstatat(.., AT_SYMLINK_NOFOLLOW)`) or from an open descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ident {
    pub kind: Kind,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    /// Modification time in nanoseconds since the epoch.
    pub mtime_ns: i128,
    /// Birth time in unix seconds, where the filesystem records it.
    pub birthtime: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

impl Ident {
    // `stat` field types differ between platforms; the casts are needed on
    // some of them.
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(st: &rustix::fs::Stat) -> Self {
        let kind = match FileType::from_raw_mode(st.st_mode as _) {
            FileType::RegularFile => Kind::File,
            FileType::Directory => Kind::Dir,
            FileType::Symlink => Kind::Symlink,
            _ => Kind::Other,
        };
        #[cfg(target_vendor = "apple")]
        let birthtime = Some(st.st_birthtime as i64);
        #[cfg(not(target_vendor = "apple"))]
        let birthtime = None;
        Self {
            kind,
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            size: st.st_size as u64,
            mtime_ns: st.st_mtime as i128 * 1_000_000_000 + st.st_mtime_nsec as i128,
            birthtime,
        }
    }

    /// `fstat` on an open descriptor.
    pub fn of(fd: impl AsFd) -> std::io::Result<Self> {
        Ok(Self::from_stat(&rustix::fs::fstat(fd)?))
    }
}

/// `fstatat(dir, name, AT_SYMLINK_NOFOLLOW)`.
pub fn stat_at(dir: impl AsFd, name: &OsStr) -> std::io::Result<Ident> {
    Ok(Ident::from_stat(&rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?))
}

/// The vault root, opened once.
#[derive(Debug)]
pub struct Root {
    fd: OwnedFd,
}

/// True for the errors a refused symlink component produces.
pub fn is_symlink_refusal(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR))
}

fn normal_components(rel: &Path) -> std::io::Result<Vec<&OsStr>> {
    rel.components()
        .map(|c| match c {
            Component::Normal(s) => Ok(s),
            _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a plain relative path")),
        })
        .collect()
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

impl Root {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let fd = rustix::fs::open(path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
        Ok(Self { fd })
    }

    pub fn fd(&self) -> &OwnedFd {
        &self.fd
    }

    /// Open the folder `rel` (empty = the root), one component at a time
    /// without following symlinks.
    pub fn open_dir(&self, rel: &Path) -> std::io::Result<OwnedFd> {
        let mut cur = rustix::fs::openat(&self.fd, ".", DIR_FLAGS, Mode::empty())?;
        for c in normal_components(rel)? {
            cur = rustix::fs::openat(&cur, c, DIR_FLAGS, Mode::empty())?;
        }
        Ok(cur)
    }

    /// Open the regular file `rel` for reading without following a symlink
    /// at any component. Returns the file and its `fstat` identity; any
    /// other kind of entry is refused.
    pub fn open_file(&self, rel: &Path) -> std::io::Result<(std::fs::File, Ident)> {
        let parts = normal_components(rel)?;
        let (name, dirs) = parts
            .split_last()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty path"))?;
        let dir = self.open_dir(&dirs.iter().collect::<std::path::PathBuf>())?;
        open_file_at(&dir, name)
    }

    /// Read the note `rel` with the size ceiling of [`super::read_capped`].
    pub fn read_note(&self, rel: &Path) -> std::io::Result<Option<String>> {
        let (file, _) = self.open_file(rel)?;
        super::read_capped(file)
    }
}

/// Open the regular file `name` in `dir` without following a symlink.
/// `O_NONBLOCK` keeps a FIFO from blocking the open; the `fstat` check then
/// refuses it.
pub fn open_file_at(dir: impl AsFd, name: &OsStr) -> std::io::Result<(std::fs::File, Ident)> {
    let fd = rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let id = Ident::of(&fd)?;
    if id.kind != Kind::File {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok((std::fs::File::from(fd), id))
}

/// Rename `from` in `from_dir` to `to` in `to_dir` only if `to` does not
/// exist: `renameatx_np(.., RENAME_EXCL)` on macOS, `renameat2(..,
/// RENAME_NOREPLACE)` on Linux. The check and the rename are one system
/// call, so an entry created at `to` is never replaced. Fails with
/// `EEXIST` when `to` exists; see [`is_unsupported`] for platforms and
/// filesystems without the call.
pub fn rename_exclusive(
    from_dir: impl AsFd,
    from: &OsStr,
    to_dir: impl AsFd,
    to: &OsStr,
) -> std::io::Result<()> {
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    {
        rustix::fs::renameat_with(from_dir, from, to_dir, to, rustix::fs::RenameFlags::NOREPLACE)?;
        Ok(())
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    {
        let _ = (from_dir, from, to_dir, to);
        Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
    }
}

/// True when [`rename_exclusive`] failed because the platform or the
/// filesystem does not provide an exclusive rename.
pub fn is_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP)
    )
}

/// Names in the folder `dir`, without `.` and `..`, with each entry's
/// identity read without following symlinks. An entry that disappears
/// while it is listed is left out.
pub fn list(dir: impl AsFd) -> std::io::Result<Vec<(OsString, Ident)>> {
    let mut out = Vec::new();
    let dir_fd = dir.as_fd();
    for entry in Dir::read_from(dir_fd)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name == "." || name == ".." {
            continue;
        }
        match stat_at(dir_fd, name) {
            Ok(id) => out.push((name.to_os_string(), id)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn symlinks_below_the_root_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        fs::create_dir_all(vault.join("a/b")).unwrap();
        fs::write(vault.join("a/b/n.md"), "in").unwrap();
        fs::create_dir_all(tmp.path().join("out")).unwrap();
        fs::write(tmp.path().join("out/n.md"), "out").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("out/n.md"), vault.join("a/link.md")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("out"), vault.join("a/dirlink")).unwrap();

        let root = Root::open(&vault).unwrap();
        assert_eq!(root.read_note(Path::new("a/b/n.md")).unwrap().as_deref(), Some("in"));
        let e = root.open_file(Path::new("a/link.md")).unwrap_err();
        assert!(is_symlink_refusal(&e), "{e}");
        let e = root.open_file(Path::new("a/dirlink/n.md")).unwrap_err();
        assert!(is_symlink_refusal(&e), "{e}");
        assert!(root.open_file(Path::new("a/b")).is_err());
        assert!(root.open_file(Path::new("../out/n.md")).is_err());

        let names: Vec<(String, Kind)> = {
            let mut v: Vec<_> = list(root.open_dir(Path::new("a")).unwrap())
                .unwrap()
                .into_iter()
                .map(|(n, id)| (n.to_string_lossy().into_owned(), id.kind))
                .collect();
            v.sort_by(|x, y| x.0.cmp(&y.0));
            v
        };
        assert_eq!(
            names,
            vec![("b".into(), Kind::Dir), ("dirlink".into(), Kind::Symlink), ("link.md".into(), Kind::Symlink)]
        );
    }
}
