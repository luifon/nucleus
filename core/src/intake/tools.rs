//! The executables Nucleus runs for privileged steps (ADR-036): `git` and
//! `gh`, resolved once to canonical absolute paths (symlinks followed to the
//! real file) and pinned by SHA-256 when the process starts.
//!
//! The implementation agent runs as the same OS user, so it could replace
//! either file, or a directory early on `PATH`. Every privileged network
//! step (fetch, push, pull request, comment) first checks that the pinned
//! files still have the hash they had when this process started; a change
//! blocks the item. This does not stop a same-user worker from replacing an
//! executable between two ticks (the next tick pins the replaced file); that
//! needs the OS sandbox or a separate OS identity, which the operator
//! deferred.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// One pinned executable.
#[derive(Debug, Clone)]
pub struct Pin {
    pub path: PathBuf,
    sha256: String,
}

impl Pin {
    pub fn new(path: &Path) -> Result<Pin> {
        let path = path.canonicalize().with_context(|| format!("resolving {}", path.display()))?;
        let sha256 = file_sha256(&path)?;
        Ok(Pin { path, sha256 })
    }

    /// `Err(reason)` when the file changed or cannot be read.
    pub fn verify(&self) -> std::result::Result<(), String> {
        match file_sha256(&self.path) {
            Ok(h) if h == self.sha256 => Ok(()),
            Ok(_) => Err(format!("executable-changed: {}", name(&self.path))),
            Err(_) => Err(format!("executable-unreadable: {}", name(&self.path))),
        }
    }
}

fn name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn file_sha256(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// `git` and `gh` as this process runs them.
#[derive(Debug, Clone)]
pub struct ToolPins {
    pub git: Pin,
    /// `None` when `gh` is not installed (only local test remotes work then).
    pub gh: Option<Pin>,
}

impl ToolPins {
    /// Pin the `git` that [`super::git`] runs and the configured `gh`.
    pub fn pin(gh_bin: &str) -> Result<ToolPins> {
        let git = Pin::new(super::git::git_bin()?)?;
        let gh = super::git::resolve_bin(gh_bin).ok().map(|p| Pin::new(&p)).transpose()?;
        Ok(ToolPins { git, gh })
    }

    /// The pinned `gh` path, or the configured value when none is pinned.
    pub fn gh_path(&self, configured: &str) -> String {
        self.gh.as_ref().map(|p| p.path.to_string_lossy().into_owned()).unwrap_or_else(|| configured.to_string())
    }

    /// `Err(reason)` when a pinned file changed since this process started.
    pub fn verify(&self) -> std::result::Result<(), String> {
        self.git.verify()?;
        if let Some(gh) = &self.gh {
            gh.verify()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_file_fails_verification() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("gh-real");
        std::fs::write(&real, "#!/bin/sh\necho one\n").unwrap();
        let link = d.path().join("gh");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let pin = Pin::new(&link).unwrap();
        assert_eq!(pin.path, real.canonicalize().unwrap(), "symlinks are followed to the real file");
        assert!(pin.verify().is_ok());
        std::fs::write(&real, "#!/bin/sh\necho two\n").unwrap();
        assert_eq!(pin.verify().unwrap_err(), "executable-changed: gh-real");
        let pins = ToolPins::pin("definitely-not-a-gh-binary").unwrap();
        assert!(pins.gh.is_none() && pins.git.path.is_absolute());
    }
}
