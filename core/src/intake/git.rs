//! Git operations the pipeline's own code performs (ADR-036): one base
//! clone per repo and one worktree per item, under the configured work
//! directory, outside the Nucleus checkout. Network steps (clone, fetch,
//! push) happen here, never in an agent session.
//!
//! Layout: `<work_dir>/<owner>__<name>/base` (the clone) and
//! `<work_dir>/<owner>__<name>/item-<n>` (worktrees).

use super::github::{self, GhRunner};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Output of one git command.
pub struct GitOut {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

pub async fn git(dir: &Path, args: &[&str]) -> Result<GitOut> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(dir).args(args).stdin(std::process::Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
    let out = tokio::time::timeout(Duration::from_secs(600), cmd.output())
        .await
        .context("git did not finish within 600 s")?
        .context("running git")?;
    Ok(GitOut {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

async fn git_ok(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git(dir, args).await?;
    if !out.ok {
        bail!("git {} failed: {}", args.join(" "), out.stderr);
    }
    Ok(out.stdout)
}

/// The directory of one repo under the work dir.
pub fn repo_dir(work_dir: &Path, repo: &str) -> PathBuf {
    work_dir.join(repo.replace('/', "__"))
}

pub fn worktree_path(work_dir: &Path, repo: &str, item: i64) -> PathBuf {
    repo_dir(work_dir, repo).join(format!("item-{item}"))
}

/// Refuse a work dir inside the Nucleus checkout: the agents' worktrees
/// must not be part of the Nucleus repo.
pub fn check_work_dir(work_dir: &Path, workspace_root: &Path) -> Result<()> {
    if !work_dir.is_absolute() {
        bail!("[intake] work_dir must be an absolute path (or start with ~/), got {}", work_dir.display());
    }
    let ws = canonical_lenient(workspace_root);
    let wd = canonical_lenient(work_dir);
    if wd.starts_with(&ws) {
        bail!("[intake] work_dir {} is inside the Nucleus checkout; choose a directory outside it", wd.display());
    }
    Ok(())
}

/// `p` with its longest existing prefix canonicalized (symlinks such as
/// macOS's `/var` → `/private/var` resolved) and the rest appended.
fn canonical_lenient(p: &Path) -> PathBuf {
    let mut existing = p.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.file_name().map(|n| n.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
    let mut out = existing.canonicalize().unwrap_or(existing);
    for name in rest.into_iter().rev() {
        out.push(name);
    }
    out
}

/// Clone the repo on first use, then fetch. Returns the base clone path.
pub async fn ensure_base(gh: &dyn GhRunner, work_dir: &Path, repo: &str) -> Result<PathBuf> {
    let dir = repo_dir(work_dir, repo);
    let base = dir.join("base");
    if !base.join(".git").exists() {
        std::fs::create_dir_all(&dir)?;
        github::clone_repo(gh, repo, &base).await?;
    }
    git_ok(&base, &["fetch", "origin", "--prune"]).await?;
    Ok(base)
}

/// The branch pull requests target: configured, or the remote's HEAD.
pub async fn default_branch(base: &Path, configured: Option<&str>) -> Result<String> {
    if let Some(b) = configured.filter(|b| !b.trim().is_empty()) {
        return Ok(b.trim().to_string());
    }
    let head = git(base, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).await?;
    if head.ok {
        if let Some(b) = head.stdout.strip_prefix("origin/") {
            return Ok(b.to_string());
        }
    }
    let _ = git(base, &["remote", "set-head", "origin", "--auto"]).await?;
    let head = git_ok(base, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).await?;
    head.strip_prefix("origin/").map(str::to_string).context("cannot tell the remote's default branch")
}

/// Create (or reset) the item's worktree, detached at `origin/<base_ref>`.
/// Used for the read-only eval and refinement agents, and again right
/// before implementation so the work starts from the newest base.
pub async fn prepare_worktree(base: &Path, wt: &Path, base_ref: &str) -> Result<()> {
    let target = format!("origin/{base_ref}");
    if wt.join(".git").exists() {
        git_ok(wt, &["reset", "--hard", "--quiet"]).await?;
        git_ok(wt, &["clean", "-fdq"]).await?;
        git_ok(wt, &["checkout", "--quiet", "--detach", &target]).await?;
        return Ok(());
    }
    // A leftover directory without a worktree (a crash mid-add) is stale.
    if wt.exists() {
        std::fs::remove_dir_all(wt).with_context(|| format!("removing stale {}", wt.display()))?;
        let _ = git(base, &["worktree", "prune"]).await;
    }
    git_ok(base, &["worktree", "add", "--detach", &wt.to_string_lossy(), &target]).await?;
    Ok(())
}

/// Put the worktree on `branch` (created from the current commit, or the
/// existing branch on a retry).
pub async fn switch_branch(wt: &Path, branch: &str) -> Result<()> {
    if git(wt, &["switch", "--quiet", "-c", branch]).await?.ok {
        return Ok(());
    }
    git_ok(wt, &["switch", "--quiet", branch]).await?;
    Ok(())
}

/// Commit whatever the agent left uncommitted. Returns true when it did.
pub async fn commit_leftovers(wt: &Path, message: &str) -> Result<bool> {
    let status = git_ok(wt, &["status", "--porcelain"]).await?;
    if status.is_empty() {
        return Ok(false);
    }
    git_ok(wt, &["add", "-A"]).await?;
    git_ok(wt, &["commit", "--quiet", "-m", message]).await?;
    Ok(true)
}

/// Commits on HEAD that `origin/<base_ref>` does not have.
pub async fn commits_ahead(wt: &Path, base_ref: &str) -> Result<u64> {
    let n = git_ok(wt, &["rev-list", "--count", &format!("origin/{base_ref}..HEAD")]).await?;
    n.trim().parse().context("git rev-list --count")
}

pub async fn push(wt: &Path, branch: &str) -> Result<()> {
    git_ok(wt, &["push", "--quiet", "-u", "origin", &format!("HEAD:refs/heads/{branch}")]).await?;
    Ok(())
}

/// Remove the item's worktree (the branch stays in the base clone and on
/// the remote).
pub async fn remove_worktree(base: &Path, wt: &Path) -> Result<()> {
    if wt.exists() {
        let out = git(base, &["worktree", "remove", "--force", &wt.to_string_lossy()]).await?;
        if !out.ok {
            std::fs::remove_dir_all(wt).ok();
            let _ = git(base, &["worktree", "prune"]).await;
        }
    }
    Ok(())
}

/// Result of Nucleus's own test run.
pub struct TestRun {
    /// `passed`, `failed`, `timeout`, `not_run`.
    pub status: &'static str,
    /// The end of the combined output.
    pub output: String,
}

/// Run the repo's test command in the worktree (`sh -c`), with a time
/// limit. The command comes from the operator's configuration, never from
/// an agent.
pub async fn run_tests(wt: &Path, command: Option<&str>, timeout: Duration) -> Result<TestRun> {
    let Some(command) = command.filter(|c| !c.trim().is_empty()) else {
        return Ok(TestRun { status: "not_run", output: String::new() });
    };
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(wt).stdin(std::process::Stdio::null()).kill_on_drop(true);
    for var in crate::proc_tree::SESSION_VARS {
        cmd.env_remove(var);
    }
    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => return Ok(TestRun { status: "timeout", output: format!("stopped after {} s", timeout.as_secs()) }),
        Ok(o) => o.context("running the test command")?,
    };
    let mut all = String::from_utf8_lossy(&out.stdout).into_owned();
    all.push_str(&String::from_utf8_lossy(&out.stderr));
    let tail: String = {
        let chars: Vec<char> = all.chars().collect();
        let start = chars.len().saturating_sub(4_000);
        chars[start..].iter().collect()
    };
    Ok(TestRun { status: if out.status.success() { "passed" } else { "failed" }, output: tail })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let ok = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).status().unwrap().success();
        assert!(ok, "{script}");
    }

    /// A bare "remote" with one commit on `main`, and a base clone of it.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        sh(&root, "git init -q --bare -b main remote.git && git clone -q remote.git seed 2>/dev/null");
        let seed = root.join("seed");
        sh(&seed, "git config user.email t@example.invalid && git config user.name T && echo one > a.txt && git add a.txt && git commit -qm init && git push -q origin HEAD:main");
        sh(&root, "git clone -q remote.git base");
        sh(&root.join("base"), "git config user.email t@example.invalid && git config user.name T");
        (d, root.join("base"), root)
    }

    #[tokio::test]
    async fn worktree_branch_commit_push_cycle() {
        let (_d, base, root) = fixture();
        assert_eq!(default_branch(&base, None).await.unwrap(), "main");
        assert_eq!(default_branch(&base, Some("develop")).await.unwrap(), "develop");
        let wt = root.join("item-1");
        prepare_worktree(&base, &wt, "main").await.unwrap();
        assert!(wt.join("a.txt").exists());
        switch_branch(&wt, "nucleus/item-1-x").await.unwrap();
        assert!(!commit_leftovers(&wt, "nothing").await.unwrap());
        std::fs::write(wt.join("a.txt"), "two\n").unwrap();
        assert!(commit_leftovers(&wt, "leftover").await.unwrap());
        assert_eq!(commits_ahead(&wt, "main").await.unwrap(), 1);
        push(&wt, "nucleus/item-1-x").await.unwrap();
        let remote_branches = git_ok(&root.join("remote.git"), &["branch", "--list"]).await.unwrap();
        assert!(remote_branches.contains("nucleus/item-1-x"), "{remote_branches}");
        // Re-preparing resets to the base; switching again reuses the branch.
        prepare_worktree(&base, &wt, "main").await.unwrap();
        switch_branch(&wt, "nucleus/item-1-x").await.unwrap();
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "two\n");
        remove_worktree(&base, &wt).await.unwrap();
        assert!(!wt.exists());
    }

    #[tokio::test]
    async fn tests_run_with_a_status_and_a_limit() {
        let d = tempfile::tempdir().unwrap();
        let r = run_tests(d.path(), Some("echo ok"), Duration::from_secs(10)).await.unwrap();
        assert_eq!((r.status, r.output.trim()), ("passed", "ok"));
        let r = run_tests(d.path(), Some("echo bad >&2; exit 3"), Duration::from_secs(10)).await.unwrap();
        assert_eq!((r.status, r.output.trim()), ("failed", "bad"));
        let r = run_tests(d.path(), Some("sleep 5"), Duration::from_millis(200)).await.unwrap();
        assert_eq!(r.status, "timeout");
        assert_eq!(run_tests(d.path(), None, Duration::from_secs(1)).await.unwrap().status, "not_run");
    }

    #[test]
    fn work_dir_must_be_outside_the_checkout() {
        let ws = tempfile::tempdir().unwrap();
        assert!(check_work_dir(&ws.path().join("work"), ws.path()).is_err());
        let other = tempfile::tempdir().unwrap();
        assert!(check_work_dir(other.path(), ws.path()).is_ok());
        assert!(check_work_dir(Path::new("relative"), ws.path()).is_err());
        assert_eq!(repo_dir(Path::new("/w"), "acme/widget"), PathBuf::from("/w/acme__widget"));
    }
}
