//! The executables Nucleus runs for the issue pipeline (ADR-036): `git` and
//! `gh`, resolved once to canonical absolute paths (symlinks followed to the
//! real file) and pinned by SHA-256.
//!
//! Every git and gh process intake starts goes through [`Pin::command`],
//! which checks the file's hash immediately before it spawns the process;
//! a changed file is a [`ToolChanged`] error and the pipeline blocks the
//! item. `git` is pinned once per process (the first use, or
//! [`pin_git_at`]); `gh` is pinned when the pipeline context opens, and
//! intake with a GitHub source refuses to start when `gh` cannot be found.
//!
//! The implementation agent runs as the same OS user, so it could replace
//! either file between two ticks, and the next tick would pin the replaced
//! file; only an OS sandbox or a separate OS identity closes that, and the
//! operator deferred it.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// A pinned executable changed (or became unreadable) since it was pinned.
#[derive(Debug)]
pub struct ToolChanged(pub String);

impl std::fmt::Display for ToolChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ToolChanged {}

/// One pinned executable: its canonical path and SHA-256.
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

    /// A command for this executable, created only after the hash check
    /// passed. The only way intake starts git or gh.
    pub fn command(&self) -> Result<tokio::process::Command> {
        self.verify().map_err(|why| anyhow::Error::new(ToolChanged(why)))?;
        Ok(tokio::process::Command::new(&self.path))
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

/// `name` as an absolute path: itself when absolute, else the first
/// executable file of that name on `PATH`.
pub fn resolve_bin(name: &str) -> Result<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        bail!("{name} does not exist");
    }
    if name.contains('/') || name.is_empty() {
        bail!("{name:?} is neither an absolute path nor a command name");
    }
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    for dir in std::env::split_paths(&path) {
        let c = dir.join(name);
        if c.is_file() && c.is_absolute() {
            return Ok(c);
        }
    }
    bail!("{name} is not on PATH")
}

static GIT: std::sync::OnceLock<std::result::Result<Pin, String>> = std::sync::OnceLock::new();

/// Pin `path` as this process's git. Only before the first git use; a
/// different path after that is an error.
pub fn pin_git_at(path: &Path) -> Result<()> {
    let want = Pin::new(path)?;
    let got = GIT.get_or_init(|| Ok(want.clone()));
    match got {
        Ok(p) if p.path == want.path => Ok(()),
        Ok(p) => bail!("git is already pinned at {}", p.path.display()),
        Err(e) => bail!("git: {e}"),
    }
}

/// This process's git: resolved through `PATH` and pinned at first use.
pub fn git_pin() -> Result<&'static Pin> {
    GIT.get_or_init(|| resolve_bin("git").and_then(|p| Pin::new(&p)).map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("git: {e}"))
}

/// The pipeline's pins: git (per process) and gh (per context).
#[derive(Debug, Clone)]
pub struct ToolPins {
    /// `None` only when intake has no GitHub source.
    pub gh: Option<Pin>,
}

impl ToolPins {
    /// Pin git and the configured `gh`. With `need_gh` (intake enabled
    /// with a GitHub repo), a `gh` that cannot be resolved is an error: the
    /// tick does not start, and nothing falls back to a bare name.
    pub fn pin(gh_bin: &str, need_gh: bool) -> Result<ToolPins> {
        git_pin()?;
        let gh = match resolve_bin(gh_bin) {
            Ok(p) => Some(Pin::new(&p)?),
            Err(e) if need_gh => {
                return Err(e.context(format!("intake cannot start: [intake.github] gh_bin {gh_bin:?} was not found")))
            }
            Err(_) => None,
        };
        Ok(ToolPins { gh })
    }

    /// `Err(reason)` when a pinned file changed since it was pinned.
    pub fn verify(&self) -> std::result::Result<(), String> {
        git_pin().map_err(|e| format!("{e:#}"))?.verify()?;
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
    fn a_changed_file_fails_verification_and_is_not_spawned() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("gh-real");
        std::fs::write(&real, "#!/bin/sh\necho one\n").unwrap();
        let link = d.path().join("gh");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let pin = Pin::new(&link).unwrap();
        assert_eq!(pin.path, real.canonicalize().unwrap(), "symlinks are followed to the real file");
        assert!(pin.verify().is_ok() && pin.command().is_ok());
        std::fs::write(&real, "#!/bin/sh\necho two\n").unwrap();
        assert_eq!(pin.verify().unwrap_err(), "executable-changed: gh-real");
        let e = pin.command().unwrap_err();
        assert!(e.downcast_ref::<ToolChanged>().is_some());
        assert!(ToolPins::pin("definitely-not-a-gh-binary", true).is_err(), "gh is required with a GitHub source");
        assert!(ToolPins::pin("definitely-not-a-gh-binary", false).unwrap().gh.is_none());
    }
}
