//! Git operations the pipeline's own code performs (ADR-036). Network steps
//! (fetch, push) happen here, never in an agent session.
//!
//! Layout under the configured work directory, outside the Nucleus checkout:
//!
//! - `<work_dir>/<owner>__<name>/mirror.git` — a bare repository that only
//!   Nucleus uses. It fetches from and pushes to the configured remote URL.
//!   Before every network step Nucleus writes its `config` file again from
//!   a fixed template and removes hooks, alternates and similar files, so a
//!   change an agent made to it has no effect.
//! - `<work_dir>/<owner>__<name>/item-<n>` — a separate clone of the mirror
//!   for one item. The item's agents work there. Nucleus never runs git
//!   with that clone's configuration after an agent could have written it:
//!   it reads the agent's commits by fetching from the clone into the
//!   mirror, and it commits leftover changes with the mirror's
//!   configuration and a temporary index (`--git-dir=<mirror>
//!   --work-tree=<clone>`).
//!
//! Every git process Nucleus starts runs with [`HARDENING`]: hooks off,
//! `core.fsmonitor` off, credential helpers reset, the `ext::` transport
//! off, no system configuration, and inherited `GIT_*` variables removed.
//! Network steps name the remote by its configured URL, never by a remote
//! name, and set the `gh` credential helper explicitly.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Output of one git command.
pub struct GitOut {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// `-c` options on every git command Nucleus runs. Command-line options
/// take precedence over every configuration file, so a repository's own
/// configuration cannot turn these back on.
pub const HARDENING: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "credential.helper=",
    "-c",
    "protocol.ext.allow=never",
    "-c",
    "core.sshCommand=ssh",
    "-c",
    "core.askPass=",
    "-c",
    "core.pager=cat",
    "-c",
    "core.editor=true",
    "-c",
    "commit.gpgSign=false",
    "-c",
    "tag.gpgSign=false",
    "-c",
    "diff.external=",
    "-c",
    "advice.detachedHead=false",
];

/// Branch names Nucleus never pushes to, besides the repository's default
/// branch.
pub const PROTECTED_BRANCHES: &[&str] = &["main", "master", "develop", "development", "trunk", "production", "release", "gh-pages"];

/// Where Nucleus fetches from and pushes to, and how it authenticates.
#[derive(Debug, Clone)]
pub struct Remote {
    /// The configured repository's URL (never read from a clone).
    pub url: String,
    /// The `gh` binary, used as the credential helper for network steps.
    pub gh_bin: String,
}

impl Remote {
    /// The URL of `repo` from the configured template (`{repo}` is
    /// replaced by `owner/name`).
    pub fn for_repo(template: &str, repo: &str, gh_bin: &str) -> Result<Remote> {
        check_repo_name(repo)?;
        let url = template.replace("{repo}", repo);
        if url.trim().is_empty() || url.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
            bail!("the remote URL for {repo} is empty or contains a character that is not allowed");
        }
        if url.starts_with('-') || url.starts_with("ext::") {
            bail!("the remote URL for {repo} is not a repository URL");
        }
        Ok(Remote { url, gh_bin: gh_bin.to_string() })
    }

    /// The `-c` options that set the `gh` credential helper (after
    /// [`HARDENING`] reset the list).
    fn credential_args(&self) -> Vec<String> {
        let quoted = format!("'{}'", self.gh_bin.replace('\'', r"'\''"));
        vec!["-c".into(), format!("credential.helper=!{quoted} auth git-credential")]
    }
}

/// `owner/name` with GitHub's characters only.
pub fn check_repo_name(repo: &str) -> Result<()> {
    let ok_part = |p: &str| {
        !p.is_empty() && p != "." && p != ".." && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    match repo.split_once('/') {
        Some((o, n)) if ok_part(o) && ok_part(n) => Ok(()),
        _ => bail!("{repo:?} is not a repository name of the form owner/name"),
    }
}

/// Run git in `cwd` with [`HARDENING`], extra `-c` options and extra
/// environment. Inherited `GIT_*` variables are removed except the author
/// and committer identity, so a caller's `GIT_DIR` or `GIT_INDEX_FILE` (a
/// git hook's environment) cannot redirect the command.
async fn run(cwd: &Path, pre: &[String], args: &[&str], env: &[(&str, &str)]) -> Result<GitOut> {
    let mut cmd = tokio::process::Command::new("git");
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy().into_owned();
        if k.starts_with("GIT_") && !k.starts_with("GIT_AUTHOR_") && !k.starts_with("GIT_COMMITTER_") {
            cmd.env_remove(k);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.current_dir(cwd).args(HARDENING).args(pre).args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(600), cmd.output())
        .await
        .context("git did not finish within 600 s")?
        .context("running git")?;
    Ok(GitOut {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// A hardened git command in `dir`.
pub async fn git(dir: &Path, args: &[&str]) -> Result<GitOut> {
    run(dir, &[], args, &[]).await
}

async fn git_ok(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git(dir, args).await?;
    if !out.ok {
        bail!("git {} failed: {}", args.join(" "), out.stderr);
    }
    Ok(out.stdout)
}

/// A hardened git command on the mirror (`--git-dir=<mirror>`).
async fn mirror_git(mirror: &Path, pre: &[String], args: &[&str], env: &[(&str, &str)]) -> Result<GitOut> {
    let mut all = vec![format!("--git-dir={}", mirror.display())];
    all.extend(pre.iter().cloned());
    run(mirror, &all, args, env).await
}

async fn mirror_ok(mirror: &Path, args: &[&str]) -> Result<String> {
    let out = mirror_git(mirror, &[], args, &[]).await?;
    if !out.ok {
        bail!("git {} failed: {}", args.join(" "), out.stderr);
    }
    Ok(out.stdout)
}

/// The directory of one repo under the work dir.
pub fn repo_dir(work_dir: &Path, repo: &str) -> PathBuf {
    work_dir.join(repo.replace('/', "__"))
}

pub fn mirror_path(work_dir: &Path, repo: &str) -> PathBuf {
    repo_dir(work_dir, repo).join("mirror.git")
}

/// The item's clone.
pub fn worktree_path(work_dir: &Path, repo: &str, item: i64) -> PathBuf {
    repo_dir(work_dir, repo).join(format!("item-{item}"))
}

/// The mirror ref that holds the commit Nucleus collected from item `n`.
pub fn item_ref(item: i64) -> String {
    format!("refs/nucleus/item-{item}")
}

/// Refuse a work dir inside the Nucleus checkout: the agents' clones must
/// not be part of the Nucleus repo.
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

/// The only configuration the mirror has: written again before every
/// network step.
fn mirror_config(url: &str) -> String {
    format!(
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n\
         [remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/heads/*\n"
    )
}

/// Files in a git directory that can run commands, redirect object or ref
/// lookups, or add configuration. Removed from the mirror before every use.
const MIRROR_STRAY: &[&str] = &["hooks", "objects/info/alternates", "objects/info/http-alternates", "commondir", "config.worktree", "info/attributes", "gitdir"];

/// Put the mirror back into the state Nucleus defines: its `config` is the
/// template for `remote`, and hooks, alternates and similar files are
/// gone. A mirror path that is a symlink or not a directory is removed and
/// created again. Returns true when the stored remote URL differed (the
/// mirror was changed by someone else).
pub async fn reset_mirror(mirror: &Path, remote: &Remote) -> Result<bool> {
    let meta = std::fs::symlink_metadata(mirror);
    let usable = matches!(&meta, Ok(m) if m.is_dir()) && mirror.join("objects").is_dir() && mirror.join("HEAD").is_file();
    if !usable {
        if let Ok(m) = &meta {
            if m.is_dir() {
                std::fs::remove_dir_all(mirror).with_context(|| format!("removing {}", mirror.display()))?;
            } else {
                std::fs::remove_file(mirror).with_context(|| format!("removing {}", mirror.display()))?;
            }
        }
        let parent = mirror.parent().context("the mirror path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let out = run(parent, &[], &["init", "--quiet", "--bare", "--template=", &mirror.to_string_lossy()], &[]).await?;
        if !out.ok {
            bail!("git init --bare {} failed: {}", mirror.display(), out.stderr);
        }
    }
    let config_path = mirror.join("config");
    let before = std::fs::read_to_string(&config_path).unwrap_or_default();
    let changed = usable && !before.contains(&format!("\turl = {}\n", remote.url));
    let wanted = mirror_config(&remote.url);
    if before != wanted {
        if std::fs::symlink_metadata(&config_path).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            std::fs::remove_file(&config_path)?;
        }
        std::fs::write(&config_path, wanted)?;
    }
    for stray in MIRROR_STRAY {
        let p = mirror.join(stray);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&p)?,
            Ok(_) => std::fs::remove_file(&p)?,
            Err(_) => {}
        }
    }
    if changed {
        tracing::warn!(mirror = %mirror.display(), "intake: the mirror's remote was changed; it was reset to the configured URL");
    }
    Ok(changed)
}

/// The mirror, reset (no network step). Every use of the mirror goes
/// through this or [`sync_mirror`].
pub async fn open_mirror(work_dir: &Path, repo: &str, remote: &Remote) -> Result<PathBuf> {
    let mirror = mirror_path(work_dir, repo);
    reset_mirror(&mirror, remote).await?;
    Ok(mirror)
}

/// Create or reset the mirror and fetch every branch from the configured
/// remote. Returns the mirror path.
pub async fn sync_mirror(work_dir: &Path, repo: &str, remote: &Remote) -> Result<PathBuf> {
    let mirror = mirror_path(work_dir, repo);
    reset_mirror(&mirror, remote).await?;
    let out = mirror_git(
        &mirror,
        &remote.credential_args(),
        &["fetch", "--quiet", "--prune", "--no-tags", "--update-head-ok", &remote.url, "+refs/heads/*:refs/heads/*"],
        &[],
    )
    .await?;
    if !out.ok {
        bail!("fetching {} failed: {}", repo, out.stderr);
    }
    Ok(mirror)
}

/// The branch pull requests target: configured, or the remote's HEAD
/// (asked from the configured URL).
pub async fn default_branch(mirror: &Path, remote: &Remote, configured: Option<&str>) -> Result<String> {
    if let Some(b) = configured.filter(|b| !b.trim().is_empty()) {
        return Ok(b.trim().to_string());
    }
    remote_head(mirror, remote).await
}

/// The remote's HEAD branch, read live.
pub async fn remote_head(mirror: &Path, remote: &Remote) -> Result<String> {
    let out = mirror_git(mirror, &remote.credential_args(), &["ls-remote", "--symref", &remote.url, "HEAD"], &[]).await?;
    if !out.ok {
        bail!("reading the remote's HEAD failed: {}", out.stderr);
    }
    out.stdout
        .lines()
        .find_map(|l| l.strip_prefix("ref: refs/heads/").and_then(|r| r.split_whitespace().next()).map(str::to_string))
        .context("cannot tell the remote's default branch")
}

/// Create the item's clone again from the mirror, detached at `base_ref`.
/// Used for the read-only eval and refinement agents, and again right
/// before implementation so the work starts from the newest base. When the
/// mirror holds collected work for the item (`refs/nucleus/item-<n>`,
/// a retry after the clone was lost) and `branch` is given, the clone is
/// put on `branch` at that work; otherwise `branch` is created at the base.
pub async fn prepare_clone(mirror: &Path, wt: &Path, base_ref: &str, item: i64, branch: Option<&str>) -> Result<()> {
    if let Some(b) = branch {
        check_branch_name(b)?;
    }
    if std::fs::symlink_metadata(wt).is_ok() {
        remove_clone(wt)?;
    }
    let parent = wt.parent().context("the clone path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let out = run(
        parent,
        &[],
        &["clone", "--quiet", "--no-hardlinks", "--template=", "--no-checkout", &mirror.to_string_lossy(), &wt.to_string_lossy()],
        &[],
    )
    .await?;
    if !out.ok {
        bail!("cloning the mirror into {} failed: {}", wt.display(), out.stderr);
    }
    git_ok(wt, &["checkout", "--quiet", "--detach", &format!("origin/{base_ref}")]).await?;
    if let Some(b) = branch {
        let saved = mirror_git(mirror, &[], &["rev-parse", "--verify", "--quiet", &format!("{}^{{commit}}", item_ref(item))], &[]).await?;
        if saved.ok {
            git_ok(wt, &["fetch", "--quiet", "--no-tags", "origin", &format!("+{}:refs/heads/{b}", item_ref(item))]).await?;
            git_ok(wt, &["switch", "--quiet", b]).await?;
        } else {
            git_ok(wt, &["switch", "--quiet", "-c", b]).await?;
        }
    }
    Ok(())
}

/// A git branch name Nucleus may create: letters, digits, `/`, `-`, `_`,
/// `.`, no `..`, no leading `-`.
pub fn check_branch_name(b: &str) -> Result<()> {
    let ok = !b.is_empty()
        && !b.starts_with('-')
        && !b.contains("..")
        && !b.ends_with('/')
        && !b.ends_with(".lock")
        && b.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'));
    if !ok {
        bail!("{b:?} is not a branch name Nucleus uses");
    }
    Ok(())
}

/// Read the agent's work into the mirror: fetch the clone's `HEAD` into
/// `refs/nucleus/item-<n>`, check that it descends from the base branch,
/// and commit what the agent left uncommitted on top (with the mirror's
/// configuration and a temporary index; the clone's configuration and
/// hooks are not used). Returns the commit to push.
pub async fn collect(mirror: &Path, remote: &Remote, wt: &Path, base_ref: &str, item: i64, leftovers_message: &str) -> Result<String> {
    reset_mirror(mirror, remote).await?;
    let r = item_ref(item);
    let out = mirror_git(
        mirror,
        &["-c".into(), "protocol.file.allow=always".into()],
        &["fetch", "--quiet", "--no-tags", "--no-write-fetch-head", &wt.to_string_lossy(), &format!("+HEAD:{r}")],
        &[],
    )
    .await?;
    if !out.ok {
        bail!("reading the agent's commits from {} failed: {}", wt.display(), out.stderr);
    }
    let base = format!("refs/heads/{base_ref}");
    let desc = mirror_git(mirror, &[], &["merge-base", "--is-ancestor", &base, &r], &[]).await?;
    if !desc.ok {
        bail!("the agent's HEAD does not descend from {base_ref}; Nucleus pushes only work based on the default branch");
    }
    let index = mirror.join(format!("nucleus-index-{item}"));
    let _ = std::fs::remove_file(&index);
    let index_s = index.to_string_lossy().into_owned();
    let wt_arg = format!("--work-tree={}", wt.display());
    let pre = vec![wt_arg];
    let env_index = [("GIT_INDEX_FILE", index_s.as_str())];
    for args in [vec!["read-tree", r.as_str()], vec!["add", "--all", "--", "."]] {
        let out = mirror_git(mirror, &pre, &args, &env_index).await?;
        if !out.ok {
            let _ = std::fs::remove_file(&index);
            bail!("git {} failed: {}", args.join(" "), out.stderr);
        }
    }
    let tree = mirror_git(mirror, &pre, &["write-tree"], &env_index).await?;
    let _ = std::fs::remove_file(&index);
    if !tree.ok {
        bail!("git write-tree failed: {}", tree.stderr);
    }
    let tree = tree.stdout.trim().to_string();
    let head_tree = mirror_ok(mirror, &["rev-parse", &format!("{r}^{{tree}}")]).await?;
    if tree != head_tree.trim() {
        let ident = mirror_git(mirror, &[], &["var", "GIT_COMMITTER_IDENT"], &[]).await?;
        let fallback = [
            ("GIT_AUTHOR_NAME", "Nucleus"),
            ("GIT_AUTHOR_EMAIL", "nucleus@localhost"),
            ("GIT_COMMITTER_NAME", "Nucleus"),
            ("GIT_COMMITTER_EMAIL", "nucleus@localhost"),
        ];
        let env: &[(&str, &str)] = if ident.ok { &[] } else { &fallback };
        let commit = mirror_git(mirror, &[], &["commit-tree", &tree, "-p", &r, "-m", leftovers_message], env).await?;
        if !commit.ok {
            bail!("committing the agent's uncommitted changes failed: {}", commit.stderr);
        }
        let sha = commit.stdout.trim().to_string();
        mirror_ok(mirror, &["update-ref", &r, &sha]).await?;
    }
    Ok(mirror_ok(mirror, &["rev-parse", &format!("{r}^{{commit}}")]).await?.trim().to_string())
}

/// Commits in `sha` that the base branch does not have.
pub async fn commits_ahead(mirror: &Path, base_ref: &str, sha: &str) -> Result<u64> {
    let n = mirror_ok(mirror, &["rev-list", "--count", &format!("refs/heads/{base_ref}..{sha}")]).await?;
    n.trim().parse().context("git rev-list --count")
}

/// Paths `sha` changes relative to its merge base with the base branch.
pub async fn changed_files(mirror: &Path, base_ref: &str, sha: &str) -> Result<Vec<String>> {
    let out = mirror_ok(
        mirror,
        &["diff", "--name-only", "-z", "--no-renames", "--no-ext-diff", "--no-textconv", &format!("refs/heads/{base_ref}...{sha}")],
    )
    .await?;
    Ok(out.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect())
}

/// Every line `sha` adds relative to its merge base with the base branch,
/// with the file names (what a push would publish).
pub async fn added_text(mirror: &Path, base_ref: &str, sha: &str) -> Result<String> {
    let diff = mirror_ok(
        mirror,
        &["diff", "-U0", "--no-color", "--no-renames", "--no-ext-diff", "--no-textconv", "--text", &format!("refs/heads/{base_ref}...{sha}")],
    )
    .await?;
    let mut out = String::new();
    for l in diff.lines() {
        if let Some(path) = l.strip_prefix("+++ b/") {
            out.push_str("file: ");
            out.push_str(path);
            out.push('\n');
        } else if l.starts_with('+') && !l.starts_with("+++") {
            out.push_str(&l[1..]);
            out.push('\n');
        }
    }
    Ok(out)
}

/// Refuse to push to anything but the item's own branch
/// (`nucleus/item-<n>` or `nucleus/item-<n>-<slug>`), and never to the
/// default branch or a protected name.
pub fn check_push_target(branch: &str, item: i64, default_branch: &str) -> Result<()> {
    check_branch_name(branch)?;
    let own = format!("nucleus/item-{item}");
    let is_own = branch == own || branch.strip_prefix(&format!("{own}-")).map(|s| !s.is_empty()).unwrap_or(false);
    if !is_own {
        bail!("refusing to push to {branch:?}: Nucleus pushes only the item's own branch ({own}…)");
    }
    let leaf = branch.rsplit('/').next().unwrap_or(branch);
    if branch.eq_ignore_ascii_case(default_branch)
        || PROTECTED_BRANCHES.iter().any(|p| branch.eq_ignore_ascii_case(p) || leaf.eq_ignore_ascii_case(p))
    {
        bail!("refusing to push to the protected branch {branch:?}");
    }
    Ok(())
}

/// Push exactly `sha` to `refs/heads/<branch>` at the configured URL. No
/// force, no other ref, no pre-push hook; the mirror is reset first and the
/// remote's default branch is read live and refused as a target.
pub async fn push(mirror: &Path, remote: &Remote, sha: &str, branch: &str, item: i64) -> Result<()> {
    reset_mirror(mirror, remote).await?;
    let head = remote_head(mirror, remote).await?;
    check_push_target(branch, item, &head)?;
    if !sha.chars().all(|c| c.is_ascii_hexdigit()) || sha.len() < 40 {
        bail!("{sha:?} is not a commit id");
    }
    let out = mirror_git(
        mirror,
        &remote.credential_args(),
        &["push", "--quiet", "--no-verify", "--no-follow-tags", &remote.url, &format!("{sha}:refs/heads/{branch}")],
        &[],
    )
    .await?;
    if !out.ok {
        bail!("pushing {branch} failed: {}", out.stderr);
    }
    Ok(())
}

/// Remove the item's clone (the collected commit stays in the mirror and on
/// the remote).
pub fn remove_clone(wt: &Path) -> Result<()> {
    match std::fs::symlink_metadata(wt) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(wt).with_context(|| format!("removing {}", wt.display())),
        Ok(_) => std::fs::remove_file(wt).with_context(|| format!("removing {}", wt.display())),
        Err(_) => Ok(()),
    }
}

/// Result of Nucleus's own test run.
pub struct TestRun {
    /// `passed`, `failed`, `timeout`, `not_run`.
    pub status: &'static str,
    /// The end of the combined output.
    pub output: String,
}

/// Run the repo's test command in the clone (`sh -c`), with a time limit.
/// The command comes from the operator's configuration, never from an
/// agent.
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
        let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A bare "remote" with one commit on `main`.
    fn fixture() -> (tempfile::TempDir, PathBuf, Remote) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        sh(&root, "git init -q --bare -b main remote.git && git clone -q remote.git seed 2>/dev/null");
        sh(
            &root.join("seed"),
            "git config user.email t@example.invalid && git config user.name T && echo one > a.txt && git add a.txt \
             && git commit -qm init && git push -q origin HEAD:main",
        );
        let remote = Remote { url: root.join("remote.git").to_string_lossy().into_owned(), gh_bin: "gh".into() };
        (d, root, remote)
    }

    fn remote_branches(root: &Path) -> String {
        let out = std::process::Command::new("git").args(["branch", "--list"]).current_dir(root.join("remote.git")).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[tokio::test]
    async fn mirror_clone_collect_push_cycle() {
        let (_d, root, remote) = fixture();
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        assert_eq!(default_branch(&mirror, &remote, None).await.unwrap(), "main");
        assert_eq!(default_branch(&mirror, &remote, Some("develop")).await.unwrap(), "develop");
        let wt = worktree_path(&work, "acme/widget", 1);
        prepare_clone(&mirror, &wt, "main", 1, Some("nucleus/item-1-x")).await.unwrap();
        assert!(wt.join("a.txt").exists());
        // The agent commits once and leaves one change uncommitted.
        sh(&wt, "git -c user.email=a@example.invalid -c user.name=A commit -q --allow-empty -m agent && echo two > a.txt && echo new > b.txt");
        let sha = collect(&mirror, &remote, &wt, "main", 1, "leftover").await.unwrap();
        assert_eq!(commits_ahead(&mirror, "main", &sha).await.unwrap(), 2);
        let mut files = changed_files(&mirror, "main", &sha).await.unwrap();
        files.sort();
        assert_eq!(files, ["a.txt", "b.txt"]);
        let added = added_text(&mirror, "main", &sha).await.unwrap();
        assert!(added.contains("file: a.txt\ntwo\n") && added.contains("new"), "{added}");
        push(&mirror, &remote, &sha, "nucleus/item-1-x", 1).await.unwrap();
        assert!(remote_branches(&root).contains("nucleus/item-1-x"));
        // A lost clone comes back on the branch with the collected work.
        prepare_clone(&mirror, &wt, "main", 1, Some("nucleus/item-1-x")).await.unwrap();
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "two\n");
        remove_clone(&wt).unwrap();
        assert!(!wt.exists());
    }

    #[tokio::test]
    async fn agent_controlled_git_metadata_is_not_used() {
        let (_d, root, remote) = fixture();
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 2);
        prepare_clone(&mirror, &wt, "main", 2, Some("nucleus/item-2")).await.unwrap();
        let marker = root.join("hook-ran");
        let hook = format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display());
        // Hooks in the agent's clone and in the mirror, a hooksPath and an
        // fsmonitor command in both configs, a clean filter for every file,
        // and the remotes rewritten to a decoy.
        sh(&root, "git init -q --bare decoy.git");
        let decoy = root.join("decoy.git");
        for gitdir in [wt.join(".git"), mirror.clone()] {
            std::fs::create_dir_all(gitdir.join("hooks")).unwrap();
            for name in ["pre-push", "pre-commit", "post-checkout", "reference-transaction", "pre-auto-gc"] {
                let p = gitdir.join("hooks").join(name);
                std::fs::write(&p, &hook).unwrap();
                sh(&root, &format!("chmod +x '{}'", p.display()));
            }
            sh(
                &root,
                &format!(
                    "git --git-dir='{g}' config core.hooksPath '{g}/hooks' && git --git-dir='{g}' config core.fsmonitor '{h}' \
                     && git --git-dir='{g}' config filter.x.clean 'touch {m}; cat' && git --git-dir='{g}' config remote.origin.url '{d}' \
                     && git --git-dir='{g}' config remote.origin.pushurl '{d}' && git --git-dir='{g}' config url.'{d}'.insteadOf '{r}'",
                    g = gitdir.display(),
                    h = gitdir.join("hooks/pre-push").display(),
                    m = marker.display(),
                    d = decoy.display(),
                    r = remote.url,
                ),
            );
        }
        std::fs::write(wt.join(".gitattributes"), "* filter=x\n").unwrap();
        std::fs::write(wt.join("a.txt"), "changed\n").unwrap();
        let sha = collect(&mirror, &remote, &wt, "main", 2, "leftover").await.unwrap();
        push(&mirror, &remote, &sha, "nucleus/item-2", 2).await.unwrap();
        assert!(!marker.exists(), "no hook, fsmonitor or filter ran");
        assert!(remote_branches(&root).contains("nucleus/item-2"), "pushed to the configured remote");
        let decoy_refs = std::process::Command::new("git").args(["for-each-ref"]).current_dir(&decoy).output().unwrap();
        assert!(decoy_refs.stdout.is_empty(), "nothing reached the rewritten origin");
        // The mirror's config is the template again.
        let cfg = std::fs::read_to_string(mirror.join("config")).unwrap();
        assert!(!cfg.contains("hooksPath") && !cfg.contains("insteadOf") && cfg.contains(&remote.url), "{cfg}");
        assert!(!mirror.join("hooks").exists());
    }

    #[tokio::test]
    async fn work_that_does_not_descend_from_the_base_is_refused() {
        let (_d, root, remote) = fixture();
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 3);
        prepare_clone(&mirror, &wt, "main", 3, Some("nucleus/item-3")).await.unwrap();
        sh(&wt, "git checkout -q --orphan other && git -c user.email=a@example.invalid -c user.name=A commit -qm unrelated");
        let e = collect(&mirror, &remote, &wt, "main", 3, "x").await.unwrap_err();
        assert!(format!("{e:#}").contains("does not descend"), "{e:#}");
    }

    #[test]
    fn push_targets_and_names() {
        assert!(check_push_target("nucleus/item-4", 4, "main").is_ok());
        assert!(check_push_target("nucleus/item-4-fix-typo", 4, "main").is_ok());
        assert!(check_push_target("nucleus/item-40", 4, "main").is_err(), "another item's branch");
        assert!(check_push_target("main", 4, "main").is_err());
        assert!(check_push_target("nucleus/item-4-x", 4, "nucleus/item-4-x").is_err(), "the default branch");
        assert!(check_push_target("feature/x", 4, "main").is_err());
        assert!(check_push_target("nucleus/item-4-..", 4, "main").is_err());
        assert!(check_branch_name("-x").is_err());
        assert!(Remote::for_repo("https://github.com/{repo}.git", "acme/widget", "gh").is_ok());
        assert!(Remote::for_repo("https://github.com/{repo}.git", "acme/../x", "gh").is_err());
        assert!(Remote::for_repo("ext::sh -c touch% /tmp/x", "acme/widget", "gh").is_err());
        assert_eq!(item_ref(7), "refs/nucleus/item-7");
    }

    #[tokio::test]
    async fn push_refuses_the_remote_default_branch() {
        let (_d, root, remote) = fixture();
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let sha = mirror_ok(&mirror, &["rev-parse", "refs/heads/main"]).await.unwrap();
        assert!(push(&mirror, &remote, &sha, "main", 1).await.is_err());
        // The remote's HEAD is read live: a remote whose HEAD is the item's
        // branch name refuses it too.
        sh(&root.join("remote.git"), "git branch nucleus/item-1 main && git symbolic-ref HEAD refs/heads/nucleus/item-1");
        let e = push(&mirror, &remote, &sha, "nucleus/item-1", 1).await.unwrap_err();
        assert!(format!("{e:#}").contains("protected"), "{e:#}");
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
