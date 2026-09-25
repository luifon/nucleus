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
//!   for one item. The item's agents work there. After an agent could have
//!   written to it, Nucleus runs no git command against it: no fetch from
//!   it, no command with its `.git` as git directory. [`import`] reads only
//!   its file tree, into the mirror, with a Nucleus-owned temporary index
//!   (`--git-dir=<mirror> --work-tree=<clone>`), and makes one commit with
//!   a fixed identity and a code-owned message on the trusted base. None of
//!   the agent's commits, authors or messages is published.
//!
//! Every git process Nucleus starts runs with [`HARDENING`] and no
//! configuration file except the mirror's own template: no system and no
//! global configuration (`GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`;
//! the implementation agent runs as the same OS user and can edit
//! `~/.gitconfig`), no global attributes or excludes file, inherited
//! `GIT_*` variables removed. Network steps name the remote by its
//! configured HTTPS URL, never by a remote name, and authenticate only
//! through the `gh` credential helper at its absolute path.

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
    "core.attributesFile=/dev/null",
    "-c",
    "core.excludesFile=/dev/null",
    "-c",
    "advice.detachedHead=false",
];

/// The identity and message of the one commit Nucleus publishes for an
/// item (from `nucleus.toml`, never from an agent or a git config).
#[derive(Debug, Clone)]
pub struct CommitSpec {
    pub author_name: String,
    pub author_email: String,
    pub message: String,
}

/// Branch names Nucleus never pushes to, besides the repository's default
/// branch.
pub const PROTECTED_BRANCHES: &[&str] = &["main", "master", "develop", "development", "trunk", "production", "release", "gh-pages"];

/// Where Nucleus fetches from and pushes to, and how it authenticates.
#[derive(Debug, Clone)]
pub struct Remote {
    /// The configured repository's URL (never read from a clone).
    pub url: String,
    /// The pinned `gh`, the only credential helper, for an HTTPS URL;
    /// `None` for a local repository path (tests).
    pub gh: Option<Pin>,
}

impl Remote {
    /// The URL of `repo` from the configured template (`{repo}` is
    /// replaced by `owner/name`). Only HTTPS URLs are accepted (and a local
    /// absolute path, used by tests): SSH would need an ssh command, and
    /// git would read it from a configuration file the agent can write.
    /// `gh_bin` is resolved to an absolute path through `PATH` when it is a
    /// bare name.
    pub fn for_repo(template: &str, repo: &str, gh: Option<&Pin>) -> Result<Remote> {
        check_repo_name(repo)?;
        let url = template.replace("{repo}", repo);
        if url.trim().is_empty() || url.chars().any(|c| c.is_control() || c.is_whitespace() || c == '"' || c == '\\' || c == '\'') {
            bail!("the remote URL for {repo} is empty or contains a character that is not allowed");
        }
        if url.starts_with("https://") {
            let gh = gh.context("an HTTPS remote needs gh, and no gh is pinned")?;
            return Ok(Remote { url, gh: Some(gh.clone()) });
        }
        if url.starts_with('/') {
            return Ok(Remote { url, gh: None });
        }
        bail!("the remote URL for {repo} must start with https:// (SSH and other transports are not supported)")
    }

    /// The `-c` options that set the pinned `gh` as credential helper
    /// (after [`HARDENING`] reset the list), after checking its hash: git
    /// runs the helper itself, so the check happens right before the git
    /// process that may run it starts.
    fn credential_args(&self) -> Result<Vec<String>> {
        match &self.gh {
            Some(gh) => {
                gh.verify().map_err(|why| anyhow::Error::new(super::tools::ToolChanged(why)))?;
                let quoted = format!("'{}'", gh.path.to_string_lossy().replace('\'', r"'\''"));
                Ok(vec!["-c".into(), format!("credential.helper=!{quoted} auth git-credential")])
            }
            None => Ok(vec![]),
        }
    }
}

pub use super::tools::resolve_bin;
use super::tools::Pin;

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
/// environment. Every inherited `GIT_*` variable is removed, so a caller's
/// `GIT_DIR` or `GIT_INDEX_FILE` (a git hook's environment) cannot redirect
/// the command, and no system or global configuration is read.
/// The one way intake builds a git process: the pinned git (hash checked
/// right before), [`HARDENING`], no system or global configuration, no
/// inherited `GIT_*` variable, then `env`.
fn git_command(cwd: &Path, pre: &[String], args: &[&str], env: &[(&str, &str)]) -> Result<tokio::process::Command> {
    let mut cmd = super::tools::git_pin()?.command()?;
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy().into_owned();
        if k.starts_with("GIT_") {
            cmd.env_remove(k);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.current_dir(cwd).args(HARDENING).args(pre).args(args).kill_on_drop(true);
    Ok(cmd)
}

/// Run git in `cwd` with [`HARDENING`], extra `-c` options and extra
/// environment (see [`git_command`]).
async fn run(cwd: &Path, pre: &[String], args: &[&str], env: &[(&str, &str)]) -> Result<GitOut> {
    let mut cmd = git_command(cwd, pre, args, env)?;
    cmd.stdin(std::process::Stdio::null());
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

/// Name prefix of an import's scratch directory inside the mirror (its
/// temporary object directory, index and snapshot).
pub const SCRATCH_PREFIX: &str = "nucleus-import-";
/// Scratch directories older than this are left by a stopped import and
/// removed when the mirror is opened.
pub const SCRATCH_MAX_AGE: Duration = Duration::from_secs(6 * 60 * 60);

/// Remove scratch directories older than [`SCRATCH_MAX_AGE`].
pub fn sweep_scratch(mirror: &Path) -> Result<usize> {
    let mut removed = 0;
    let Ok(entries) = std::fs::read_dir(mirror) else { return Ok(0) };
    for e in entries.flatten() {
        if !e.file_name().to_string_lossy().starts_with(SCRATCH_PREFIX) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age > SCRATCH_MAX_AGE)
            .unwrap_or(false);
        if old && std::fs::remove_dir_all(e.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
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
    sweep_scratch(mirror)?;
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
        &remote.credential_args()?,
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
    let out = mirror_git(mirror, &remote.credential_args()?, &["ls-remote", "--symref", &remote.url, "HEAD"], &[]).await?;
    if !out.ok {
        bail!("reading the remote's HEAD failed: {}", out.stderr);
    }
    out.stdout
        .lines()
        .find_map(|l| l.strip_prefix("ref: refs/heads/").and_then(|r| r.split_whitespace().next()).map(str::to_string))
        .context("cannot tell the remote's default branch")
}

/// The commit the remote's exact ref `refs/heads/<branch>` holds now
/// (`ls-remote` against the configured URL), or `None` when it does not
/// exist.
pub async fn remote_branch(mirror: &Path, remote: &Remote, branch: &str) -> Result<Option<String>> {
    check_branch_name(branch)?;
    reset_mirror(mirror, remote).await?;
    let want = format!("refs/heads/{branch}");
    let out = mirror_git(mirror, &remote.credential_args()?, &["ls-remote", &remote.url, &want], &[]).await?;
    if !out.ok {
        bail!("reading {want} at the remote failed: {}", out.stderr);
    }
    Ok(out.stdout.lines().find_map(|l| {
        let (sha, name) = l.split_once('\t')?;
        (name == want).then(|| sha.trim().to_string())
    }))
}

/// Create the item's clone again from the mirror, detached at `base_ref`.
/// Used for the read-only eval and refinement agents, and again right
/// before implementation so the work starts from the newest base. When the
/// mirror holds collected work for the item (`refs/nucleus/item-<n>`,
/// a retry after the clone was lost) and `branch` is given, the clone is
/// put on `branch` at that work; otherwise `branch` is created at the base.
/// Returns the base commit the clone starts from (the commit Nucleus's one
/// commit for the item will have as its parent), and whether collected
/// work was restored (then the item keeps the base that work was built on).
pub async fn prepare_clone(mirror: &Path, wt: &Path, base_ref: &str, item: i64, branch: Option<&str>) -> Result<(String, bool)> {
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
    let base_sha = mirror_ok(mirror, &["rev-parse", "--verify", &format!("refs/heads/{base_ref}^{{commit}}")]).await?.trim().to_string();
    let mut restored = false;
    if let Some(b) = branch {
        let saved = mirror_git(mirror, &[], &["rev-parse", "--verify", "--quiet", &format!("{}^{{commit}}", item_ref(item))], &[]).await?;
        if saved.ok {
            git_ok(wt, &["fetch", "--quiet", "--no-tags", "origin", &format!("+{}:refs/heads/{b}", item_ref(item))]).await?;
            git_ok(wt, &["switch", "--quiet", b]).await?;
            restored = true;
        } else {
            git_ok(wt, &["switch", "--quiet", "-c", b]).await?;
        }
    }
    Ok((base_sha, restored))
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

/// A policy refusal of an import (a limit, a file type, a nested repository,
/// a submodule change): retrying cannot fix it; the pipeline blocks the
/// item with this reason.
#[derive(Debug)]
pub struct ImportRefused(pub String);

impl std::fmt::Display for ImportRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ImportRefused {}

macro_rules! refuse {
    ($($t:tt)*) => {
        return Err(anyhow::Error::new(ImportRefused(format!($($t)*))))
    };
}

/// Size limits checked before anything is read into git.
#[derive(Debug, Clone, Copy)]
pub struct ImportLimits {
    /// Most paths (tracked plus untracked, not ignored) in the clone.
    pub max_files: usize,
    /// Largest file, by its length (a sparse file counts by its length).
    pub max_file_bytes: u64,
    /// Sum of all file lengths (ignore files included).
    pub max_total_bytes: u64,
    /// Largest ignore file (`.gitignore`) read to decide what is ignored.
    pub max_ignore_bytes: u64,
}

/// Import the agent's file tree into the mirror as one commit on
/// `base_sha`, with `spec`'s fixed identity and code-owned message. No git
/// command runs against the clone: git reads its files as a work tree of
/// the mirror (`--git-dir=<mirror> --work-tree=<clone>`) with a temporary
/// index. Paths named `.git` are never read (git refuses them), so the
/// clone's own repository, whatever it contains, is ignored; symlinks are
/// stored as symlinks (their target text), never followed. The clone's
/// `.gitignore` files decide which untracked files are left out, and its
/// `.gitattributes` can only name drivers, which resolve to nothing: the
/// only configuration is the mirror's template, checked here to define no
/// filter, diff or merge driver.
///
/// Before `git add` reads any file, every path git would consider is
/// checked with `lstat` (symlinks not followed): only regular files and
/// symlinks are accepted (a FIFO, socket or device is refused), and
/// `limits` caps the number of paths, each file's length and the total. A
/// new directory that is a repository is refused. After `git add`, the
/// staged submodule entries (gitlinks) must equal the base's exactly.
///
/// Objects are written into a temporary object directory (the mirror's
/// objects as an alternate) and moved into the mirror only when the commit
/// exists; a failed import leaves no loose object and no index behind.
/// Returns `None` when the tree equals the base (no change).
pub async fn import(
    mirror: &Path,
    remote: &Remote,
    wt: &Path,
    base_sha: &str,
    item: i64,
    spec: &CommitSpec,
    limits: &ImportLimits,
) -> Result<Option<String>> {
    reset_mirror(mirror, remote).await?;
    let root = super::snapshot::open_root(wt)?;
    let drivers = mirror_git(mirror, &[], &["config", "--get-regexp", r"^(filter|diff|merge)\.[^.]+\."], &[]).await?;
    if !drivers.stdout.trim().is_empty() {
        bail!("the mirror's configuration defines a filter, diff or merge driver");
    }
    let scratch = mirror.join(format!("{SCRATCH_PREFIX}{item}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(scratch.join("objects"))?;
    std::fs::create_dir_all(scratch.join("tree"))?;
    let result = import_in(mirror, &root, base_sha, item, spec, limits, &scratch).await;
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

#[allow(clippy::too_many_arguments)]
async fn import_in(
    mirror: &Path,
    root: &std::os::fd::OwnedFd,
    base_sha: &str,
    item: i64,
    spec: &CommitSpec,
    limits: &ImportLimits,
    scratch: &Path,
) -> Result<Option<String>> {
    let index_s = scratch.join("index").to_string_lossy().into_owned();
    let objects_s = scratch.join("objects").to_string_lossy().into_owned();
    let snapshot = scratch.join("tree");
    let mirror_objects = mirror.join("objects").to_string_lossy().into_owned();
    let env = [
        ("GIT_INDEX_FILE", index_s.as_str()),
        ("GIT_OBJECT_DIRECTORY", objects_s.as_str()),
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES", mirror_objects.as_str()),
    ];
    let out = mirror_git(mirror, &[], &["read-tree", base_sha], &env).await?;
    if !out.ok {
        bail!("git read-tree failed: {}", out.stderr);
    }
    // The base tree, read through the capped reader.
    let cap = limits.max_files.saturating_mul(1024).saturating_add(1 << 20);
    let Some(base_listing) =
        run_capped(mirror, &[format!("--git-dir={}", mirror.display())], &["ls-tree", "-r", "-z", base_sha], &[], cap).await?
    else {
        refuse!("the base tree lists more than the import limits allow; nothing was imported");
    };
    let base = BaseTree::parse(&base_listing);
    let base_links = gitlinks(&String::from_utf8_lossy(&base_listing), true);

    // Walk the clone ourselves (no git command reads it): descriptor-
    // relative, no-follow, one directory level at a time. Ignore files go
    // into a private rules directory under their own limit; `git
    // check-ignore --no-index` decides, from those copies only, which
    // entries of the next level are ignored; ignored directories are not
    // entered (unless the base tracks something inside them).
    let rules = scratch.join("rules");
    std::fs::create_dir_all(&rules)?;
    let mut budget = super::snapshot::Budget { files: 0, bytes: 0 };
    let candidates = walk_clone(mirror, root, &rules, &base, limits, &mut budget).await?;

    // The private snapshot: every candidate copied with descriptor-
    // relative, no-follow operations and read limits (snapshot.rs).
    for (rel, gitlink) in &candidates {
        super::snapshot::copy_path(root, rel, &snapshot, limits, &mut budget, *gitlink)?;
    }

    // git add reads only the snapshot.
    let pre = vec![format!("--work-tree={}", snapshot.display())];
    let out = mirror_git(mirror, &pre, &["add", "--all", "--", "."], &env).await?;
    if !out.ok {
        bail!("git add failed: {}", out.stderr);
    }
    let staged = mirror_git(mirror, &pre, &["ls-files", "--stage", "-z"], &env).await?;
    let staged_links = gitlinks(&staged.stdout, false);
    for link in &staged_links {
        match base_links.iter().find(|(p, _)| *p == link.0) {
            None => refuse!("the change adds a submodule at {}; Nucleus does not publish submodule changes", link.0),
            Some((_, sha)) if *sha != link.1 => {
                refuse!("the change moves the submodule {} to another commit; Nucleus does not publish submodule changes", link.0)
            }
            Some(_) => {}
        }
    }
    if let Some((p, _)) = base_links.iter().find(|(p, _)| !staged_links.iter().any(|(q, _)| q == p)) {
        refuse!("the change deletes the submodule {p}; Nucleus does not publish submodule changes");
    }
    let tree = mirror_git(mirror, &pre, &["write-tree"], &env).await?;
    if !tree.ok {
        bail!("git write-tree failed: {}", tree.stderr);
    }
    let tree = tree.stdout.trim().to_string();
    // `.gitmodules` must stay byte-identical to the base when the base has
    // submodules or either tree has the file (a changed URL is a submodule
    // change).
    let modules = |t: String| {
        let env = &env;
        async move { mirror_git(mirror, &[], &["rev-parse", "--verify", "--quiet", &format!("{t}:.gitmodules")], env).await.map(|o| o.ok.then_some(o.stdout.trim().to_string())) }
    };
    let (base_mod, new_mod) = (modules(base_sha.to_string()).await?, modules(tree.clone()).await?);
    if (!base_links.is_empty() || base_mod.is_some() || new_mod.is_some()) && base_mod != new_mod {
        refuse!("the change modifies .gitmodules; Nucleus does not publish submodule changes");
    }
    let base_tree = mirror_ok(mirror, &["rev-parse", &format!("{base_sha}^{{tree}}")]).await?;
    if tree == base_tree.trim() {
        return Ok(None);
    }
    let ident = [
        ("GIT_AUTHOR_NAME", spec.author_name.as_str()),
        ("GIT_AUTHOR_EMAIL", spec.author_email.as_str()),
        ("GIT_COMMITTER_NAME", spec.author_name.as_str()),
        ("GIT_COMMITTER_EMAIL", spec.author_email.as_str()),
        ("GIT_OBJECT_DIRECTORY", objects_s.as_str()),
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES", mirror_objects.as_str()),
    ];
    let commit = mirror_git(mirror, &[], &["commit-tree", &tree, "-p", base_sha, "-m", &spec.message], &ident).await?;
    if !commit.ok {
        bail!("creating the item's commit failed: {}", commit.stderr);
    }
    let sha = commit.stdout.trim().to_string();
    install_pack(mirror, &scratch.join("objects")).await?;
    mirror_ok(mirror, &["update-ref", &item_ref(item), &sha]).await?;
    Ok(Some(sha))
}

/// Every loose object id in an object directory.
fn loose_ids(objects: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for d in std::fs::read_dir(objects)? {
        let d = d?;
        let dn = d.file_name().to_string_lossy().into_owned();
        if dn.len() != 2 || !dn.chars().all(|c| c.is_ascii_hexdigit()) || !d.file_type()?.is_dir() {
            continue;
        }
        for f in std::fs::read_dir(d.path())? {
            let f = f?.file_name().to_string_lossy().into_owned();
            if f.chars().all(|c| c.is_ascii_hexdigit()) {
                out.push(format!("{dn}{f}"));
            }
        }
    }
    Ok(out)
}

/// Pack the new objects of a temporary object directory and install the
/// pack in the mirror: the `.pack` (and `.rev`) first, the `.idx` last, each
/// by rename. Git ignores a pack without its index, so a crash leaves no
/// half-visible objects.
async fn install_pack(mirror: &Path, objects: &Path) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let ids = loose_ids(objects)?;
    if ids.is_empty() {
        return Ok(());
    }
    let out_dir = objects.join("nucleus-pack");
    std::fs::create_dir_all(&out_dir)?;
    let objects_s = objects.to_string_lossy().into_owned();
    let mirror_objects = mirror.join("objects").to_string_lossy().into_owned();
    let base = out_dir.join("pack").to_string_lossy().into_owned();
    let mut cmd = git_command(
        mirror,
        &[format!("--git-dir={}", mirror.display())],
        &["pack-objects", "-q", &base],
        &[("GIT_OBJECT_DIRECTORY", objects_s.as_str()), ("GIT_ALTERNATE_OBJECT_DIRECTORIES", mirror_objects.as_str())],
    )?;
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("running git pack-objects")?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    stdin.write_all(format!("{}\n", ids.join("\n")).as_bytes()).await?;
    drop(stdin);
    let out = tokio::time::timeout(Duration::from_secs(600), child.wait_with_output()).await.context("git pack-objects timed out")??;
    if !out.status.success() {
        bail!("git pack-objects failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dest = mirror.join("objects/pack");
    std::fs::create_dir_all(&dest)?;
    for ext in ["pack", "rev", "idx"] {
        let from = out_dir.join(format!("pack-{hash}.{ext}"));
        if from.exists() {
            std::fs::rename(&from, dest.join(format!("pack-{hash}.{ext}"))).with_context(|| format!("installing pack-{hash}.{ext}"))?;
        } else if ext != "rev" {
            bail!("git pack-objects wrote no .{ext}");
        }
    }
    Ok(())
}

/// The paths of the base tree: tracked files and links, their parent
/// directories, and submodule paths.
struct BaseTree {
    tracked: std::collections::HashSet<Vec<u8>>,
    dirs: std::collections::HashSet<Vec<u8>>,
    gitlinks: std::collections::HashSet<Vec<u8>>,
}

impl BaseTree {
    /// From `ls-tree -r -z` output (records `mode type id\tpath`).
    fn parse(listing: &[u8]) -> BaseTree {
        let mut t = BaseTree { tracked: Default::default(), dirs: Default::default(), gitlinks: Default::default() };
        for rec in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let Some(tab) = rec.iter().position(|b| *b == b'\t') else { continue };
            let (head, path) = (&rec[..tab], rec[tab + 1..].to_vec());
            if head.starts_with(b"160000 ") {
                t.gitlinks.insert(path.clone());
            }
            let mut end = path.len();
            while let Some(p) = path[..end].iter().rposition(|b| *b == b'/') {
                t.dirs.insert(path[..p].to_vec());
                end = p;
            }
            t.tracked.insert(path);
        }
        t
    }
}

fn join_rel(parent: &[u8], name: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        name.to_vec()
    } else {
        [parent, b"/", name].concat()
    }
}

/// Walk the clone and return the paths to import (with a flag for base
/// submodule directories). See [`import`].
async fn walk_clone(
    mirror: &Path,
    root: &std::os::fd::OwnedFd,
    rules: &Path,
    base: &BaseTree,
    limits: &ImportLimits,
    budget: &mut super::snapshot::Budget,
) -> Result<Vec<(Vec<u8>, bool)>> {
    use super::snapshot::Kind;
    use std::os::unix::ffi::OsStrExt;
    let max_entries = limits.max_files.saturating_mul(50).max(100_000);
    let mut seen = 0usize;
    let mut out: Vec<(Vec<u8>, bool)> = Vec::new();
    // (directory, whether only base-tracked paths are kept below it)
    let mut level: Vec<(Vec<u8>, bool)> = vec![(Vec::new(), false)];
    while !level.is_empty() {
        let mut children: Vec<(Vec<u8>, Kind, bool, bool)> = Vec::new(); // rel, kind, tracked_only parent, is repo dir
        for (dir, tracked_only) in &level {
            let fd = super::snapshot::open_rel_dir(root, dir)?;
            let (entries, _) = super::snapshot::read_entries(&fd)?;
            for (name, kind) in entries {
                seen += 1;
                if seen > max_entries {
                    refuse!("the clone has more than {max_entries} entries on disk; nothing was imported");
                }
                let rel = join_rel(dir, &name);
                if name == b".gitignore" && kind == Kind::File && !tracked_only {
                    super::snapshot::copy_ignore_file(&fd, &rel, &rules.join(std::ffi::OsStr::from_bytes(&rel)), limits.max_ignore_bytes, limits, budget)?;
                }
                let mut repo = false;
                if kind == Kind::Dir {
                    std::fs::create_dir_all(rules.join(std::ffi::OsStr::from_bytes(&rel)))?;
                    let sub = super::snapshot::open_rel_dir(root, &rel)?;
                    repo = super::snapshot::read_entries(&sub)?.1;
                }
                children.push((rel, kind, *tracked_only, repo));
            }
        }
        let asked: Vec<&[u8]> = children.iter().filter(|c| !c.2).map(|c| c.0.as_slice()).collect();
        let ignored = check_ignore(mirror, rules, &asked).await?;
        let mut next = Vec::new();
        for (rel, kind, tracked_only, repo) in children {
            let is_ignored = tracked_only || ignored.contains(&rel);
            let tracked = base.tracked.contains(&rel);
            match kind {
                Kind::Dir if base.gitlinks.contains(&rel) => out.push((rel, true)),
                Kind::Dir => {
                    let holds_tracked = base.dirs.contains(&rel);
                    if is_ignored && !holds_tracked {
                        continue;
                    }
                    if repo {
                        refuse!("the clone contains a nested repository at {}; Nucleus does not publish one", String::from_utf8_lossy(&rel));
                    }
                    next.push((rel, is_ignored));
                }
                Kind::File | Kind::Symlink => {
                    if !is_ignored || tracked {
                        out.push((rel, false));
                    }
                }
                Kind::Special => {
                    if !is_ignored || tracked {
                        refuse!("{} is not a regular file or a symlink (a FIFO, socket or device); nothing was imported", String::from_utf8_lossy(&rel));
                    }
                }
            }
            if out.len() > limits.max_files {
                refuse!("the clone has more than {} files; nothing was imported", limits.max_files);
            }
        }
        level = next;
    }
    Ok(out)
}

/// The paths among `paths` that the ignore files in `rules` exclude
/// (`git check-ignore --no-index --stdin -z`, pinned git, trusted
/// configuration, no global excludes file). Directories exist in `rules` as
/// empty directories so that directory-only patterns apply. Input is
/// written and output read concurrently; the output is capped by the input
/// size.
async fn check_ignore(mirror: &Path, rules: &Path, paths: &[&[u8]]) -> Result<std::collections::HashSet<Vec<u8>>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if paths.is_empty() {
        return Ok(Default::default());
    }
    let input: Vec<u8> = paths.iter().flat_map(|p| p.iter().copied().chain(std::iter::once(0))).collect();
    let cap = input.len() + 4096;
    let mut cmd = git_command(
        mirror,
        &[format!("--git-dir={}", mirror.display()), format!("--work-tree={}", rules.display())],
        &["check-ignore", "--no-index", "--stdin", "-z"],
        &[],
    )?;
    cmd.current_dir(rules).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("running git check-ignore")?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let writer = tokio::spawn(async move {
        let r = stdin.write_all(&input).await;
        drop(stdin);
        r
    });
    let mut stdout = child.stdout.take().context("no stdout")?;
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 1 << 16];
    loop {
        let n = stdout.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > cap {
            let _ = child.kill().await;
            bail!("git check-ignore printed more than it was given");
        }
    }
    let _ = writer.await;
    let out = child.wait_with_output().await?;
    // Exit 1: no path is ignored.
    match out.status.code() {
        Some(0) | Some(1) => {}
        _ => bail!("git check-ignore failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
    Ok(buf.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| p.to_vec()).collect())
}

/// Submodule entries (mode 160000) of `ls-tree -r -z` (`tree` true) or
/// `ls-files --stage -z` output: (path, commit).
fn gitlinks(out: &str, tree: bool) -> Vec<(String, String)> {
    out.split('\0')
        .filter_map(|rec| {
            let (head, path) = rec.split_once('\t')?;
            let mut f = head.split_whitespace();
            let mode = f.next()?;
            if mode != "160000" {
                return None;
            }
            if tree {
                f.next(); // "commit"
            }
            Some((path.to_string(), f.next()?.to_string()))
        })
        .collect()
}

#[cfg(test)]
fn walk_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let t = e.file_type()?;
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() {
                out.push(e.path());
            }
        }
    }
    Ok(out)
}

/// Run git and read at most `max_bytes` of its output; `None` when the
/// output is longer (the process is stopped). A failure is an error.
async fn run_capped(cwd: &Path, pre: &[String], args: &[&str], env: &[(&str, &str)], max_bytes: usize) -> Result<Option<Vec<u8>>> {
    use tokio::io::AsyncReadExt;
    let mut cmd = git_command(cwd, pre, args, env)?;
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("running git")?;
    let mut stdout = child.stdout.take().context("no stdout")?;
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 1 << 16];
    let read = async {
        loop {
            let n = stdout.read(&mut chunk).await?;
            if n == 0 {
                return Ok::<bool, std::io::Error>(true);
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > max_bytes {
                return Ok(false);
            }
        }
    };
    let complete = tokio::time::timeout(Duration::from_secs(600), read).await.context("git did not finish within 600 s")??;
    if !complete {
        let _ = child.kill().await;
        return Ok(None);
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(Some(buf))
}

/// The author line and message of `sha` (what the push would publish
/// besides the diff).
pub async fn commit_header(mirror: &Path, sha: &str) -> Result<String> {
    mirror_ok(mirror, &["log", "-1", "--format=%an <%ae>%n%cn <%ce>%n%B", sha]).await
}

/// Paths the commit `sha` changes relative to its parent `base_sha`.
pub async fn changed_files(mirror: &Path, base_sha: &str, sha: &str) -> Result<Vec<String>> {
    let out = mirror_ok(mirror, &["diff", "--name-only", "-z", "--no-renames", "--no-ext-diff", "--no-textconv", base_sha, sha]).await?;
    Ok(out.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect())
}

/// Every line the commit `sha` adds relative to its parent `base_sha`, with
/// every file name it touches (what a push would publish; symlink targets
/// appear as added lines). The diff is read through a bounded reader:
/// `None` when it is longer than `max_bytes` (the caller blocks the item;
/// a diff is never cut and passed on).
pub async fn added_text(mirror: &Path, base_sha: &str, sha: &str, max_bytes: usize) -> Result<Option<String>> {
    let Some(raw) = run_capped(
        mirror,
        &[format!("--git-dir={}", mirror.display())],
        &["diff", "-U0", "--no-color", "--no-renames", "--no-ext-diff", "--no-textconv", "--text", base_sha, sha],
        &[],
        max_bytes,
    )
    .await?
    else {
        return Ok(None);
    };
    let diff = String::from_utf8_lossy(&raw);
    let mut out = diff_additions(&diff);
    // The whole bounded diff goes to the guard as well (removed lines
    // included), so a parsing mistake cannot hide an added line.
    out.push_str("\n--- full diff ---\n");
    out.push_str(&diff);
    Ok(Some(out))
}

/// The file names and added lines of a `git diff -U0` text. `---` / `+++`
/// are file headers only outside a hunk (after `diff --git`, before the
/// first `@@`); inside a hunk every line starting with `+` is an added line,
/// including one whose content starts with `++`.
pub fn diff_additions(diff: &str) -> String {
    let mut out = String::new();
    let mut in_hunk = false;
    for l in diff.lines() {
        if l.starts_with("diff --git ") {
            in_hunk = false;
            continue;
        }
        if !in_hunk {
            if let Some(path) = l.strip_prefix("--- a/").or_else(|| l.strip_prefix("+++ b/")) {
                out.push_str("file: ");
                out.push_str(path);
                out.push('\n');
            } else if l.starts_with("@@") {
                in_hunk = true;
            }
            continue;
        }
        if l.starts_with("@@") {
            continue;
        }
        if let Some(added) = l.strip_prefix('+') {
            out.push_str(added);
            out.push('\n');
        }
    }
    out
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

/// Push exactly `sha` to `refs/heads/<branch>` at the configured URL, no
/// other ref, no pre-push hook; the mirror is reset first.
/// `remote_default` is the remote's default branch, read live by the
/// caller while preparing (so no network read sits between the caller's
/// final source check and the push); it is refused as a target.
///
/// The push is leased on the exact ref: `last_pushed` is `None` for the
/// first push, which then only creates the branch (it fails when the
/// branch exists, whatever it holds, so a branch that became the default
/// or someone else's cannot be overwritten); later pushes succeed only
/// while the remote branch still holds the commit Nucleus pushed last.
pub async fn push(
    mirror: &Path,
    remote: &Remote,
    sha: &str,
    branch: &str,
    item: i64,
    remote_default: &str,
    last_pushed: Option<&str>,
) -> Result<()> {
    reset_mirror(mirror, remote).await?;
    check_push_target(branch, item, remote_default)?;
    for id in std::iter::once(sha).chain(last_pushed) {
        if !id.chars().all(|c| c.is_ascii_hexdigit()) || id.len() < 40 {
            bail!("{id:?} is not a commit id");
        }
    }
    let out = mirror_git(
        mirror,
        &remote.credential_args()?,
        &[
            "push",
            "--quiet",
            "--no-verify",
            "--no-follow-tags",
            &format!("--force-with-lease=refs/heads/{branch}:{}", last_pushed.unwrap_or("")),
            &remote.url,
            &format!("{sha}:refs/heads/{branch}"),
        ],
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

/// Most bytes of test output kept in memory: the tail (stdout and stderr
/// interleaved as they arrive). Older output is dropped while reading.
pub const TEST_OUTPUT_TAIL_BYTES: usize = 256 * 1024;

/// Keep the last `cap` bytes appended to it.
struct Tail {
    buf: std::collections::VecDeque<u8>,
    cap: usize,
}

impl Tail {
    fn push(&mut self, data: &[u8]) {
        let data = if data.len() > self.cap { &data[data.len() - self.cap..] } else { data };
        let over = (self.buf.len() + data.len()).saturating_sub(self.cap);
        self.buf.drain(..over);
        self.buf.extend(data);
    }
}

/// Run the repo's test command in the clone (`sh -c`), with a time limit.
/// The command comes from the operator's configuration, never from an
/// agent. It runs in its own process group; stdout and stderr are drained
/// at once into a buffer that keeps the last [`TEST_OUTPUT_TAIL_BYTES`];
/// on timeout the whole group is killed and the shell reaped.
pub async fn run_tests(wt: &Path, command: Option<&str>, timeout: Duration) -> Result<TestRun> {
    use tokio::io::AsyncReadExt;
    let Some(command) = command.filter(|c| !c.trim().is_empty()) else {
        return Ok(TestRun { status: "not_run", output: String::new() });
    };
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(wt)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    for var in crate::proc_tree::SESSION_VARS {
        cmd.env_remove(var);
    }
    let mut child = cmd.spawn().context("running the test command")?;
    let pgid = child.id().map(|p| p as i32);
    let tail = std::sync::Arc::new(std::sync::Mutex::new(Tail { buf: Default::default(), cap: TEST_OUTPUT_TAIL_BYTES }));
    let mut readers = Vec::new();
    let out: Box<dyn tokio::io::AsyncRead + Unpin + Send> = Box::new(child.stdout.take().context("no stdout")?);
    let err: Box<dyn tokio::io::AsyncRead + Unpin + Send> = Box::new(child.stderr.take().context("no stderr")?);
    for mut r in [out, err] {
        let tail = tail.clone();
        readers.push(tokio::spawn(async move {
            let mut chunk = vec![0u8; 1 << 16];
            while let Ok(n) = r.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                tail.lock().unwrap().push(&chunk[..n]);
            }
        }));
    }
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(s) => Some(s.context("waiting for the test command")?),
        Err(_) => {
            if let Some(g) = pgid {
                // SAFETY: killpg only sends a signal to the group this
                // function created.
                unsafe {
                    libc::killpg(g, libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            None
        }
    };
    // Readers end when every writer in the group is gone; a stray process
    // that keeps a pipe open cannot hold the tick for long.
    for r in readers {
        let _ = tokio::time::timeout(Duration::from_secs(5), r).await;
    }
    let bytes: Vec<u8> = tail.lock().unwrap().buf.iter().copied().collect();
    let text = String::from_utf8_lossy(&bytes);
    let chars: Vec<char> = text.chars().collect();
    let kept: String = chars[chars.len().saturating_sub(4_000)..].iter().collect();
    Ok(match status {
        None => TestRun { status: "timeout", output: format!("stopped after {} s\n{kept}", timeout.as_secs()) },
        Some(s) => TestRun { status: if s.success() { "passed" } else { "failed" }, output: kept },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn out(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
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
        let remote = Remote { url: root.join("remote.git").to_string_lossy().into_owned(), gh: None };
        (d, root, remote)
    }

    const LIMITS: ImportLimits = ImportLimits { max_files: 1000, max_file_bytes: 1 << 20, max_total_bytes: 8 << 20, max_ignore_bytes: 1 << 16 };

    fn object_files(mirror: &Path) -> usize {
        walk_files(&mirror.join("objects")).unwrap().len()
    }

    fn spec() -> CommitSpec {
        CommitSpec { author_name: "Pipeline".into(), author_email: "pipeline@example.invalid".into(), message: "Implement #1\n\nNucleus-Item: 1".into() }
    }

    fn remote_branches(root: &Path) -> String {
        out(&root.join("remote.git"), &["branch", "--list"])
    }

    #[tokio::test]
    async fn mirror_clone_import_push_cycle() {
        let (_d, root, remote) = fixture();
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        assert_eq!(default_branch(&mirror, &remote, None).await.unwrap(), "main");
        assert_eq!(default_branch(&mirror, &remote, Some("develop")).await.unwrap(), "develop");
        let wt = worktree_path(&work, "acme/widget", 1);
        let (base, restored) = prepare_clone(&mirror, &wt, "main", 1, Some("nucleus/item-1")).await.unwrap();
        assert!(!restored && wt.join("a.txt").exists());
        // Nothing changed: no commit.
        assert_eq!(import(&mirror, &remote, &wt, &base, 1, &spec(), &LIMITS).await.unwrap(), None);
        // The agent commits once and leaves changes uncommitted, a symlink to
        // a file outside the tree, and an ignored file.
        sh(
            &wt,
            "echo two > a.txt && git -c user.email=a@example.invalid -c user.name=A commit -qam agent \
             && echo new > b.txt && ln -s /etc/hosts link && echo x > skip.log && echo '*.log' > .gitignore",
        );
        let sha = import(&mirror, &remote, &wt, &base, 1, &spec(), &LIMITS).await.unwrap().unwrap();
        let mut files = changed_files(&mirror, &base, &sha).await.unwrap();
        files.sort();
        assert_eq!(files, [".gitignore", "a.txt", "b.txt", "link"]);
        let modes = out(&mirror, &["--git-dir=.", "ls-tree", &sha, "link"]);
        assert!(modes.starts_with("120000 "), "the symlink is stored as a symlink: {modes}");
        assert_eq!(added_text(&mirror, &base, &sha, 10).await.unwrap(), None, "a diff over the limit is not returned cut");
        let added = added_text(&mirror, &base, &sha, 1 << 20).await.unwrap().unwrap();
        assert!(added.contains("file: a.txt\ntwo\n") && added.contains("/etc/hosts"), "{added}");
        // One commit on the base, with the fixed identity and message.
        assert_eq!(out(&mirror, &["--git-dir=.", "rev-parse", &format!("{sha}^")]).trim(), base);
        let header = commit_header(&mirror, &sha).await.unwrap();
        assert!(header.starts_with("Pipeline <pipeline@example.invalid>\nPipeline <pipeline@example.invalid>\nImplement #1"), "{header}");
        push(&mirror, &remote, &sha, "nucleus/item-1", 1, "main", None).await.unwrap();
        assert!(remote_branches(&root).contains("nucleus/item-1"));
        // A lost clone comes back on the branch with the imported work.
        let (_, restored) = prepare_clone(&mirror, &wt, "main", 1, Some("nucleus/item-1")).await.unwrap();
        assert!(restored);
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "two\n");
        remove_clone(&wt).unwrap();
        assert!(!wt.exists());
    }

    #[tokio::test]
    async fn the_agent_clone_is_never_run_as_a_repository() {
        let (_d, root, remote) = fixture();
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 2);
        let (base, _) = prepare_clone(&mirror, &wt, "main", 2, Some("nucleus/item-2")).await.unwrap();
        let marker = root.join("hook-ran");
        let hook = format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display());
        sh(&root, "git init -q --bare decoy.git");
        let decoy = root.join("decoy.git");
        // Hooks, hooksPath, fsmonitor, a clean filter for every file,
        // upload-pack settings and rewritten remotes, in the agent clone's
        // .git and in the mirror.
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
                     && git --git-dir='{g}' config filter.x.clean 'touch {m}; cat' && git --git-dir='{g}' config filter.x.required true \
                     && git --git-dir='{g}' config diff.x.command 'touch {m}' && git --git-dir='{g}' config uploadpack.packObjectsHook 'touch {m};' \
                     && git --git-dir='{g}' config remote.origin.url '{d}' && git --git-dir='{g}' config remote.origin.pushurl '{d}' \
                     && git --git-dir='{g}' config url.'{d}'.insteadOf '{r}'",
                    g = gitdir.display(),
                    h = gitdir.join("hooks/pre-push").display(),
                    m = marker.display(),
                    d = decoy.display(),
                    r = remote.url,
                ),
            );
        }
        std::fs::write(wt.join(".gitattributes"), "* filter=x diff=x merge=x\n").unwrap();
        std::fs::write(wt.join("a.txt"), "changed\n").unwrap();
        let sha = import(&mirror, &remote, &wt, &base, 2, &spec(), &LIMITS).await.unwrap().unwrap();
        push(&mirror, &remote, &sha, "nucleus/item-2", 2, "main", None).await.unwrap();
        assert!(!marker.exists(), "no hook, fsmonitor, filter or driver ran");
        assert!(remote_branches(&root).contains("nucleus/item-2"), "pushed to the configured remote");
        assert!(out(&decoy, &["for-each-ref"]).is_empty(), "nothing reached the rewritten origin");
        // The imported file is the work tree's bytes (no filter applied).
        assert_eq!(out(&mirror, &["--git-dir=.", "show", &format!("{sha}:a.txt")]), "changed\n");
        let cfg = std::fs::read_to_string(mirror.join("config")).unwrap();
        assert!(!cfg.contains("hooksPath") && !cfg.contains("insteadOf") && !cfg.contains("filter") && cfg.contains(&remote.url), "{cfg}");
        assert!(!mirror.join("hooks").exists());
    }

    #[tokio::test]
    async fn nested_repositories_and_a_replaced_clone_are_refused() {
        let (_d, root, remote) = fixture();
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 3);
        let (base, _) = prepare_clone(&mirror, &wt, "main", 3, Some("nucleus/item-3")).await.unwrap();
        sh(&wt, "mkdir sub && cd sub && git init -q && echo z > z && git add z && git -c user.email=a@example.invalid -c user.name=A commit -qm z");
        let e = import(&mirror, &remote, &wt, &base, 3, &spec(), &LIMITS).await.unwrap_err();
        assert!(format!("{e:#}").contains("nested repository"), "{e:#}");
        // The clone directory replaced by a symlink to elsewhere.
        std::fs::remove_dir_all(&wt).unwrap();
        std::os::unix::fs::symlink(root.join("seed"), &wt).unwrap();
        let e = import(&mirror, &remote, &wt, &base, 3, &spec(), &LIMITS).await.unwrap_err();
        assert!(format!("{e:#}").contains("not a directory"), "{e:#}");
        // A .git file at the top of the tree is never read or published.
        std::fs::remove_file(&wt).unwrap();
        let (base, _) = prepare_clone(&mirror, &wt, "main", 3, Some("nucleus/item-3")).await.unwrap();
        std::fs::remove_dir_all(wt.join(".git")).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", root.join("seed/.git").display())).unwrap();
        std::fs::write(wt.join("a.txt"), "three\n").unwrap();
        let sha = import(&mirror, &remote, &wt, &base, 3, &spec(), &LIMITS).await.unwrap().unwrap();
        assert_eq!(changed_files(&mirror, &base, &sha).await.unwrap(), ["a.txt"]);
    }

    #[tokio::test]
    async fn oversized_trees_and_special_files_are_refused_before_git_add() {
        let (_d, root, remote) = fixture();
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 6);
        let (base, _) = prepare_clone(&mirror, &wt, "main", 6, Some("nucleus/item-6")).await.unwrap();
        let before = object_files(&mirror);
        let refused = |e: anyhow::Error| e.downcast_ref::<ImportRefused>().map(|r| r.0.clone()).unwrap_or_else(|| format!("not a refusal: {e:#}"));
        // A file over the per-file limit.
        std::fs::write(wt.join("big.bin"), vec![b'x'; (LIMITS.max_file_bytes + 1) as usize]).unwrap();
        let e = refused(import(&mirror, &remote, &wt, &base, 6, &spec(), &LIMITS).await.unwrap_err());
        assert!(e.contains("big.bin") && e.contains("per-file limit"), "{e}");
        std::fs::remove_file(wt.join("big.bin")).unwrap();
        // A sparse file: small on disk, long by its length.
        let f = std::fs::File::create(wt.join("sparse.bin")).unwrap();
        f.set_len(1 << 40).unwrap();
        let e = refused(import(&mirror, &remote, &wt, &base, 6, &spec(), &LIMITS).await.unwrap_err());
        assert!(e.contains("sparse.bin") && e.contains("per-file limit"), "{e}");
        std::fs::remove_file(wt.join("sparse.bin")).unwrap();
        // Too many files.
        std::fs::create_dir_all(wt.join("many")).unwrap();
        for i in 0..20 {
            std::fs::write(wt.join(format!("many/{i}.txt")), "x").unwrap();
        }
        let few = ImportLimits { max_files: 10, ..LIMITS };
        let e = refused(import(&mirror, &remote, &wt, &base, 6, &spec(), &few).await.unwrap_err());
        assert!(e.contains("more than") && e.contains("10"), "{e}");
        std::fs::remove_dir_all(wt.join("many")).unwrap();
        // A FIFO.
        sh(&wt, "mkfifo pipe");
        let e = refused(import(&mirror, &remote, &wt, &base, 6, &spec(), &LIMITS).await.unwrap_err());
        assert!(e.contains("pipe") && e.contains("FIFO"), "{e}");
        std::fs::remove_file(wt.join("pipe")).unwrap();
        // The total limit.
        for i in 0..3 {
            std::fs::write(wt.join(format!("part{i}.bin")), vec![b'y'; 700 * 1024]).unwrap();
        }
        let small_total = ImportLimits { max_total_bytes: 1 << 20, ..LIMITS };
        let e = refused(import(&mirror, &remote, &wt, &base, 6, &spec(), &small_total).await.unwrap_err());
        assert!(e.contains("total limit"), "{e}");
        // Nothing was written into the mirror, and no scratch directory is left.
        assert_eq!(object_files(&mirror), before);
        assert!(std::fs::read_dir(&mirror).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with("nucleus-import")));
        // Within the limits the import works and moves its objects in.
        let sha = import(&mirror, &remote, &wt, &base, 6, &spec(), &LIMITS).await.unwrap().unwrap();
        assert!(object_files(&mirror) > before);
        assert_eq!(out(&mirror, &["--git-dir=.", "cat-file", "-t", &sha]).trim(), "commit");
    }

    #[tokio::test]
    async fn the_snapshot_refuses_hard_links_and_never_follows_symlinks() {
        let (_d, root, remote) = fixture();
        let seed = root.join("seed");
        sh(&seed, "mkdir dir && echo f > dir/f.txt && git add dir && git commit -qm dir && git push -q origin HEAD:main");
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 8);
        let (base, _) = prepare_clone(&mirror, &wt, "main", 8, Some("nucleus/item-8")).await.unwrap();
        // A hard link (to a file outside the tree).
        std::fs::write(root.join("outside.txt"), "secret\n").unwrap();
        std::fs::hard_link(root.join("outside.txt"), wt.join("linked.txt")).unwrap();
        let e = import(&mirror, &remote, &wt, &base, 8, &spec(), &LIMITS).await.unwrap_err();
        assert!(e.downcast_ref::<ImportRefused>().unwrap().0.contains("linked.txt has more than one hard link"), "{e:#}");
        std::fs::remove_file(wt.join("linked.txt")).unwrap();
        // A tracked directory replaced by a symlink to elsewhere: stored as a
        // symlink, never followed (nothing from the target is read).
        std::fs::remove_dir_all(wt.join("dir")).unwrap();
        std::os::unix::fs::symlink(&seed, wt.join("dir")).unwrap();
        let sha = import(&mirror, &remote, &wt, &base, 8, &spec(), &LIMITS).await.unwrap().unwrap();
        assert!(out(&mirror, &["--git-dir=.", "ls-tree", &sha, "dir"]).starts_with("120000 "));
        let all = out(&mirror, &["--git-dir=.", "ls-tree", "-r", "--name-only", &sha]);
        assert!(!all.contains("dir/"), "{all}");
    }

    #[tokio::test]
    async fn a_huge_sparse_gitignore_is_refused_without_being_parsed() {
        let (_d, root, remote) = fixture();
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 12);
        let (base, _) = prepare_clone(&mirror, &wt, "main", 12, Some("nucleus/item-12")).await.unwrap();
        std::fs::create_dir_all(wt.join("deep")).unwrap();
        std::fs::File::create(wt.join("deep/.gitignore")).unwrap().set_len(4 << 30).unwrap();
        let started = std::time::Instant::now();
        let e = import(&mirror, &remote, &wt, &base, 12, &spec(), &LIMITS).await.unwrap_err();
        let why = e.downcast_ref::<ImportRefused>().map(|r| r.0.clone()).unwrap_or_else(|| format!("{e:#}"));
        assert!(why.contains("deep/.gitignore") && why.contains("per-file limit"), "{why}");
        assert!(started.elapsed() < Duration::from_secs(10), "refused quickly: {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn ignore_rules_match_git_and_tracked_files_stay() {
        let (_d, root, remote) = fixture();
        let seed = root.join("seed");
        // The base tracks vendor.log although *.log is ignored.
        sh(&seed, "printf '*.log\\n!keep.log\\nbuild/\\n' > .gitignore && echo v1 > vendor.log && git add .gitignore && git add -f vendor.log && git commit -qm rules && git push -q origin HEAD:main");
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 13);
        let (base, _) = prepare_clone(&mirror, &wt, "main", 13, Some("nucleus/item-13")).await.unwrap();
        sh(
            &wt,
            "echo a > a.log && echo k > keep.log && mkdir -p sub build/inner && printf 'secret.txt\\n' > sub/.gitignore \
             && echo s > sub/secret.txt && echo o > sub/ok.txt && echo b > build/out.bin && mkfifo build/inner/pipe \
             && echo v2 > vendor.log",
        );
        // What git itself would add, for comparison.
        let git_view = std::process::Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard"])
            .current_dir(&wt)
            .output()
            .unwrap();
        let mut expected: Vec<String> = String::from_utf8_lossy(&git_view.stdout).lines().map(str::to_string).collect();
        expected.push("vendor.log".into());
        expected.sort();
        let sha = import(&mirror, &remote, &wt, &base, 13, &spec(), &LIMITS).await.unwrap().unwrap();
        let mut changed = changed_files(&mirror, &base, &sha).await.unwrap();
        changed.sort();
        assert_eq!(changed, expected, "the same files git would add, plus the tracked vendor.log");
        assert_eq!(changed, ["keep.log", "sub/.gitignore", "sub/ok.txt", "vendor.log"]);
        assert_eq!(out(&mirror, &["--git-dir=.", "show", &format!("{sha}:vendor.log")]), "v2\n", "a tracked file matching a rule is imported");
    }

    #[tokio::test]
    async fn gitmodules_must_stay_as_in_the_base() {
        let (_d, root, remote) = fixture();
        let seed = root.join("seed");
        sh(&seed, "printf '[submodule \"lib\"]\\n\\turl = https://example.invalid/lib.git\\n' > .gitmodules && git add .gitmodules && git commit -qm m && git push -q origin HEAD:main");
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 9);
        let (base, _) = prepare_clone(&mirror, &wt, "main", 9, Some("nucleus/item-9")).await.unwrap();
        std::fs::write(wt.join(".gitmodules"), "[submodule \"lib\"]\n\turl = https://example.invalid/other.git\n").unwrap();
        let e = import(&mirror, &remote, &wt, &base, 9, &spec(), &LIMITS).await.unwrap_err();
        assert!(format!("{e:#}").contains("modifies .gitmodules"), "{e:#}");
        // A new .gitmodules in a repo without one is refused too.
        let (_d2, root2, remote2) = fixture();
        let work2 = root2.join("work");
        let mirror2 = sync_mirror(&work2, "acme/widget", &remote2).await.unwrap();
        let wt2 = worktree_path(&work2, "acme/widget", 9);
        let (base2, _) = prepare_clone(&mirror2, &wt2, "main", 9, Some("nucleus/item-9")).await.unwrap();
        std::fs::write(wt2.join(".gitmodules"), "x").unwrap();
        assert!(import(&mirror2, &remote2, &wt2, &base2, 9, &spec(), &LIMITS).await.is_err());
    }

    #[tokio::test]
    async fn objects_arrive_as_one_pack_and_old_scratch_is_swept() {
        let (_d, root, remote) = fixture();
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 10);
        let (base, _) = prepare_clone(&mirror, &wt, "main", 10, Some("nucleus/item-10")).await.unwrap();
        let loose_before = loose_ids(&mirror.join("objects")).unwrap().len();
        std::fs::write(wt.join("new.txt"), "new\n").unwrap();
        let sha = import(&mirror, &remote, &wt, &base, 10, &spec(), &LIMITS).await.unwrap().unwrap();
        assert_eq!(loose_ids(&mirror.join("objects")).unwrap().len(), loose_before, "no loose object is added");
        let packs: Vec<String> = std::fs::read_dir(mirror.join("objects/pack")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(packs.iter().any(|p| p.ends_with(".pack")) && packs.iter().any(|p| p.ends_with(".idx")), "{packs:?}");
        assert_eq!(out(&mirror, &["--git-dir=.", "cat-file", "-t", &sha]).trim(), "commit");
        // Scratch left by a stopped import: an old one is removed on open, a
        // recent one is kept.
        let old = mirror.join(format!("{SCRATCH_PREFIX}1-old"));
        let fresh = mirror.join(format!("{SCRATCH_PREFIX}1-fresh"));
        std::fs::create_dir_all(old.join("objects")).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        sh(&mirror, &format!("touch -t 200001010000 '{}'", old.display()));
        open_mirror(&work, "acme/widget", &remote).await.unwrap();
        assert!(!old.exists() && fresh.exists());
    }

    #[tokio::test]
    async fn non_utf8_file_names_survive_an_import() {
        use std::os::unix::ffi::OsStrExt;
        let (_d, root, remote) = fixture();
        let seed = root.join("seed");
        // Listing keeps bytes whatever the file system allows.
        let listed = b"100644 blob 0123\ta.txt\0100644 blob 4567\tdir/caf\xe9.txt\0";
        let t = BaseTree::parse(listed);
        assert!(t.tracked.contains(&b"dir/caf\xe9.txt"[..].to_vec()) && t.dirs.contains(&b"dir"[..].to_vec()));
        assert_eq!(crate::intake::snapshot::dest_of(Path::new("/s"), b"caf\xe9.txt").as_os_str().as_bytes(), b"/s/caf\xe9.txt");
        let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
        if let Err(e) = std::fs::write(seed.join(name), "latin-1 name\n") {
            // APFS refuses names that are not UTF-8 (EILSEQ): such a file
            // cannot exist in a clone there.
            eprintln!("skipping the on-disk part: {e}");
            return;
        }
        sh(&seed, "git add -A && git commit -qm latin1 && git push -q origin HEAD:main");
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 11);
        let (base, _) = prepare_clone(&mirror, &wt, "main", 11, Some("nucleus/item-11")).await.unwrap();
        assert!(wt.join(name).exists());
        std::fs::write(wt.join("a.txt"), "ascii edit\n").unwrap();
        let sha = import(&mirror, &remote, &wt, &base, 11, &spec(), &LIMITS).await.unwrap().unwrap();
        assert_eq!(changed_files(&mirror, &base, &sha).await.unwrap(), ["a.txt"], "the non-UTF-8 file is unchanged");
        let tree = std::process::Command::new("git").args(["--git-dir=.", "ls-tree", "-z", "--name-only", &sha]).current_dir(&mirror).output().unwrap();
        assert!(tree.stdout.split(|b| *b == 0).any(|n| n == b"caf\xe9.txt"));
    }

    #[tokio::test]
    async fn submodules_of_the_base_may_stay_but_not_change() {
        let (_d, root, remote) = fixture();
        // The base gets a submodule entry (a gitlink to some commit).
        let seed = root.join("seed");
        let head = out(&seed, &["rev-parse", "HEAD"]);
        sh(
            &seed,
            &format!(
                "printf '[submodule \"lib\"]\\n\\tpath = lib\\n\\turl = https://example.invalid/lib.git\\n' > .gitmodules \
                 && git add .gitmodules && git update-index --add --cacheinfo 160000,{},lib \
                 && git commit -qm submodule && git push -q origin HEAD:main",
                head.trim()
            ),
        );
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 7);
        let (base, _) = prepare_clone(&mirror, &wt, "main", 7, Some("nucleus/item-7")).await.unwrap();
        // An ordinary change keeps the unchanged gitlink.
        std::fs::write(wt.join("a.txt"), "changed\n").unwrap();
        let sha = import(&mirror, &remote, &wt, &base, 7, &spec(), &LIMITS).await.unwrap().unwrap();
        assert!(out(&mirror, &["--git-dir=.", "ls-tree", &sha, "lib"]).starts_with("160000 "));
        // Deleting the submodule is refused.
        std::fs::remove_dir_all(wt.join("lib")).unwrap();
        let e = import(&mirror, &remote, &wt, &base, 7, &spec(), &LIMITS).await.unwrap_err();
        assert!(format!("{e:#}").contains("deletes the submodule lib"), "{e:#}");
        std::fs::create_dir_all(wt.join("lib")).unwrap();
        // Moving it to another commit is refused.
        sh(&wt.join("lib"), "git init -q && git -c user.email=a@example.invalid -c user.name=A commit -q --allow-empty -m other");
        let e = import(&mirror, &remote, &wt, &base, 7, &spec(), &LIMITS).await.unwrap_err();
        assert!(format!("{e:#}").contains("touches the submodule lib"), "{e:#}");
        std::fs::remove_dir_all(wt.join("lib")).unwrap();
        std::fs::create_dir_all(wt.join("lib")).unwrap();
        // Adding one is refused.
        sh(&wt, "mkdir vendor && cd vendor && git init -q && git -c user.email=a@example.invalid -c user.name=A commit -q --allow-empty -m v");
        let e = import(&mirror, &remote, &wt, &base, 7, &spec(), &LIMITS).await.unwrap_err();
        assert!(format!("{e:#}").contains("nested repository at vendor"), "{e:#}");
    }

    #[test]
    fn added_lines_starting_with_plus_plus_are_kept() {
        let diff = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1 +1,2 @@\n-old\n+++HIDDEN\n+normal\n\
                    diff --git a/y b/y\nnew file mode 100644\n--- /dev/null\n+++ b/y\n@@ -0,0 +1 @@\n+--- a/fake\n";
        let out = diff_additions(diff);
        assert!(out.contains("file: x\n") && out.contains("file: y\n"), "{out}");
        assert!(out.contains("\n++HIDDEN\n") && out.contains("\nnormal\n"), "{out}");
        assert!(out.contains("--- a/fake"), "an added line that looks like a header is an added line: {out}");
        assert!(!out.contains("old"), "{out}");
    }

    #[test]
    fn push_targets_names_and_remotes() {
        assert!(check_push_target("nucleus/item-4", 4, "main").is_ok());
        assert!(check_push_target("nucleus/item-4-fix-typo", 4, "main").is_ok());
        assert!(check_push_target("nucleus/item-40", 4, "main").is_err(), "another item's branch");
        assert!(check_push_target("main", 4, "main").is_err());
        assert!(check_push_target("nucleus/item-4", 4, "nucleus/item-4").is_err(), "the default branch");
        assert!(check_push_target("feature/x", 4, "main").is_err());
        assert!(check_push_target("nucleus/item-4-..", 4, "main").is_err());
        assert!(check_branch_name("-x").is_err());
        let sh_bin = resolve_bin("sh").unwrap();
        assert!(sh_bin.is_absolute());
        let pin = Pin::new(&sh_bin).unwrap();
        let r = Remote::for_repo("https://github.com/{repo}.git", "acme/widget", Some(&pin)).unwrap();
        assert_eq!(r.gh.as_ref().map(|p| p.path.clone()), Some(sh_bin.canonicalize().unwrap()));
        assert!(Remote::for_repo("https://github.com/{repo}.git", "acme/widget", None).is_err(), "HTTPS needs a pinned gh");
        assert!(Remote::for_repo("https://github.com/{repo}.git", "acme/../x", Some(&pin)).is_err());
        let scp_style = format!("{}@{}:{{repo}}.git", "git", "github.com");
        assert!(Remote::for_repo(&scp_style, "acme/widget", Some(&pin)).is_err(), "no SSH");
        assert!(Remote::for_repo("ssh://github.com/{repo}.git", "acme/widget", Some(&pin)).is_err());
        assert!(Remote::for_repo("ext::sh -c touch% /tmp/x", "acme/widget", Some(&pin)).is_err());
        assert_eq!(item_ref(7), "refs/nucleus/item-7");
    }

    #[tokio::test]
    async fn the_remote_default_branch_is_read_live() {
        let (_d, root, remote) = fixture();
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let sha = mirror_ok(&mirror, &["rev-parse", "refs/heads/main"]).await.unwrap();
        assert!(push(&mirror, &remote, &sha, "main", 1, "main", None).await.is_err());
        sh(&root.join("remote.git"), "git branch nucleus/item-1 main && git symbolic-ref HEAD refs/heads/nucleus/item-1");
        assert_eq!(remote_head(&mirror, &remote).await.unwrap(), "nucleus/item-1");
        let e = push(&mirror, &remote, &sha, "nucleus/item-1", 1, "nucleus/item-1", None).await.unwrap_err();
        assert!(format!("{e:#}").contains("protected"), "{e:#}");
    }

    #[tokio::test]
    async fn pushes_are_leased_on_the_exact_ref() {
        let (_d, root, remote) = fixture();
        let work = root.join("work");
        let mirror = sync_mirror(&work, "acme/widget", &remote).await.unwrap();
        let wt = worktree_path(&work, "acme/widget", 5);
        let (base, _) = prepare_clone(&mirror, &wt, "main", 5, Some("nucleus/item-5")).await.unwrap();
        std::fs::write(wt.join("a.txt"), "first\n").unwrap();
        let first = import(&mirror, &remote, &wt, &base, 5, &spec(), &LIMITS).await.unwrap().unwrap();
        // The branch already exists on the remote (someone else's): the
        // first push, create-only, fails.
        sh(&root.join("remote.git"), "git branch nucleus/item-5 main");
        let e = push(&mirror, &remote, &first, "nucleus/item-5", 5, "main", None).await.unwrap_err();
        assert!(format!("{e:#}").contains("pushing nucleus/item-5 failed"), "{e:#}");
        sh(&root.join("remote.git"), "git branch -D nucleus/item-5");
        push(&mirror, &remote, &first, "nucleus/item-5", 5, "main", None).await.unwrap();
        // A later push (a new commit on the base, not a fast-forward) leases
        // on the commit pushed last.
        std::fs::write(wt.join("a.txt"), "second\n").unwrap();
        let second = import(&mirror, &remote, &wt, &base, 5, &spec(), &LIMITS).await.unwrap().unwrap();
        push(&mirror, &remote, &second, "nucleus/item-5", 5, "main", Some(&first)).await.unwrap();
        assert_eq!(out(&root.join("remote.git"), &["rev-parse", "nucleus/item-5"]).trim(), second);
        // The branch moved to another commit: the next push fails closed.
        sh(&root.join("remote.git"), "git update-ref refs/heads/nucleus/item-5 refs/heads/main");
        std::fs::write(wt.join("a.txt"), "third\n").unwrap();
        let third = import(&mirror, &remote, &wt, &base, 5, &spec(), &LIMITS).await.unwrap().unwrap();
        assert!(push(&mirror, &remote, &third, "nucleus/item-5", 5, "main", Some(&second)).await.is_err());
        assert_ne!(out(&root.join("remote.git"), &["rev-parse", "nucleus/item-5"]).trim(), third);
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
        // Endless output: stopped at the timeout, the tail kept, memory
        // bounded by the tail buffer.
        let started = std::time::Instant::now();
        let r = run_tests(d.path(), Some("while :; do echo endless-line; done"), Duration::from_millis(800)).await.unwrap();
        assert_eq!(r.status, "timeout");
        assert!(r.output.contains("endless-line") && r.output.len() < 5_000, "{}", r.output.len());
        assert!(started.elapsed() < Duration::from_secs(10));
        // The whole process group is killed, not only the shell.
        let pidfile = d.path().join("pid");
        let r = run_tests(d.path(), Some(&format!("sleep 30 & echo $! > {}; wait", pidfile.display())), Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(r.status, "timeout");
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "the background sleep was killed with its group");
        let mut t = Tail { buf: Default::default(), cap: 8 };
        t.push(b"0123456789");
        t.push(b"ab");
        assert_eq!(t.buf.iter().copied().collect::<Vec<_>>(), b"456789ab");
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
