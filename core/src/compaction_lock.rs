//! Daily compaction lock (ADR-025). Mirror of
//! `messaging/whatsapp/src/compaction_lock.ts`.
//!
//! `tools/restart-bots.sh` restarts the resident bots at 04:00, the same
//! minute the daily compaction starts, and a compaction takes minutes. While a
//! bot compacts it holds `memory/compaction/<agent>.lock` (its pid and start
//! time); the restart waits until no live lock is left. The lock is taken
//! [`COMPACTION_LOCK_LEAD`] before 04:00, so it exists when the restart job
//! starts at 04:00. A lock whose pid is dead or that is older than
//! [`COMPACTION_LOCK_MAX_AGE`] is stale and does not hold the restart.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Lock directory, relative to the workspace root.
pub const COMPACTION_LOCK_DIR: &str = "memory/compaction";

/// The lock is taken this long before the compaction starts.
pub const COMPACTION_LOCK_LEAD: Duration = Duration::from_secs(60);

/// A whole pass ends within this time: every step's timeout is cut to what
/// is left of it, and a chat is not started once it is used up.
pub const DAILY_COMPACTION_BUDGET: Duration = Duration::from_secs(25 * 60);

/// `restart-bots.sh` treats an older lock as stale (and waits at most this
/// long). The budget caps when a step starts and its timeout, but a step can
/// run past the budget: the summary turn is retried once after an
/// infrastructure error with the same timeout, and settling, typing and a
/// resume each take up to a minute. Lead 1 + budget 25 + one summary retry 5
/// + about 4 minutes of settling stays under 45.
pub const COMPACTION_LOCK_MAX_AGE: Duration = Duration::from_secs(45 * 60);

/// The text of a lock file: `pid=<pid>` and `started=<unix seconds>`.
pub fn lock_body(pid: u32, started_secs: u64) -> String {
    format!("pid={pid}\nstarted={started_secs}\n")
}

/// A held lock; dropping it removes the file when it is still this
/// process's.
#[derive(Debug)]
pub struct CompactionLock {
    path: PathBuf,
    body: String,
}

impl CompactionLock {
    /// Write `<root>/memory/compaction/<agent>.lock` for this process
    /// (replacing a stale one).
    pub fn take(workspace_root: &Path, agent: &str) -> std::io::Result<Self> {
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self::take_at(workspace_root, agent, started)
    }

    fn take_at(workspace_root: &Path, agent: &str, started_secs: u64) -> std::io::Result<Self> {
        let dir = workspace_root.join(COMPACTION_LOCK_DIR);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{agent}.lock"));
        let body = lock_body(std::process::id(), started_secs);
        let tmp = dir.join(format!("{agent}.lock.{}.tmp", std::process::id()));
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, &path)?;
        Ok(Self { path, body })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CompactionLock {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path).is_ok_and(|b| b == self.body) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Sleep until [`COMPACTION_LOCK_LEAD`] before the next daily compaction,
/// take the lock, sleep until the compaction time, and return the lock. The
/// caller compacts and then drops it. A lock that cannot be written is
/// logged and the compaction runs without it (`None`).
pub async fn wait_for_daily_compaction(workspace_root: &Path, agent: &str) -> Option<CompactionLock> {
    let wait = crate::claude_session::duration_until_next_compaction();
    let at = tokio::time::Instant::now() + wait;
    tokio::time::sleep_until(at - COMPACTION_LOCK_LEAD.min(wait)).await;
    let lock = match CompactionLock::take(workspace_root, agent) {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!(agent, "compaction lock not written: {e}");
            None
        }
    };
    tokio::time::sleep_until(at).await;
    lock
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_holds_pid_and_start_time_and_is_removed_only_by_its_owner() {
        let root = tempfile::tempdir().unwrap();
        let lock = CompactionLock::take_at(root.path(), "discord", 1_700_000_000).unwrap();
        assert_eq!(lock.path(), root.path().join("memory/compaction/discord.lock"));
        assert_eq!(
            std::fs::read_to_string(lock.path()).unwrap(),
            lock_body(std::process::id(), 1_700_000_000)
        );
        assert_eq!(lock_body(42, 7), "pid=42\nstarted=7\n");
        let path = lock.path().to_path_buf();
        drop(lock);
        assert!(!path.exists());
        // A lock another process took since is not removed.
        let mine = CompactionLock::take(root.path(), "discord").unwrap();
        std::fs::write(mine.path(), lock_body(1, 1)).unwrap();
        let path = mine.path().to_path_buf();
        drop(mine);
        assert_eq!(std::fs::read_to_string(path).unwrap(), lock_body(1, 1));
    }
}
