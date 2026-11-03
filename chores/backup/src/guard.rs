//! The HDD guard (ADR-039): before every backup, the filesystem that holds
//! the repository path on the homelab must be the configured one (by UUID)
//! and must not be the homelab's root filesystem.
//!
//! When the HDD is not mounted, its mount point is an ordinary directory on
//! the system disk. restic would then write the repository to the system
//! disk without an error, so the guard asks the homelab which filesystem
//! the path is on before restic runs, and refuses on any mismatch.

use anyhow::{Context, Result, bail};
use std::time::Duration;

/// An `sftp:[user@]host:/absolute/path` restic repository.
#[derive(Debug, Clone, PartialEq)]
pub struct SftpRepo {
    /// What `ssh` connects to: a `~/.ssh/config` alias or `user@host`.
    pub host: String,
    /// The repository's absolute path on that host.
    pub path: String,
}

/// Parse the repository string. Only the `sftp:host:/path` form is
/// accepted: the guard needs the absolute path, and the URL form
/// (`sftp://host//path`) has relative-path forms that would hide it.
pub fn parse_sftp_repo(repo: &str) -> Result<SftpRepo> {
    let repo = repo.trim();
    let Some(rest) = repo.strip_prefix("sftp:") else {
        bail!("NUCLEUS_BACKUP_REPOSITORY must be an sftp repository (sftp:host:/absolute/path)");
    };
    if rest.starts_with("//") {
        bail!("NUCLEUS_BACKUP_REPOSITORY: use the sftp:host:/absolute/path form, not sftp://");
    }
    let Some((host, path)) = rest.split_once(':') else {
        bail!("NUCLEUS_BACKUP_REPOSITORY: expected sftp:host:/absolute/path");
    };
    if host.is_empty() || host.starts_with('-') || host.chars().any(|c| c.is_whitespace()) {
        bail!("NUCLEUS_BACKUP_REPOSITORY: invalid host {host:?}");
    }
    if !path.starts_with('/') || path.split('/').any(|c| c == "..") {
        bail!("NUCLEUS_BACKUP_REPOSITORY: the path must be absolute and contain no `..`");
    }
    if path.trim_end_matches('/').is_empty() {
        bail!("NUCLEUS_BACKUP_REPOSITORY: the path cannot be /");
    }
    Ok(SftpRepo { host: host.to_string(), path: path.trim_end_matches('/').to_string() })
}

/// One `findmnt -P` line.
#[derive(Debug, Clone, PartialEq)]
pub struct Mount {
    pub uuid: String,
    pub source: String,
    pub target: String,
}

impl Mount {
    /// The block device without a btrfs subvolume suffix
    /// (`/dev/nvme0n1p6[/@/var]` → `/dev/nvme0n1p6`).
    pub fn device(&self) -> &str {
        self.source.split('[').next().unwrap_or(&self.source)
    }
}

/// Parse `KEY="value"` pairs as `findmnt -P` prints them (`\xHH` escapes
/// inside values).
pub fn parse_findmnt_pairs(line: &str) -> Result<Mount> {
    let mut uuid = None;
    let mut source = None;
    let mut target = None;
    let mut rest = line.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once("=\"").with_context(|| format!("unexpected findmnt output: {line:?}"))?;
        let end = after.find('"').with_context(|| format!("unterminated value in findmnt output: {line:?}"))?;
        let value = unescape_hex(&after[..end])?;
        match key.trim() {
            "UUID" => uuid = Some(value),
            "SOURCE" => source = Some(value),
            "TARGET" => target = Some(value),
            _ => {}
        }
        rest = after[end + 1..].trim_start();
    }
    match (uuid, source, target) {
        (Some(uuid), Some(source), Some(target)) => Ok(Mount { uuid, source, target }),
        _ => bail!("findmnt output lacks UUID, SOURCE or TARGET: {line:?}"),
    }
}

fn unescape_hex(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1] == b'x' {
            let hex = std::str::from_utf8(&bytes[i + 2..i + 4])?;
            out.push(u8::from_str_radix(hex, 16).with_context(|| format!("bad escape in {s:?}"))?);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(String::from_utf8(out)?)
}

/// The guard's decision.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// The repository path is on the configured filesystem.
    Pass(Mount),
    /// The path does not exist (typically: the HDD is not mounted and the
    /// repository directory lives on it).
    Missing,
    /// The path is on the homelab's root filesystem.
    RootFilesystem(Mount),
    /// The path is on another filesystem than the configured one.
    WrongFilesystem(Mount),
}

impl Verdict {
    pub fn passed(&self) -> bool {
        matches!(self, Verdict::Pass(_))
    }

    /// One code-owned line, safe for an alert (no paths or UUIDs).
    pub fn reason(&self) -> &'static str {
        match self {
            Verdict::Pass(_) => "on the HDD",
            Verdict::Missing => "the repository path does not exist on the homelab (HDD not mounted?)",
            Verdict::RootFilesystem(_) => "the repository path is on the homelab's system disk, not the HDD",
            Verdict::WrongFilesystem(_) => "the repository path is not on the configured HDD filesystem",
        }
    }
}

/// Decide from the two probes. The root check comes first: a configured
/// UUID that is (by mistake) the root filesystem's still refuses.
pub fn evaluate(repo: Option<&Mount>, root: &Mount, expected_uuid: &str) -> Verdict {
    let Some(repo) = repo else { return Verdict::Missing };
    let same_uuid = |a: &str, b: &str| !a.is_empty() && a.eq_ignore_ascii_case(b);
    if repo.target == "/" || same_uuid(&repo.uuid, &root.uuid) || repo.device() == root.device() {
        return Verdict::RootFilesystem(repo.clone());
    }
    if !same_uuid(&repo.uuid, expected_uuid.trim()) {
        return Verdict::WrongFilesystem(repo.clone());
    }
    Verdict::Pass(repo.clone())
}

const MISSING_MARK: &str = "NUCLEUS_BACKUP_PATH_MISSING";

/// Quote for a POSIX shell (the remote side of `ssh` runs the command
/// through one).
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The remote command: the repository path's filesystem (or a marker when
/// the path does not exist), then the root filesystem, one line each.
pub fn probe_command(path: &str) -> String {
    let q = shell_quote(path);
    format!(
        "if [ -d {q} ]; then findmnt -n -P -o UUID,SOURCE,TARGET --target {q}; else echo {MISSING_MARK}; fi; \
         findmnt -n -P -o UUID,SOURCE,TARGET --target /"
    )
}

/// Parse the probe's stdout into (repository mount, root mount).
pub fn parse_probe(out: &str) -> Result<(Option<Mount>, Mount)> {
    let lines: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() != 2 {
        bail!("unexpected guard probe output ({} lines)", lines.len());
    }
    let repo = if lines[0] == MISSING_MARK { None } else { Some(parse_findmnt_pairs(lines[0])?) };
    Ok((repo, parse_findmnt_pairs(lines[1])?))
}

/// Why the guard could not decide.
#[derive(Debug)]
pub enum ProbeError {
    /// ssh failed to connect or the remote command failed.
    Ssh(String),
    /// The output did not parse.
    Output(anyhow::Error),
}

/// Run the probe over SSH and decide.
pub async fn check(
    host: &str,
    path: &str,
    expected_uuid: &str,
    connect_timeout_secs: u32,
) -> std::result::Result<Verdict, ProbeError> {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", "-o", &format!("ConnectTimeout={connect_timeout_secs}"), "--", host])
        .arg(probe_command(path))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let limit = Duration::from_secs(u64::from(connect_timeout_secs) + 60);
    let out = match tokio::time::timeout(limit, cmd.output()).await {
        Err(_) => return Err(ProbeError::Ssh(format!("ssh did not finish within {}s", limit.as_secs()))),
        Ok(Err(e)) => return Err(ProbeError::Ssh(format!("could not start ssh: {e}"))),
        Ok(Ok(o)) => o,
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(ProbeError::Ssh(format!("ssh exited with {}: {}", out.status, stderr.trim())));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (repo, root) = parse_probe(&stdout).map_err(ProbeError::Output)?;
    Ok(evaluate(repo.as_ref(), &root, expected_uuid))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HDD: &str = "c0ffee00-1111-2222-3333-444455556666";
    const ROOT: &str = "deadbeef-aaaa-bbbb-cccc-ddddeeeeffff";

    fn m(uuid: &str, source: &str, target: &str) -> Mount {
        Mount { uuid: uuid.into(), source: source.into(), target: target.into() }
    }

    #[test]
    fn repository_strings() {
        assert_eq!(
            parse_sftp_repo("sftp:backup-host:/srv/backups/restic-nucleus").unwrap(),
            SftpRepo { host: "backup-host".into(), path: "/srv/backups/restic-nucleus".into() }
        );
        assert_eq!(parse_sftp_repo("sftp:root@10.0.0.2:/data/r/").unwrap().host, "root@10.0.0.2");
        assert_eq!(parse_sftp_repo("sftp:root@10.0.0.2:/data/r/").unwrap().path, "/data/r");
        assert!(parse_sftp_repo("/local/repo").is_err());
        assert!(parse_sftp_repo("sftp://host//srv/r").is_err());
        assert!(parse_sftp_repo("sftp:host:relative/r").is_err());
        assert!(parse_sftp_repo("sftp:host:/").is_err());
        assert!(parse_sftp_repo("sftp:host:/srv/../etc").is_err());
        assert!(parse_sftp_repo("sftp:-oProxyCommand=x:/srv").is_err());
        assert!(parse_sftp_repo("sftp::/srv").is_err());
    }

    #[test]
    fn findmnt_pairs() {
        let line = format!(r#"UUID="{HDD}" SOURCE="/dev/sdb1" TARGET="/srv/data""#);
        assert_eq!(parse_findmnt_pairs(&line).unwrap(), m(HDD, "/dev/sdb1", "/srv/data"));
        let sub = format!(r#"UUID="{ROOT}" SOURCE="/dev/nvme0n1p6[/@/var]" TARGET="/var""#);
        let sub = parse_findmnt_pairs(&sub).unwrap();
        assert_eq!(sub.device(), "/dev/nvme0n1p6");
        let esc = parse_findmnt_pairs(r#"UUID="" SOURCE="tmpfs" TARGET="/mnt/a\x20b""#).unwrap();
        assert_eq!(esc, m("", "tmpfs", "/mnt/a b"));
        assert!(parse_findmnt_pairs("garbage").is_err());
        assert!(parse_findmnt_pairs(r#"UUID="x" SOURCE="y""#).is_err());
    }

    #[test]
    fn verdicts() {
        let root = m(ROOT, "/dev/nvme0n1p6[/@/.snapshots/1/snapshot]", "/");
        let hdd = m(HDD, "/dev/sdb1", "/srv/data");
        assert!(evaluate(Some(&hdd), &root, HDD).passed());
        assert!(evaluate(Some(&hdd), &root, &HDD.to_uppercase()).passed(), "UUIDs compare case-insensitively");
        // HDD unmounted: the mount point is a directory on a subvolume of
        // the system disk.
        let nvme = m(ROOT, "/dev/nvme0n1p6[/@/srv]", "/srv");
        assert!(matches!(evaluate(Some(&nvme), &root, HDD), Verdict::RootFilesystem(_)));
        // The same, if the subvolume reported another UUID: same device.
        let nvme2 = m("other", "/dev/nvme0n1p6[/@/srv]", "/srv");
        assert!(matches!(evaluate(Some(&nvme2), &root, HDD), Verdict::RootFilesystem(_)));
        // A configured UUID that is the root filesystem's still refuses.
        assert!(matches!(evaluate(Some(&nvme), &root, ROOT), Verdict::RootFilesystem(_)));
        // Another disk that is not the configured one.
        let usb = m("1234-5678", "/dev/sdc1", "/mnt/usb");
        assert!(matches!(evaluate(Some(&usb), &root, HDD), Verdict::WrongFilesystem(_)));
        // A filesystem with no UUID never matches.
        let tmp = m("", "tmpfs", "/tmp");
        assert!(matches!(evaluate(Some(&tmp), &root, ""), Verdict::WrongFilesystem(_)));
        assert_eq!(evaluate(None, &root, HDD), Verdict::Missing);
    }

    #[test]
    fn probe_output() {
        let out = format!("UUID=\"{HDD}\" SOURCE=\"/dev/sdb1\" TARGET=\"/srv/data\"\nUUID=\"{ROOT}\" SOURCE=\"/dev/nvme0n1p6\" TARGET=\"/\"\n");
        let (repo, root) = parse_probe(&out).unwrap();
        assert_eq!(repo.unwrap().target, "/srv/data");
        assert_eq!(root.target, "/");
        let (repo, _) = parse_probe(&format!("{MISSING_MARK}\nUUID=\"{ROOT}\" SOURCE=\"/dev/x\" TARGET=\"/\"\n")).unwrap();
        assert!(repo.is_none());
        assert!(parse_probe("").is_err());
        assert!(parse_probe("one line only").is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/srv/a b"), "'/srv/a b'");
        assert_eq!(shell_quote("/srv/it's"), r"'/srv/it'\''s'");
        assert!(probe_command("/srv/x").contains("--target '/srv/x'"));
    }
}
