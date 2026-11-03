//! restic invocations and the parsing of `restic backup --json`.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The tag on every snapshot this job makes; `forget` acts on it only.
pub const TAG: &str = "nucleus";

/// How to reach the repository.
#[derive(Debug, Clone)]
pub struct Restic {
    pub bin: String,
    pub repository: String,
    pub keychain_service: String,
    pub keychain_account: String,
    pub ssh_connect_timeout_secs: u32,
}

/// Keychain names go into restic's `--password-command`, which restic
/// splits on spaces; only plain names are accepted.
pub fn valid_keychain_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Restic {
    pub fn new(
        bin: &str,
        repository: &str,
        keychain_service: &str,
        keychain_account: &str,
        ssh_connect_timeout_secs: u32,
    ) -> Result<Self> {
        if !valid_keychain_name(keychain_service) || !valid_keychain_name(keychain_account) {
            bail!("[backup] keychain_service and keychain_account may hold only letters, digits, `.`, `_` and `-`");
        }
        Ok(Self {
            bin: bin.to_string(),
            repository: repository.to_string(),
            keychain_service: keychain_service.to_string(),
            keychain_account: keychain_account.to_string(),
            ssh_connect_timeout_secs,
        })
    }

    /// The options every command shares. The password is read by restic
    /// from the login Keychain; it is never in this process's arguments,
    /// environment or logs.
    pub fn base_args(&self) -> Vec<String> {
        vec![
            "--repo".into(),
            self.repository.clone(),
            "--password-command".into(),
            format!(
                "/usr/bin/security find-generic-password -s {} -a {} -w",
                self.keychain_service, self.keychain_account
            ),
            "--option".into(),
            format!("sftp.args=-o BatchMode=yes -o ConnectTimeout={}", self.ssh_connect_timeout_secs),
            // Wait for a lock another restic process holds instead of
            // failing (a manual run during the scheduled one).
            "--retry-lock".into(),
            "10m".into(),
        ]
    }

    pub fn command(&self, args: &[String]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.args(self.base_args()).args(args).stdin(Stdio::null()).kill_on_drop(true);
        cmd
    }

    /// Run and capture (stdout, stderr, exit code).
    pub async fn output(&self, args: &[String]) -> Result<Captured> {
        let out = self
            .command(args)
            .output()
            .await
            .with_context(|| format!("starting {} (is restic installed and in PATH?)", self.bin))?;
        Ok(Captured {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code(),
        })
    }

    /// Run with the terminal attached (operator commands).
    pub async fn interactive(&self, args: &[String]) -> Result<Option<i32>> {
        let status = self
            .command(args)
            .stdin(Stdio::inherit())
            .status()
            .await
            .with_context(|| format!("starting {}", self.bin))?;
        Ok(status.code())
    }
}

#[derive(Debug)]
pub struct Captured {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
}

pub fn backup_args(host: &str, exclude_file: &Path, roots: &[PathBuf]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "backup".into(),
        "--json".into(),
        "--host".into(),
        host.into(),
        "--tag".into(),
        TAG.into(),
        "--exclude-file".into(),
        exclude_file.to_string_lossy().into_owned(),
        "--".into(),
    ];
    args.extend(roots.iter().map(|r| r.to_string_lossy().into_owned()));
    args
}

pub fn forget_args(host: &str, daily: u32, weekly: u32, monthly: u32) -> Vec<String> {
    vec![
        "forget".into(),
        "--quiet".into(),
        "--host".into(),
        host.into(),
        "--tag".into(),
        TAG.into(),
        // The root list changes when a configured path appears or
        // disappears; grouping by paths would then keep every snapshot.
        "--group-by".into(),
        "host,tags".into(),
        "--keep-daily".into(),
        daily.to_string(),
        "--keep-weekly".into(),
        weekly.to_string(),
        "--keep-monthly".into(),
        monthly.to_string(),
        "--prune".into(),
    ]
}

pub fn check_args(subset: &str) -> Vec<String> {
    vec!["check".into(), "--quiet".into(), "--read-data-subset".into(), subset.into()]
}

/// The final message of `restic backup --json`.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Summary {
    #[serde(default)]
    pub files_new: u64,
    #[serde(default)]
    pub files_changed: u64,
    #[serde(default)]
    pub files_unmodified: u64,
    #[serde(default)]
    pub data_added: u64,
    #[serde(default)]
    pub total_files_processed: u64,
    #[serde(default)]
    pub total_bytes_processed: u64,
    #[serde(default)]
    pub total_duration: f64,
    #[serde(default)]
    pub snapshot_id: Option<String>,
}

/// A file restic could not read during the backup.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemError {
    pub item: String,
    pub during: String,
    pub message: String,
}

impl ItemError {
    /// The file was deleted between restic's directory listing and its read
    /// (a transcript or lock file removed during the run). Not a loss.
    pub fn vanished(&self) -> bool {
        matches!(self.during.as_str(), "archival" | "scan")
            && self.message.to_ascii_lowercase().ends_with(": no such file or directory")
    }
}

/// What `restic backup --json` reported.
#[derive(Debug, Default)]
pub struct BackupOutput {
    pub summary: Option<Summary>,
    /// Per-file errors (`"message_type":"error"`).
    pub errors: Vec<ItemError>,
    /// Other stderr lines (plain-text warnings, `exit_error` messages).
    pub other: Vec<String>,
}

/// Parse `restic backup --json`. restic 0.19 writes the summary to stdout
/// and the per-file errors, the `exit_error` message and plain-text
/// warnings to stderr; both streams are read and status lines dropped.
pub fn parse_backup_output(stdout: &str, stderr: &str) -> BackupOutput {
    let mut out = BackupOutput::default();
    for line in stdout.lines().chain(stderr.lines()) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            out.other.push(line.to_string());
            continue;
        };
        let field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        match v.get("message_type").and_then(|t| t.as_str()) {
            Some("summary") => out.summary = serde_json::from_value(v.clone()).ok(),
            Some("error") => {
                let message = match v.get("error") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(o) => o.get("message").and_then(|m| m.as_str()).unwrap_or_default().to_string(),
                    None => String::new(),
                };
                out.errors.push(ItemError { item: field("item"), during: field("during"), message });
            }
            Some("exit_error") => out.other.push(field("message")),
            Some("status") | Some("verbose_status") => {}
            _ => out.other.push(line.to_string()),
        }
    }
    out
}

/// Human-readable byte count for logs.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_names() {
        assert!(valid_keychain_name("nucleus-backup"));
        assert!(!valid_keychain_name("a b"));
        assert!(!valid_keychain_name(""));
        assert!(Restic::new("restic", "sftp:h:/r", "a;b", "restic", 20).is_err());
    }

    #[test]
    fn base_args_name_the_keychain_item_not_the_password() {
        let r = Restic::new("restic", "sftp:h:/r", "nucleus-backup", "restic", 20).unwrap();
        let a = r.base_args();
        assert_eq!(a[1], "sftp:h:/r");
        assert_eq!(a[3], "/usr/bin/security find-generic-password -s nucleus-backup -a restic -w");
        assert!(a.contains(&"sftp.args=-o BatchMode=yes -o ConnectTimeout=20".to_string()));
    }

    #[test]
    fn backup_and_forget_args() {
        let a = backup_args("nucleus", Path::new("/s/excludes"), &[PathBuf::from("/w/memory"), PathBuf::from("/s/sqlite")]);
        assert_eq!(a.last().unwrap(), "/s/sqlite");
        assert!(a.windows(2).any(|w| w == ["--tag", "nucleus"]));
        let f = forget_args("nucleus", 7, 4, 12);
        assert!(f.windows(2).any(|w| w == ["--group-by", "host,tags"]));
        assert!(f.windows(2).any(|w| w == ["--keep-monthly", "12"]));
        assert_eq!(f.last().unwrap(), "--prune");
    }

    /// The stream layout of restic 0.19.1, captured from a throwaway
    /// repository (an unreadable file and a missing source root, exit 3).
    #[test]
    fn parses_summary_and_errors() {
        let stdout = r#"{"message_type":"status","percent_done":0.5}
{"message_type":"summary","files_new":3,"files_changed":1,"files_unmodified":10,"data_added":2048,"data_added_packed":1516,"total_files_processed":14,"total_bytes_processed":4096,"total_duration":1.5,"snapshot_id":"abcdef12"}"#;
        let stderr = r#"/t/src/gone does not exist, skipping
{"message_type":"error","error":{"message":"open /t/src/secret: permission denied"},"during":"archival","item":"/t/src/secret"}
{"message_type":"error","error":{"message":"lstat /t/memory/x.lock: no such file or directory"},"during":"archival","item":"/t/memory/x.lock"}
{"message_type":"exit_error","code":3,"message":"Warning: at least one source file could not be read"}"#;
        let o = parse_backup_output(stdout, stderr);
        let s = o.summary.unwrap();
        assert_eq!(s.files_new, 3);
        assert_eq!(s.snapshot_id.as_deref(), Some("abcdef12"));
        assert_eq!(o.errors.len(), 2);
        assert!(!o.errors[0].vanished());
        assert_eq!(o.errors[0].item, "/t/src/secret");
        assert!(o.errors[1].vanished());
        assert_eq!(
            o.other,
            vec!["/t/src/gone does not exist, skipping", "Warning: at least one source file could not be read"]
        );
        // Errors on stdout (older layouts) are read too.
        let o = parse_backup_output(r#"{"message_type":"error","error":"plain","item":"/a"}"#, "");
        assert!(o.summary.is_none());
        assert_eq!(o.errors[0].message, "plain");
        assert!(!o.errors[0].vanished(), "no `during`: not recognised as vanished");
    }

    #[test]
    fn byte_units() {
        assert_eq!(bytes(10), "10 B");
        assert_eq!(bytes(2048), "2.0 KiB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
