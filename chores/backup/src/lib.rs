//! `backup` — the daily encrypted backup of Nucleus's state to the homelab
//! (ADR-039).
//!
//! restic on this Mac writes to an `sftp:` repository on the homelab's HDD.
//! One run:
//!
//! 1. the HDD guard (`guard.rs`): over SSH, the repository path must be on
//!    the configured filesystem and not on the homelab's root filesystem;
//! 2. every SQLite database under the backed-up paths is copied with
//!    SQLite's online backup API into a job-owned staging directory
//!    (`sqlite.rs`), and the live files are excluded;
//! 3. `restic backup` of the configured paths plus the staging directory;
//! 4. the guard again, then `restic forget --prune` with the configured
//!    retention, only when nothing failed;
//! 5. on a scheduled run when `[backup] check_cron` is due and nothing
//!    failed, the guard again, then `restic check --read-data-subset`.
//!
//! Schedule: launchd wakes `nucleus backup run --scheduled` every 15
//! minutes; the run proceeds when `[backup] cron` (default 05:30 daily) has
//! had a match since the last successful scheduled run, so a Mac that was
//! asleep runs the backup at the next wake. A failed run is retried after
//! `retry_minutes`, and sends one WhatsApp alert per scheduled occurrence.
//! The first success after an alert sends one short note that the backup
//! recovered.
//!
//! Chore state (ADR-029 watermarks): `backup.last_success` (RFC 3339) is
//! what the heartbeat checks for a backup that stopped running;
//! `backup.last_run` holds the last run's result as JSON.

pub mod guard;
pub mod plan;
pub mod restic;
pub mod sqlite;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use clap::{Parser, Subcommand};
use nucleus_core::config::{BackupConfig, Settings};
use plan::{ExpandContext, Excludes};
use restic::Restic;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

const WM_SCHEDULED: &str = "backup.scheduled";
const WM_LAST_SUCCESS: &str = "backup.last_success";
const WM_LAST_RUN: &str = "backup.last_run";
const WM_LAST_FAILED_ATTEMPT: &str = "backup.last_failed_attempt";
const WM_ALERTED: &str = "backup.alerted";
const WM_CHECK: &str = "backup.check";
const LOCK_FILE: &str = "memory/backup.lock";
/// The job-owned directory inside `[backup] staging_dir`. It is created
/// with the marker file and removed only when the marker is in it, so a
/// misconfigured `staging_dir` can never make a run delete anything else.
const STAGING_WORK: &str = "work";
const STAGING_MARKER: &str = ".nucleus-backup-staging";
const STAGING_SQLITE: &str = "sqlite";
const EXCLUDE_FILE: &str = "excludes.txt";
const DIARY_AGENT: &str = "backup";

#[derive(Parser)]
#[command(name = "backup", about = "Encrypted daily backup of Nucleus's state to the homelab HDD (ADR-039)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run one backup now (guard, SQLite snapshots, restic backup, forget --prune).
    Run {
        /// What launchd runs: only when `[backup] cron` is due; alerts on failure.
        #[arg(long)]
        scheduled: bool,
        /// Also run `restic check` after the backup (manual runs).
        #[arg(long, conflicts_with = "scheduled")]
        check: bool,
    },
    /// Last success, last run, last check.
    Status,
    /// What a run would read: the roots, the missing entries and the SQLite databases.
    Plan,
    /// Run only the HDD guard and print its decision (exit 1 on refusal).
    Guard {
        /// Check this path on the homelab instead of the repository path.
        #[arg(long)]
        path: Option<String>,
    },
    /// List the snapshots.
    Snapshots,
    /// Run `restic check` now (after the guard).
    Check {
        /// Override `[backup] check_read_data_subset` (e.g. `100%`).
        #[arg(long)]
        read_data_subset: Option<String>,
    },
    /// Restore a snapshot into an empty directory. SQLite databases are moved
    /// from the staging copy back to their original place under that directory.
    Restore {
        /// Empty or new directory; files land under it at their absolute paths.
        #[arg(long)]
        to: PathBuf,
        /// Snapshot id, or `latest`.
        #[arg(long, default_value = "latest")]
        snapshot: String,
        /// Restore only this entry (same syntax as `[backup] paths`); repeatable.
        #[arg(long = "path")]
        paths: Vec<String>,
    },
}

pub async fn run(args: Vec<std::ffi::OsString>) -> Result<()> {
    let cli = Cli::parse_from(args);
    let settings = Settings::load().context("loading settings")?;
    let ws = settings.workspace_root()?;
    let cfg = settings.backup.clone();

    if let Cmd::Status = cli.cmd {
        return print_status(&ws).await;
    }
    let ctx = expand_context(&settings, &ws)?;
    if let Cmd::Plan = cli.cmd {
        return print_plan(&cfg, &ctx);
    }

    if let Cmd::Run { scheduled: true, .. } = cli.cmd {
        if cfg.repository.is_none() && cfg.hdd_uuid.is_none() {
            // A clone without a homelab: the job is installed with the
            // others and does nothing. One of the two set is a
            // configuration error (below), which fails and alerts.
            println!("backup: NUCLEUS_BACKUP_REPOSITORY and NUCLEUS_BACKUP_HDD_UUID not set; nothing to do");
            return Ok(());
        }
        let sched = Schedule {
            tz: reminders::store::nucleus_tz(),
            alerts: Alerts { enabled: cfg.notify, target: reminders::operator_whatsapp_dm() },
        };
        let job = Job::new(&cfg, ws.clone(), ctx);
        return run_scheduled(&ws, &cfg, job, &sched, Utc::now()).await;
    }

    let job = Job::new(&cfg, ws.clone(), ctx)?;
    match cli.cmd {
        Cmd::Run { check, .. } => {
            let _lock = job.lock()?.ok_or_else(|| anyhow!("another backup run holds {LOCK_FILE}"))?;
            let report = run_backup(&job, check).await;
            record_run(&job.ws, &report).await?;
            println!("{}", report.log_line());
            if report.ok() { Ok(()) } else { bail!("backup failed: {}", report.reason()) }
        }
        Cmd::Guard { path } => {
            let path = path.unwrap_or_else(|| job.repo.path.clone());
            match guard::check(&job.repo.host, &path, &job.hdd_uuid, job.cfg.ssh_connect_timeout_secs).await {
                Ok(v) => {
                    println!("{}: {} ({:?})", if v.passed() { "PASS" } else { "REFUSED" }, v.reason(), v);
                    if v.passed() { Ok(()) } else { std::process::exit(1) }
                }
                Err(e) => bail!("guard could not decide: {e:?}"),
            }
        }
        Cmd::Snapshots => {
            job.require_guard().await?;
            let args = vec!["snapshots".into(), "--host".into(), job.cfg.snapshot_host.clone(), "--tag".into(), restic::TAG.into()];
            exit_code(job.restic.interactive(&args).await?)
        }
        Cmd::Check { read_data_subset } => {
            job.require_guard().await?;
            let subset = read_data_subset.unwrap_or_else(|| job.cfg.check_read_data_subset.clone());
            let code = job.restic.interactive(&restic::check_args(&subset)).await?;
            if code == Some(0) {
                nucleus_core::chore_state::set_watermark(&job.ws, WM_CHECK, &Utc::now().to_rfc3339()).await?;
            }
            exit_code(code)
        }
        Cmd::Restore { to, snapshot, paths } => restore(&job, &to, &snapshot, &paths).await,
        Cmd::Status | Cmd::Plan => unreachable!(),
    }
}

fn exit_code(code: Option<i32>) -> Result<()> {
    match code {
        Some(0) => Ok(()),
        Some(c) => bail!("restic exited with {c}"),
        None => bail!("restic was stopped by a signal"),
    }
}

fn expand_context(settings: &Settings, ws: &Path) -> Result<ExpandContext> {
    let home = PathBuf::from(std::env::var("HOME").context("HOME is not set")?);
    Ok(ExpandContext {
        claude_project: home
            .join(".claude/projects")
            .join(nucleus_core::claude_session::project_dir_name(ws)),
        home,
        workspace: ws.to_path_buf(),
        vault: settings.obsidian.vault_dir(),
        launchd_prefix: std::env::var("NUCLEUS_LAUNCHD_PREFIX")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "dev.nucleus".into()),
    })
}

/// Everything one command needs.
struct Job {
    ws: PathBuf,
    cfg: BackupConfig,
    ctx: ExpandContext,
    repo: guard::SftpRepo,
    hdd_uuid: String,
    restic: Restic,
    excludes: Excludes,
    /// `[backup] staging_dir`; the run only writes inside `work()`.
    staging: PathBuf,
}

impl Job {
    fn new(cfg: &BackupConfig, ws: PathBuf, ctx: ExpandContext) -> Result<Self> {
        let repository = cfg.repository.clone().context("NUCLEUS_BACKUP_REPOSITORY is not set (.env)")?;
        let hdd_uuid = cfg.hdd_uuid.clone().context("NUCLEUS_BACKUP_HDD_UUID is not set (.env)")?;
        let repo = guard::parse_sftp_repo(&repository)?;
        let restic = Restic::new(
            &cfg.restic_bin,
            &repository,
            &cfg.keychain_service,
            &cfg.keychain_account,
            cfg.ssh_connect_timeout_secs,
        )?;
        let staging = nucleus_core::config::expand_home(&cfg.staging_dir);
        validate_staging(&staging, &cfg.paths, &ctx)?;
        Ok(Self { excludes: Excludes::new(&cfg.exclude)?, ws, cfg: cfg.clone(), ctx, repo, hdd_uuid, restic, staging })
    }

    fn work(&self) -> PathBuf {
        self.staging.join(STAGING_WORK)
    }

    /// The run lock: `None` when another run holds it.
    fn lock(&self) -> Result<Option<std::fs::File>> {
        let path = self.ws.join(LOCK_FILE);
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
        match f.try_lock() {
            Ok(()) => Ok(Some(f)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
        }
    }

    async fn require_guard(&self) -> Result<()> {
        match guard::check(&self.repo.host, &self.repo.path, &self.hdd_uuid, self.cfg.ssh_connect_timeout_secs).await {
            Ok(v) if v.passed() => Ok(()),
            Ok(v) => bail!("HDD guard refused: {} ({v:?})", v.reason()),
            Err(e) => bail!("HDD guard could not decide: {e:?}"),
        }
    }
}

/// Refuse a `staging_dir` that is `/`, `$HOME` or one of its ancestors, or
/// that contains or lies inside a backed-up path (configured, whether it
/// exists or not).
fn validate_staging(staging: &Path, entries: &[String], ctx: &ExpandContext) -> Result<()> {
    if !staging.is_absolute() {
        bail!("[backup] staging_dir must be absolute or start with ~/");
    }
    if staging.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("[backup] staging_dir must not contain `..`");
    }
    if staging == Path::new("/") || ctx.home.starts_with(staging) {
        bail!("[backup] staging_dir cannot be / or the home directory or one of its parents");
    }
    for entry in entries {
        let p = plan::expand_entry(entry, ctx)?;
        // A wildcard entry stands for entries of its parent directory.
        let has_wildcard = p.file_name().is_some_and(|n| n.to_string_lossy().contains(['*', '?']));
        let p = if has_wildcard { p.parent().unwrap_or(&p).to_path_buf() } else { p };
        if staging.starts_with(&p) || p.starts_with(staging) {
            bail!("[backup] staging_dir overlaps the backed-up path {entry:?}");
        }
    }
    Ok(())
}

/// Create the job-owned directory (with its marker) inside `staging`. A
/// leftover from an earlier run is removed only when it has the marker.
fn prepare_work(staging: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let work = staging.join(STAGING_WORK);
    remove_work(&work)?;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(staging)?;
    // Not recursive: fails if `work` appeared in between.
    std::fs::DirBuilder::new().mode(0o700).create(&work).with_context(|| format!("creating {}", work.display()))?;
    std::fs::write(work.join(STAGING_MARKER), b"Created by `nucleus backup`; removed after each run.\n")?;
    std::fs::DirBuilder::new().mode(0o700).create(work.join(STAGING_SQLITE))?;
    Ok(work)
}

/// Remove the job-owned directory. Absent: nothing to do. Present without
/// the marker: refuse, it is not ours.
fn remove_work(work: &Path) -> Result<()> {
    match work.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("reading {}", work.display())),
        Ok(m) if m.is_dir() && work.join(STAGING_MARKER).is_file() => {
            std::fs::remove_dir_all(work).with_context(|| format!("removing {}", work.display()))
        }
        Ok(_) => bail!("{} exists and was not created by the backup job; not touching it", work.display()),
    }
}

/// Why a run failed. The alert texts are code-owned and carry no paths,
/// host names or command output.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Failure {
    Config,
    Ssh,
    Guard { reason: String },
    Internal,
    DatabaseCopy { count: usize },
    Backup { code: Option<i32> },
    Unreadable { count: usize },
    Forget { code: Option<i32> },
    Check { code: Option<i32> },
}

impl Failure {
    fn text(&self) -> String {
        match self {
            Failure::Config => "the backup configuration is invalid".into(),
            Failure::Ssh => "the homelab could not be reached over SSH".into(),
            Failure::Guard { reason } => format!("HDD guard refused: {reason}"),
            Failure::Internal => "an internal error stopped the run".into(),
            Failure::DatabaseCopy { count } => format!("{count} SQLite database(s) could not be copied"),
            Failure::Backup { code } => format!("restic backup failed ({})", code_text(*code)),
            Failure::Unreadable { count } => format!("{count} file(s) could not be read"),
            Failure::Forget { code } => format!("restic forget --prune failed ({})", code_text(*code)),
            Failure::Check { code } => format!("the weekly repository check failed ({})", code_text(*code)),
        }
    }
}

fn code_text(code: Option<i32>) -> String {
    code.map(|c| format!("exit {c}")).unwrap_or_else(|| "stopped by a signal".into())
}

/// The result of one run.
#[derive(Debug, Default, Serialize)]
struct RunReport {
    at: String,
    failures: Vec<Failure>,
    /// A snapshot was written (it can be incomplete: see the failures).
    snapshot_saved: bool,
    snapshot_id: Option<String>,
    files: u64,
    bytes_processed: u64,
    bytes_added: u64,
    databases: usize,
    duration_secs: u64,
    check_ran: bool,
    /// Log-only details (paths, restic stderr); never sent anywhere.
    #[serde(skip)]
    details: Vec<String>,
}

impl RunReport {
    fn ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// True when the backup itself succeeded (only the check failed).
    fn backup_ok(&self) -> bool {
        self.failures.iter().all(|f| matches!(f, Failure::Check { .. }))
    }

    fn reason(&self) -> String {
        self.failures.iter().map(Failure::text).collect::<Vec<_>>().join("; ")
    }

    fn log_line(&self) -> String {
        let mut s = format!(
            "backup {}: {} files, {} read, {} added, {} SQLite databases, {} s",
            if self.ok() { "ok" } else { "FAILED" },
            self.files,
            restic::bytes(self.bytes_processed),
            restic::bytes(self.bytes_added),
            self.databases,
            self.duration_secs,
        );
        if let Some(id) = &self.snapshot_id {
            s.push_str(&format!(", snapshot {id}"));
        }
        if self.check_ran {
            s.push_str(", check ran");
        }
        if !self.ok() {
            s.push_str(&format!(" — {}", self.reason()));
        }
        for d in &self.details {
            s.push_str(&format!("\n  {d}"));
        }
        s
    }

    fn alert_text(&self) -> String {
        let saved = if self.snapshot_saved && !self.backup_ok() {
            " A snapshot was saved, but it is incomplete."
        } else if self.backup_ok() {
            " Today's snapshot was saved."
        } else {
            " No snapshot was saved."
        };
        format!("⚠️ Nucleus backup failed: {}.{saved} Details: memory/backup.log", self.reason())
    }
}

/// One backup, start to end. Never returns early without a report: every
/// failure is recorded in it.
async fn run_backup(job: &Job, run_check: bool) -> RunReport {
    let started = Instant::now();
    let mut r = RunReport { at: Utc::now().to_rfc3339(), ..Default::default() };
    if let Err(e) = backup_steps(job, run_check, &mut r).await {
        r.failures.push(Failure::Internal);
        r.details.push(format!("internal error: {e:#}"));
    }
    if let Err(e) = remove_work(&job.work()) {
        r.details.push(format!("staging not removed: {e:#}"));
    }
    r.duration_secs = started.elapsed().as_secs();
    r
}

/// Run the HDD guard; record a refusal or an SSH failure in the report.
async fn guard_step(job: &Job, r: &mut RunReport, stage: &str) -> bool {
    match guard::check(&job.repo.host, &job.repo.path, &job.hdd_uuid, job.cfg.ssh_connect_timeout_secs).await {
        Ok(v) if v.passed() => true,
        Ok(v) => {
            r.details.push(format!("guard ({stage}): {v:?}"));
            r.failures.push(Failure::Guard { reason: v.reason().to_string() });
            false
        }
        Err(e) => {
            r.details.push(format!("guard ({stage}): {e:?}"));
            r.failures.push(match e {
                guard::ProbeError::Ssh(_) => Failure::Ssh,
                guard::ProbeError::Output(_) => Failure::Internal,
            });
            false
        }
    }
}

/// The outcome of `restic backup`, from its exit code and parsed output.
fn classify_backup(code: Option<i32>, out: &restic::BackupOutput) -> Option<Failure> {
    let snapshot = out.summary.as_ref().is_some_and(|s| s.snapshot_id.is_some());
    match code {
        Some(0) => None,
        // 3: a snapshot was written but some source data could not be read.
        // Acceptable only when every error is a file deleted during the run.
        Some(3) if snapshot => {
            if !out.errors.is_empty() && out.errors.iter().all(restic::ItemError::vanished) {
                None
            } else {
                Some(Failure::Unreadable { count: out.errors.iter().filter(|e| !e.vanished()).count() })
            }
        }
        code => Some(Failure::Backup { code }),
    }
}

async fn backup_steps(job: &Job, run_check: bool, r: &mut RunReport) -> Result<()> {
    // 1. HDD guard.
    if !guard_step(job, r, "before backup").await {
        return Ok(());
    }

    // 2. Roots, databases, SQLite snapshots.
    let roots = plan::expand_paths(&job.cfg.paths, &job.ctx)?;
    if !roots.missing.is_empty() {
        r.details.push(format!("not present, skipped: {}", roots.missing.join(", ")));
    }
    if roots.roots.is_empty() {
        bail!("none of the [backup] paths exists");
    }
    let work = prepare_work(&job.staging)?;
    let staging_sqlite = work.join(STAGING_SQLITE);

    let (roots_v, excludes, staging_c) = (roots.roots.clone(), job.excludes.clone(), staging_sqlite.clone());
    let (copied, failed) = tokio::task::spawn_blocking(move || {
        let dbs = plan::find_databases(&roots_v, &excludes);
        let mut copied = Vec::new();
        let mut failed = Vec::new();
        for db in dbs {
            match sqlite::snapshot(&db, &plan::staged_path(&staging_c, &db)) {
                Ok(_) => copied.push(db),
                Err(e) => failed.push((db, format!("{e:#}"))),
            }
        }
        (copied, failed)
    })
    .await
    .context("SQLite snapshot task")?;
    r.databases = copied.len();
    if !failed.is_empty() {
        for (db, e) in &failed {
            r.details.push(format!("SQLite copy failed: {}: {e}", db.display()));
        }
        r.failures.push(Failure::DatabaseCopy { count: failed.len() });
    }
    // A database whose copy failed is excluded too: a raw copy of a live
    // database is what this step exists to avoid.
    let mut all_dbs = copied;
    all_dbs.extend(failed.into_iter().map(|(db, _)| db));
    let exclude_path = work.join(EXCLUDE_FILE);
    std::fs::write(&exclude_path, plan::exclude_file(&job.excludes, &all_dbs)?)?;

    // 3. restic backup.
    let mut sources = roots.roots.clone();
    sources.push(staging_sqlite);
    let out = job
        .restic
        .output(&restic::backup_args(&job.cfg.snapshot_host, &exclude_path, &sources))
        .await?;
    let parsed = restic::parse_backup_output(&out.stdout, &out.stderr);
    if let Some(s) = &parsed.summary {
        r.snapshot_id = s.snapshot_id.clone();
        r.files = s.total_files_processed;
        r.bytes_processed = s.total_bytes_processed;
        r.bytes_added = s.data_added;
    }
    r.snapshot_saved = r.snapshot_id.is_some();
    for e in &parsed.errors {
        let kind = if e.vanished() { "vanished during the run" } else { "not read" };
        r.details.push(format!("restic ({kind}): {}: {}", e.item, e.message));
    }
    for line in parsed.other.iter().take(20) {
        r.details.push(format!("restic: {line}"));
    }
    if let Some(f) = classify_backup(out.code, &parsed) {
        let stop = matches!(f, Failure::Backup { .. });
        r.failures.push(f);
        if stop {
            return Ok(());
        }
    }

    // 4. The repository must still be on the HDD (restic's SFTP backend
    // creates missing directories), then retention, only after a clean run:
    // a partial snapshot must not push a complete one out.
    if !guard_step(job, r, "after backup").await {
        return Ok(());
    }
    if !r.failures.is_empty() {
        r.details.push("retention and check skipped: the run had failures".into());
        return Ok(());
    }
    let c = &job.cfg;
    let out = job
        .restic
        .output(&restic::forget_args(&c.snapshot_host, c.keep_daily, c.keep_weekly, c.keep_monthly))
        .await?;
    if out.code != Some(0) {
        push_stderr(r, &out.stderr);
        r.failures.push(Failure::Forget { code: out.code });
        return Ok(());
    }

    // 5. Weekly check.
    if run_check {
        if !guard_step(job, r, "before check").await {
            return Ok(());
        }
        r.check_ran = true;
        let out = job.restic.output(&restic::check_args(&c.check_read_data_subset)).await?;
        if out.code == Some(0) {
            nucleus_core::chore_state::set_watermark(&job.ws, WM_CHECK, &Utc::now().to_rfc3339()).await?;
        } else {
            push_stderr(r, &out.stderr);
            r.failures.push(Failure::Check { code: out.code });
        }
    }
    Ok(())
}

fn push_stderr(r: &mut RunReport, stderr: &str) {
    for line in stderr.lines().filter(|l| !l.trim().is_empty()).take(20) {
        r.details.push(format!("restic: {line}"));
    }
}

async fn record_run(ws: &Path, r: &RunReport) -> Result<()> {
    use nucleus_core::chore_state::set_watermark;
    set_watermark(ws, WM_LAST_RUN, &serde_json::to_string(r)?).await?;
    if r.backup_ok() {
        set_watermark(ws, WM_LAST_SUCCESS, &r.at).await?;
    }
    let tag = if r.ok() { nucleus_core::diary::Tag::Routine } else { nucleus_core::diary::Tag::Observation };
    let line = if r.ok() {
        format!(
            "backup ok: {} files, {} added, {} s",
            r.files,
            restic::bytes(r.bytes_added),
            r.duration_secs
        )
    } else {
        format!("backup failed: {}", r.reason())
    };
    let _ = nucleus_core::diary::record_observation(ws, DIARY_AGENT, "daily", &line, tag);
    Ok(())
}

async fn watermark_time(ws: &Path, key: &str) -> Result<Option<DateTime<Utc>>> {
    Ok(nucleus_core::chore_state::watermark(ws, key)
        .await?
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|t| t.with_timezone(&Utc)))
}

/// The latest cron match after `last` and at or before `now`, if any.
fn latest_match(cron: &str, last: DateTime<Utc>, now: DateTime<Utc>, tz: Tz) -> Result<Option<DateTime<Utc>>> {
    let next = |t| reminders::store::next_match_utc(cron, t, tz).with_context(|| format!("[backup] cron {cron:?}"));
    let mut m = next(last)?;
    if m > now {
        return Ok(None);
    }
    for _ in 0..100_000 {
        let n = next(m)?;
        if n > now {
            return Ok(Some(m));
        }
        m = n;
    }
    Ok(Some(m))
}

/// Without a watermark (a fresh install), look back this far: the first
/// wake runs the backup.
const FIRST_RUN_LOOKBACK_DAYS: i64 = 8;

/// What a scheduled run takes from the process environment, resolved once
/// by the caller (tests pass their own).
struct Schedule {
    /// The zone the crons are evaluated in (`NUCLEUS_TZ`).
    tz: Tz,
    alerts: Alerts,
}

struct Alerts {
    /// `[backup] notify`.
    enabled: bool,
    /// The operator's WhatsApp DM (first `WHATSAPP_ALLOWED_DM_JIDS` entry).
    target: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Gate {
    NotDue,
    /// The last attempt for this occurrence failed less than
    /// `retry_minutes` ago.
    Waiting,
    Due { occurrence: DateTime<Utc>, check: bool },
}

async fn gate(ws: &Path, cfg: &BackupConfig, tz: Tz, now: DateTime<Utc>) -> Result<Gate> {
    let lookback = now - Duration::days(FIRST_RUN_LOOKBACK_DAYS);
    let last = watermark_time(ws, WM_SCHEDULED).await?.unwrap_or(lookback);
    let Some(occurrence) = latest_match(&cfg.cron, last, now, tz)? else {
        return Ok(Gate::NotDue);
    };
    let failed = watermark_time(ws, WM_LAST_FAILED_ATTEMPT).await?;
    if failed.is_some_and(|f| f >= occurrence && now - f < Duration::minutes(cfg.retry_minutes)) {
        return Ok(Gate::Waiting);
    }
    let last_check = watermark_time(ws, WM_CHECK).await?.unwrap_or(lookback);
    let check = latest_match(&cfg.check_cron, last_check, now, tz)?.is_some();
    Ok(Gate::Due { occurrence, check })
}

/// A scheduled wake. `job` is the result of reading the configuration: an
/// invalid one (for example only one of the two `.env` values set) is a
/// failed run, with the same alert and retry as any other failure.
async fn run_scheduled(
    ws: &Path,
    cfg: &BackupConfig,
    job: Result<Job>,
    sched: &Schedule,
    now: DateTime<Utc>,
) -> Result<()> {
    let (occurrence, check) = match gate(ws, cfg, sched.tz, now).await? {
        Gate::Due { occurrence, check } => (occurrence, check),
        Gate::NotDue | Gate::Waiting => return Ok(()),
    };
    let report = match job {
        Err(e) => RunReport {
            at: now.to_rfc3339(),
            failures: vec![Failure::Config],
            details: vec![format!("configuration: {e:#}")],
            ..Default::default()
        },
        Ok(job) => {
            let Some(_lock) = job.lock()? else {
                println!("backup: another run holds {LOCK_FILE}; skipping this wake");
                return Ok(());
            };
            run_backup(&job, check).await
        }
    };
    record_run(ws, &report).await?;
    println!("{} {}", Utc::now().to_rfc3339(), report.log_line());
    finish_scheduled(ws, &sched.alerts, occurrence, &report, now).await
}

/// Watermarks and alerts after a scheduled run.
async fn finish_scheduled(
    ws: &Path,
    alerts: &Alerts,
    occurrence: DateTime<Utc>,
    report: &RunReport,
    now: DateTime<Utc>,
) -> Result<()> {
    use nucleus_core::chore_state::{set_watermark, watermark};
    let key = occurrence.to_rfc3339();
    if report.backup_ok() {
        set_watermark(ws, WM_SCHEDULED, &now.to_rfc3339()).await?;
        set_watermark(ws, WM_LAST_FAILED_ATTEMPT, "").await?;
    } else {
        set_watermark(ws, WM_LAST_FAILED_ATTEMPT, &now.to_rfc3339()).await?;
    }
    if report.ok() {
        // An alert is outstanding (from this or an earlier occurrence):
        // tell the operator once that the backup works again.
        if let Some(alerted) = watermark(ws, WM_ALERTED).await?.filter(|v| !v.is_empty()) {
            let text = "✅ Nucleus backup recovered: the latest run succeeded.";
            notify(ws, alerts, &format!("backup:{alerted}:recovered"), text).await?;
            set_watermark(ws, WM_ALERTED, "").await?;
        }
        return Ok(());
    }
    // One alert per occurrence for the backup, one for the check.
    let source = if report.backup_ok() { format!("backup-check:{key}") } else { format!("backup:{key}") };
    notify(ws, alerts, &source, &report.alert_text()).await?;
    if !report.backup_ok() {
        set_watermark(ws, WM_ALERTED, &key).await?;
    }
    bail!("backup failed: {}", report.reason())
}

async fn notify(ws: &Path, alerts: &Alerts, source: &str, text: &str) -> Result<()> {
    if !alerts.enabled {
        return Ok(());
    }
    let Some(target) = &alerts.target else {
        tracing::warn!("backup: WHATSAPP_ALLOWED_DM_JIDS is empty; alert not sent");
        return Ok(());
    };
    let pool = nucleus_core::whatsapp_queue::open(ws).await?;
    let queued = reminders::store::enqueue_whatsapp_once(&pool, target, text, source).await?;
    tracing::info!(source, queued = queued.is_some(), "backup: alert");
    Ok(())
}

async fn print_status(ws: &Path) -> Result<()> {
    use nucleus_core::chore_state::watermark;
    for key in [WM_LAST_SUCCESS, WM_SCHEDULED, WM_CHECK, WM_LAST_FAILED_ATTEMPT, WM_ALERTED] {
        let v = watermark(ws, key).await?.filter(|v| !v.is_empty());
        println!("{key:<28} {}", v.as_deref().unwrap_or("-"));
    }
    match watermark(ws, WM_LAST_RUN).await? {
        Some(v) => println!("{WM_LAST_RUN:<28} {v}"),
        None => println!("{WM_LAST_RUN:<28} -"),
    }
    Ok(())
}

fn print_plan(cfg: &BackupConfig, ctx: &ExpandContext) -> Result<()> {
    let roots = plan::expand_paths(&cfg.paths, ctx)?;
    let excludes = Excludes::new(&cfg.exclude)?;
    println!("roots:");
    for r in &roots.roots {
        println!("  {}", r.display());
    }
    if !roots.missing.is_empty() {
        println!("not present: {}", roots.missing.join(", "));
    }
    println!("excluded names: {}", excludes.patterns().join(", "));
    println!("SQLite databases (copied with the backup API):");
    for db in plan::find_databases(&roots.roots, &excludes) {
        let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
        println!("  {} ({})", db.display(), restic::bytes(size));
    }
    Ok(())
}

async fn restore(job: &Job, to: &Path, snapshot: &str, entries: &[String]) -> Result<()> {
    if !to.is_absolute() || to == Path::new("/") {
        bail!("--to must be an absolute directory other than /");
    }
    if to.exists() && std::fs::read_dir(to)?.next().is_some() {
        bail!("{} is not empty; restore into an empty or new directory", to.display());
    }
    let layouts = staged_layouts(&job.staging);
    let mut args: Vec<String> = vec![
        "restore".into(),
        snapshot.into(),
        "--target".into(),
        to.to_string_lossy().into_owned(),
        "--host".into(),
        job.cfg.snapshot_host.clone(),
        "--tag".into(),
        restic::TAG.into(),
    ];
    for entry in entries {
        let abs = plan::expand_entry(entry, &job.ctx)?;
        let mut patterns = vec![abs.clone()];
        patterns.extend(layouts.iter().map(|l| plan::staged_path(l, &abs)));
        for p in patterns {
            args.push("--include".into());
            args.push(plan::escape_pattern(&p));
        }
    }
    exit_code(job.restic.interactive(&args).await?)?;
    let mut moved = 0;
    for layout in &layouts {
        moved += relocate_staged(to, layout)?;
    }
    println!("restored into {}; {moved} SQLite database(s) moved back to their original paths", to.display());
    Ok(())
}

/// Where snapshots hold the SQLite copies: `<staging>/work/sqlite`, and
/// `<staging>/sqlite` in snapshots made before the job-owned `work`
/// directory existed (2026-11-03).
fn staged_layouts(staging: &Path) -> Vec<PathBuf> {
    vec![staging.join(STAGING_WORK).join(STAGING_SQLITE), staging.join(STAGING_SQLITE)]
}

/// Move restored SQLite copies from `<to>/<staged root>/<abs path>` to
/// `<to>/<abs path>` and remove the restored staged root.
fn relocate_staged(to: &Path, staged_root_abs: &Path) -> Result<usize> {
    let staged_root = plan::staged_path(to, staged_root_abs);
    if !staged_root.exists() {
        return Ok(0);
    }
    let mut files = Vec::new();
    collect_files(&staged_root, &mut files)?;
    for f in &files {
        let rel = f.strip_prefix(&staged_root)?;
        let dest = to.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dest.exists() {
            bail!("{} exists in the snapshot twice (as a file and as a SQLite copy)", dest.display());
        }
        std::fs::rename(f, &dest)?;
    }
    std::fs::remove_dir_all(&staged_root)?;
    Ok(files.len())
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let t = e.file_type()?;
        if t.is_dir() {
            collect_files(&e.path(), out)?;
        } else {
            out.push(e.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const UTC: Tz = chrono_tz::UTC;

    fn cfg() -> BackupConfig {
        BackupConfig::default()
    }

    fn alerts() -> Alerts {
        Alerts { enabled: true, target: Some("5511999999999@s.whatsapp.net".into()) }
    }

    fn ctx(tmp: &Path) -> ExpandContext {
        ExpandContext {
            home: tmp.join("home"),
            workspace: tmp.join("home/ws"),
            vault: tmp.join("home/Documents/Vault"),
            claude_project: tmp.join("home/.claude/projects/-ws"),
            launchd_prefix: "dev.nucleus".into(),
        }
    }

    async fn queued(ws: &Path) -> Vec<(String, String)> {
        let pool = nucleus_core::whatsapp_queue::open(ws).await.unwrap();
        sqlx::query_as("SELECT source, body FROM outbound_queue ORDER BY id").fetch_all(&pool).await.unwrap()
    }

    fn workspace() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("memory")).unwrap();
        t
    }

    #[test]
    fn latest_match_picks_the_most_recent_occurrence() {
        let last = Utc.with_ymd_and_hms(2026, 11, 1, 5, 40, 0).unwrap();
        let before = Utc.with_ymd_and_hms(2026, 11, 2, 5, 29, 0).unwrap();
        assert_eq!(latest_match("30 5 * * *", last, before, UTC).unwrap(), None);
        let at = Utc.with_ymd_and_hms(2026, 11, 2, 5, 30, 0).unwrap();
        assert_eq!(latest_match("30 5 * * *", last, at, UTC).unwrap(), Some(at));
        // Three days asleep: one run, keyed by the latest match.
        let late = Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap();
        assert_eq!(
            latest_match("30 5 * * *", last, late, UTC).unwrap(),
            Some(Utc.with_ymd_and_hms(2026, 11, 4, 5, 30, 0).unwrap())
        );
        // The zone is a parameter: 05:30 in UTC+9 is 20:30 UTC the day before.
        let tokyo: Tz = "Asia/Tokyo".parse().unwrap();
        assert_eq!(
            latest_match("30 5 * * *", last, late, tokyo).unwrap(),
            Some(Utc.with_ymd_and_hms(2026, 11, 3, 20, 30, 0).unwrap())
        );
        assert!(latest_match("bad", last, late, UTC).is_err());
    }

    #[tokio::test]
    async fn gate_runs_once_per_day_and_backs_off_after_a_failure() {
        let t = workspace();
        let ws = t.path();
        let c = cfg();
        let now = Utc.with_ymd_and_hms(2026, 11, 3, 5, 31, 0).unwrap(); // Tuesday
        let occ = Utc.with_ymd_and_hms(2026, 11, 3, 5, 30, 0).unwrap();
        // Fresh install: due at the first wake, with a check (never ran).
        assert_eq!(gate(ws, &c, UTC, now).await.unwrap(), Gate::Due { occurrence: occ, check: true });
        // A failed run: the next wake waits, a wake after retry_minutes runs.
        let failed = RunReport { failures: vec![Failure::Ssh], ..Default::default() };
        let off = Alerts { enabled: false, target: None };
        assert!(finish_scheduled(ws, &off, occ, &failed, now).await.is_err());
        assert_eq!(gate(ws, &c, UTC, now + Duration::minutes(15)).await.unwrap(), Gate::Waiting);
        assert!(matches!(gate(ws, &c, UTC, now + Duration::minutes(61)).await.unwrap(), Gate::Due { .. }));
        // Success: not due again until tomorrow's 05:30.
        finish_scheduled(ws, &off, occ, &RunReport::default(), now + Duration::minutes(61)).await.unwrap();
        assert_eq!(gate(ws, &c, UTC, now + Duration::hours(5)).await.unwrap(), Gate::NotDue);
        nucleus_core::chore_state::set_watermark(ws, WM_CHECK, &now.to_rfc3339()).await.unwrap();
        assert_eq!(
            gate(ws, &c, UTC, now + Duration::days(1)).await.unwrap(),
            Gate::Due { occurrence: occ + Duration::days(1), check: false }
        );
        // Sunday 2026-11-08 05:30: the weekly check is due.
        let sunday = Utc.with_ymd_and_hms(2026, 11, 8, 5, 45, 0).unwrap();
        assert!(matches!(gate(ws, &c, UTC, sunday).await.unwrap(), Gate::Due { check: true, .. }));
    }

    #[tokio::test]
    async fn one_alert_per_occurrence_and_a_recovery_note() {
        let t = workspace();
        let ws = t.path();
        let occ = Utc.with_ymd_and_hms(2026, 11, 3, 5, 30, 0).unwrap();
        let now = occ + Duration::minutes(1);
        let failed = RunReport { failures: vec![Failure::Guard { reason: "x".into() }], ..Default::default() };
        assert!(finish_scheduled(ws, &alerts(), occ, &failed, now).await.is_err());
        assert!(finish_scheduled(ws, &alerts(), occ, &failed, now + Duration::hours(1)).await.is_err());
        finish_scheduled(ws, &alerts(), occ, &RunReport::default(), now + Duration::hours(2)).await.unwrap();
        // Another success sends nothing more.
        finish_scheduled(ws, &alerts(), occ, &RunReport::default(), now + Duration::hours(3)).await.unwrap();
        let rows = queued(ws).await;
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].0, format!("backup:{}", occ.to_rfc3339()));
        assert!(rows[0].1.starts_with("⚠️ Nucleus backup failed: HDD guard refused: x."));
        assert_eq!(rows[1].0, format!("backup:{}:recovered", occ.to_rfc3339()));
    }

    /// An alert on one day and the next success on a later occurrence: the
    /// recovery note is still sent and the outstanding alert cleared.
    #[tokio::test]
    async fn recovery_note_after_an_alert_from_an_earlier_day() {
        let t = workspace();
        let ws = t.path();
        let day1 = Utc.with_ymd_and_hms(2026, 11, 3, 5, 30, 0).unwrap();
        let day2 = day1 + Duration::days(1);
        let failed = RunReport { failures: vec![Failure::Ssh], ..Default::default() };
        assert!(finish_scheduled(ws, &alerts(), day1, &failed, day1).await.is_err());
        finish_scheduled(ws, &alerts(), day2, &RunReport::default(), day2).await.unwrap();
        let rows = queued(ws).await;
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[1].0, format!("backup:{}:recovered", day1.to_rfc3339()));
        let alerted = nucleus_core::chore_state::watermark(ws, WM_ALERTED).await.unwrap();
        assert_eq!(alerted.as_deref(), Some(""));
    }

    /// Only one of the two `.env` values set: a failed run with an alert,
    /// not a silent exit.
    #[tokio::test]
    async fn a_partial_configuration_fails_and_alerts() {
        let t = workspace();
        let ws = t.path();
        let mut c = cfg();
        c.repository = Some("sftp:backup-host:/srv/backups/r".into());
        let job = Job::new(&c, ws.to_path_buf(), ctx(ws));
        assert!(job.is_err());
        let sched = Schedule { tz: UTC, alerts: alerts() };
        let now = Utc.with_ymd_and_hms(2026, 11, 3, 5, 31, 0).unwrap();
        let res = run_scheduled(ws, &c, job, &sched, now).await;
        assert!(res.is_err());
        let rows = queued(ws).await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.starts_with("⚠️ Nucleus backup failed: the backup configuration is invalid."), "{}", rows[0].1);
        let last = nucleus_core::chore_state::watermark(ws, WM_LAST_RUN).await.unwrap().unwrap();
        assert!(last.contains("\"kind\":\"config\""), "{last}");
        assert_eq!(nucleus_core::chore_state::watermark(ws, WM_LAST_SUCCESS).await.unwrap(), None);
    }

    #[test]
    fn staging_dir_validation() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(t.path());
        let home = &c.home;
        let paths: Vec<String> = ["memory", "{vault}", "~/.claude/skills", "~/Library/LaunchAgents/{launchd_prefix}.*.plist"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(validate_staging(&home.join("Library/Caches/nucleus-backup"), &paths, &c).is_ok());
        for bad in [
            PathBuf::from("/"),
            home.clone(),
            t.path().to_path_buf(),                 // a parent of $HOME
            home.join("Documents"),                 // contains the vault
            home.join("Documents/Vault/x"),         // inside the vault
            home.join("ws/memory/staging"),         // inside memory
            home.join("Library/LaunchAgents"),      // a wildcard entry's directory
            PathBuf::from("relative/dir"),
            home.join("a/../b"),
        ] {
            assert!(validate_staging(&bad, &paths, &c).is_err(), "{} accepted", bad.display());
        }
    }

    #[test]
    fn work_dir_is_created_and_removed_only_with_its_marker() {
        let t = tempfile::tempdir().unwrap();
        let staging = t.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("unrelated"), b"keep").unwrap();
        let work = prepare_work(&staging).unwrap();
        assert!(work.join(STAGING_MARKER).is_file() && work.join(STAGING_SQLITE).is_dir());
        std::fs::write(work.join(STAGING_SQLITE).join("a.db"), b"x").unwrap();
        // A second prepare replaces the earlier (marked) directory.
        let work = prepare_work(&staging).unwrap();
        assert!(!work.join(STAGING_SQLITE).join("a.db").exists());
        remove_work(&work).unwrap();
        assert!(!work.exists());
        assert_eq!(std::fs::read(staging.join("unrelated")).unwrap(), b"keep", "only `work` is removed");
        // A `work` directory without the marker is never removed.
        std::fs::create_dir_all(work.join("data")).unwrap();
        assert!(remove_work(&work).is_err());
        assert!(prepare_work(&staging).is_err());
        assert!(work.join("data").exists());
        // Nothing there: nothing to do.
        assert!(remove_work(&t.path().join("absent/work")).is_ok());
    }

    #[test]
    fn exit_three_is_a_failure_unless_every_error_is_a_vanished_file() {
        let summary = Some(restic::Summary { snapshot_id: Some("abc".into()), ..Default::default() });
        let err = |during: &str, message: &str| restic::ItemError {
            item: "/x".into(),
            during: during.into(),
            message: message.into(),
        };
        let out = |errors| restic::BackupOutput { summary: summary.clone(), errors, other: vec![] };
        assert_eq!(classify_backup(Some(0), &out(vec![])), None);
        let vanished = err("archival", "lstat /x: no such file or directory");
        assert_eq!(classify_backup(Some(3), &out(vec![vanished.clone()])), None);
        let denied = err("archival", "open /x: permission denied");
        assert_eq!(classify_backup(Some(3), &out(vec![vanished, denied])), Some(Failure::Unreadable { count: 1 }));
        // Exit 3 with no recognised error: not accepted.
        assert_eq!(classify_backup(Some(3), &out(vec![])), Some(Failure::Unreadable { count: 0 }));
        // Exit 3 without a snapshot, or any other code: the backup failed.
        let none = restic::BackupOutput::default();
        assert_eq!(classify_backup(Some(3), &none), Some(Failure::Backup { code: Some(3) }));
        assert_eq!(classify_backup(Some(1), &out(vec![])), Some(Failure::Backup { code: Some(1) }));
        assert_eq!(classify_backup(None, &out(vec![])), Some(Failure::Backup { code: None }));
    }

    #[test]
    fn alert_texts_are_code_owned() {
        let r = RunReport {
            failures: vec![Failure::DatabaseCopy { count: 2 }],
            snapshot_saved: true,
            details: vec!["/secret/path".into()],
            ..Default::default()
        };
        assert_eq!(
            r.alert_text(),
            "⚠️ Nucleus backup failed: 2 SQLite database(s) could not be copied. A snapshot was saved, but it is incomplete. Details: memory/backup.log"
        );
        let r = RunReport { failures: vec![Failure::Check { code: Some(1) }], ..Default::default() };
        assert!(r.backup_ok());
        assert!(r.alert_text().contains("Today's snapshot was saved."));
        let r = RunReport { failures: vec![Failure::Backup { code: Some(1) }], ..Default::default() };
        assert!(r.alert_text().contains("No snapshot was saved."));
    }

    #[test]
    fn relocates_staged_databases_in_both_layouts() {
        let t = tempfile::tempdir().unwrap();
        let to = t.path().join("restore");
        let staging = PathBuf::from("/home/u/Library/Caches/nucleus-backup");
        let [current, legacy] = <[PathBuf; 2]>::try_from(staged_layouts(&staging)).unwrap();
        for (layout, name) in [(&current, "a.db"), (&legacy, "b.db")] {
            let staged = plan::staged_path(&to, layout).join("home/u/ws/memory");
            std::fs::create_dir_all(&staged).unwrap();
            std::fs::write(staged.join(name), name).unwrap();
        }
        std::fs::create_dir_all(to.join("home/u/ws/memory")).unwrap();
        std::fs::write(to.join("home/u/ws/memory/x.log"), b"log").unwrap();
        assert_eq!(relocate_staged(&to, &current).unwrap(), 1);
        assert_eq!(relocate_staged(&to, &legacy).unwrap(), 1);
        assert_eq!(std::fs::read(to.join("home/u/ws/memory/a.db")).unwrap(), b"a.db");
        assert_eq!(std::fs::read(to.join("home/u/ws/memory/b.db")).unwrap(), b"b.db");
        assert!(!plan::staged_path(&to, &current).exists());
        assert!(!plan::staged_path(&to, &legacy).exists());
        assert!(to.join("home/u/ws/memory/x.log").exists());
        assert_eq!(relocate_staged(&to, &current).unwrap(), 0);
    }
}
