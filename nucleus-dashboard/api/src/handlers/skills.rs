//! Skills surface — lists and manages the skill trees Claude Code loads
//! (ADR-008, ADR-032). Discovery, frontmatter parsing, tiers, and collision
//! detection live in `nucleus_core::skills` (shared with the skill-gap
//! learner, ADR-017) so the dashboard and the learner read skills
//! identically.
//!
//! Read:
//!   - `GET  /skills/api/list`  → `SkillLibrary` (personal, repo, global,
//!     archived)
//!   - `GET  /skills/api/body?path=<Skill.path>` → raw SKILL.md text
//!
//! Write (JSON in, `SkillActionResp` out; refusals are `SkillsErrorBody`):
//!   - `POST /skills/api/move    { dir_name, from: "personal" | "global" }`
//!   - `POST /skills/api/archive { dir_name, tier: "personal" | "global" }`
//!   - `POST /skills/api/restore { dir_name, tier: "personal-archive" | "global-archive" }`
//!   - `POST /skills/api/delete  { dir_name, tier: "personal-archive" | "global-archive" }`
//!
//! The repo tier is read-only here: changing it is a git change to the
//! public repository, not a dashboard action.
//!
//! Write protection matches the other mutating surfaces (reminders, chat):
//! the server binds to loopback behind the tailnet perimeter (ADR-011), and
//! every write takes a JSON body. Axum's `Json` extractor rejects any other
//! content type, so a cross-site page cannot send one without a CORS
//! preflight, and this server grants no CORS.
//!
//! Every write is serialized by one lock, re-reads the library, and
//! re-validates paths against the specific root (canonicalized). Writes
//! never follow a symlink: a symlinked skill cannot be moved or archived,
//! and deleting an archived symlink removes only the link.

use axum::{
    extract::{rejection::JsonRejection, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use nucleus_core::skills::{
    private_dir, read_library, validate_dir_name, LibraryRoots, Skill, SkillLibrary, SkillTier,
    SKILL_FILE,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Note file a manual archive may leave in the skill directory. Restore
/// deletes it: it describes the archiving, which no longer applies.
const ARCHIVED_NOTE: &str = "ARCHIVED.md";

pub struct SkillsState {
    pub roots: LibraryRoots,
    /// `<workspace>/.nucleus` — its own local git repo (ADR-032). Changes to
    /// the personal tiers are committed there.
    pub private_repo: PathBuf,
    /// `memory/reminders.db`. Writes refuse to run without it, because they
    /// cannot check which reminders reference a skill.
    pub reminders: Option<SqlitePool>,
    write_lock: tokio::sync::Mutex<()>,
}

impl SkillsState {
    pub fn new(workspace_root: &Path, reminders: Option<SqlitePool>) -> Self {
        Self::with_roots(
            LibraryRoots::for_workspace(workspace_root),
            private_dir(workspace_root),
            reminders,
        )
    }

    pub fn with_roots(roots: LibraryRoots, private_repo: PathBuf, reminders: Option<SqlitePool>) -> Self {
        Self { roots, private_repo, reminders, write_lock: tokio::sync::Mutex::new(()) }
    }
}

pub fn router(state: Arc<SkillsState>) -> Router {
    Router::new()
        .route("/list", get(list_skills))
        .route("/body", get(get_body))
        .route("/move", post(move_skill))
        .route("/archive", post(archive_skill))
        .route("/restore", post(restore_skill))
        .route("/delete", post(delete_skill))
        .with_state(state)
}

// ─── wire types ────────────────────────────────────────────────────────────

/// An active tier the dashboard can write to. `repo` is excluded: it is the
/// committed tree of the public repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ts_rs::TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum MutableTier {
    Personal,
    Global,
}

impl From<MutableTier> for SkillTier {
    fn from(t: MutableTier) -> Self {
        match t {
            MutableTier::Personal => SkillTier::Personal,
            MutableTier::Global => SkillTier::Global,
        }
    }
}

/// An archive tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ts_rs::TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum ArchiveTier {
    PersonalArchive,
    GlobalArchive,
}

impl From<ArchiveTier> for SkillTier {
    fn from(t: ArchiveTier) -> Self {
        match t {
            ArchiveTier::PersonalArchive => SkillTier::PersonalArchive,
            ArchiveTier::GlobalArchive => SkillTier::GlobalArchive,
        }
    }
}

/// `POST /skills/api/move` — move an active skill between `personal`
/// (`.nucleus/.claude/skills`) and `global` (`~/.claude/skills`). The
/// destination is the other of the two.
#[derive(Debug, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct MoveSkillReq {
    /// `Skill.dir_name` of the skill to move.
    pub dir_name: String,
    /// The skill's current tier.
    pub from: MutableTier,
}

/// `POST /skills/api/archive` — move an active skill into its tier's archive
/// (`personal` → `personal-archive`, `global` → `global-archive`).
#[derive(Debug, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct ArchiveSkillReq {
    pub dir_name: String,
    pub tier: MutableTier,
}

/// `POST /skills/api/restore` and `POST /skills/api/delete` — act on an
/// archived skill.
#[derive(Debug, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct ArchivedSkillReq {
    pub dir_name: String,
    pub tier: ArchiveTier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum SkillAction {
    Move,
    Archive,
    Restore,
    Delete,
}

/// What happened in the `.nucleus` git repository after a write.
/// Discriminated on `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(tag = "status", rename_all = "kebab-case")]
#[ts(export)]
pub enum GitOutcome {
    /// The action touched only the global tiers, which are not under git.
    NotApplicable,
    /// The change was committed in `.nucleus`; `sha` is the new commit.
    Committed { sha: String },
    /// No commit was made (`.nucleus/.git` is missing, or git saw no change).
    Skipped { reason: String },
    /// The filesystem change succeeded but the commit failed. The change is
    /// on disk and uncommitted.
    Failed { error: String },
}

/// Result of a successful write.
#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct SkillActionResp {
    pub action: SkillAction,
    /// Tier the skill was in before the action.
    pub from: SkillTier,
    /// Directory name before the action.
    pub dir_name: String,
    /// Tier after the action; null for `delete`.
    pub to: Option<SkillTier>,
    /// Directory name after the action; null for `delete`. Differs from
    /// `dir_name` when archive added a date suffix or restore used the
    /// skill's `restore_name`.
    pub new_dir_name: Option<String>,
    /// SKILL.md path after the action (usable with `/body`); null for
    /// `delete`.
    pub path: Option<String>,
    pub git: GitOutcome,
    /// Extra information for the operator, for example whether a delete can
    /// be recovered.
    pub note: Option<String>,
}

/// Which reminder column mentions the skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ReminderRefField {
    ConditionCmd,
    FallbackCmd,
    SystemPrompt,
}

/// A live reminder (status active, paused, or pending) whose command or
/// prompt refers to the skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ReminderRef {
    #[ts(type = "number")]
    pub id: i64,
    pub title: Option<String>,
    pub field: ReminderRefField,
}

/// Body of every non-2xx response from this surface.
#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct SkillsErrorBody {
    /// Human-readable reason, ready to show to the operator.
    pub error: String,
    /// Reminders that block the action (409 on move/archive); empty
    /// otherwise.
    pub reminders: Vec<ReminderRef>,
}

// ─── read ──────────────────────────────────────────────────────────────────

async fn list_skills(State(s): State<Arc<SkillsState>>) -> Result<Json<SkillLibrary>, SkillsError> {
    let roots = s.roots.clone();
    blocking(move || Ok(read_library(&roots))).await.map(Json)
}

#[derive(Deserialize)]
struct BodyQ {
    path: String,
}

/// Returns raw SKILL.md content (frontmatter + body).
async fn get_body(
    State(s): State<Arc<SkillsState>>,
    Query(q): Query<BodyQ>,
) -> Result<String, SkillsError> {
    let roots = s.roots.clone();
    blocking(move || {
        let file = resolve_body_path(&roots, &q.path)?;
        std::fs::read_to_string(&file)
            .map_err(|e| SkillsError::Io(format!("reading {}: {e}", file.display())))
    })
    .await
}

/// Map a requested `Skill.path` to the file to read, or refuse.
///
/// Accepted shape: an absolute `<root>/<dir>/SKILL.md`, no `.`/`..`
/// components, where `<root>` (after canonicalization) is one of the tier
/// roots and `<dir>` is a valid directory name. The resolved file must be
/// exactly `<canonical root>/<dir>/SKILL.md`. The one exception is a global
/// skill whose `<dir>` entry is a symlink (vendor skills): its SKILL.md is
/// read from the link target, and only the SKILL.md directly inside that
/// target. So the readable files are the SKILL.md files the listing shows,
/// never an arbitrary path.
fn resolve_body_path(roots: &LibraryRoots, requested: &str) -> Result<PathBuf, SkillsError> {
    let req = Path::new(requested);
    if !req.is_absolute()
        || req.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        || req.file_name().and_then(|n| n.to_str()) != Some(SKILL_FILE)
    {
        return Err(SkillsError::OutsideRoots);
    }
    let dir = req.parent().ok_or(SkillsError::OutsideRoots)?;
    let dir_name = dir.file_name().and_then(|n| n.to_str()).ok_or(SkillsError::OutsideRoots)?;
    validate_dir_name(dir_name).map_err(|_| SkillsError::OutsideRoots)?;
    let given_root = dir.parent().ok_or(SkillsError::OutsideRoots)?;
    let canon_given = std::fs::canonicalize(given_root).map_err(|_| SkillsError::OutsideRoots)?;

    for (tier, root) in roots.all() {
        let Ok(canon_root) = std::fs::canonicalize(root) else { continue };
        if canon_root != canon_given {
            continue;
        }
        let entry = canon_root.join(dir_name);
        let meta = std::fs::symlink_metadata(&entry).map_err(|_| SkillsError::OutsideRoots)?;
        let skill_dir = if meta.file_type().is_symlink() {
            if tier != SkillTier::Global {
                return Err(SkillsError::OutsideRoots);
            }
            std::fs::canonicalize(&entry).map_err(|_| SkillsError::OutsideRoots)?
        } else if meta.is_dir() {
            entry
        } else {
            return Err(SkillsError::OutsideRoots);
        };
        let expected = skill_dir.join(SKILL_FILE);
        let file = std::fs::canonicalize(&expected).map_err(|_| SkillsError::OutsideRoots)?;
        if file != expected {
            return Err(SkillsError::OutsideRoots); // SKILL.md itself is a symlink
        }
        return Ok(file);
    }
    Err(SkillsError::OutsideRoots)
}

// ─── write ─────────────────────────────────────────────────────────────────

async fn move_skill(
    State(s): State<Arc<SkillsState>>,
    payload: Result<Json<MoveSkillReq>, JsonRejection>,
) -> Result<Json<SkillActionResp>, SkillsError> {
    let Json(req) = payload?;
    let from = SkillTier::from(req.from);
    let dir = req.dir_name.clone();
    logged("move", &dir, from, do_move(&s, req)).await
}

async fn do_move(s: &SkillsState, req: MoveSkillReq) -> Result<SkillActionResp, SkillsError> {
    validate_dir_name(&req.dir_name).map_err(SkillsError::BadRequest)?;
    let from = SkillTier::from(req.from);
    let to = match req.from {
        MutableTier::Personal => SkillTier::Global,
        MutableTier::Global => SkillTier::Personal,
    };
    let src_root = require_root(&s.roots, from)?;
    let dst_root = require_root(&s.roots, to)?;
    let _guard = s.write_lock.lock().await;

    let lib = load_library(s).await?;
    let skill = find_skill(&lib, from, &req.dir_name, &src_root)?;
    refuse_symlink(skill)?;
    let keys = [skill.name.as_str(), skill.dir_name.as_str()];
    let holders = lib.active_tiers_named(&keys, Some((from, &skill.dir_name)));
    if !holders.is_empty() {
        return Err(SkillsError::Conflict(format!(
            "a skill named {:?} already exists in {}; moving it would create a duplicate",
            skill.name,
            tier_list(&holders)
        )));
    }
    check_reminders(s, &reminder_needles(from, &skill.dir_name, None)).await?;

    let dir = req.dir_name.clone();
    let (src, dst) = (src_root.clone(), dst_root.clone());
    blocking(move || relocate(&src, &dir, &dst, &dir)).await?;
    let git = commit_private(
        s,
        format!("dashboard: move skill {} from {from} to {to}", req.dir_name),
    )
    .await;
    Ok(SkillActionResp {
        action: SkillAction::Move,
        from,
        dir_name: req.dir_name.clone(),
        to: Some(to),
        new_dir_name: Some(req.dir_name.clone()),
        path: Some(skill_md_path(&dst_root, &req.dir_name)),
        git,
        note: None,
    })
}

async fn archive_skill(
    State(s): State<Arc<SkillsState>>,
    payload: Result<Json<ArchiveSkillReq>, JsonRejection>,
) -> Result<Json<SkillActionResp>, SkillsError> {
    let Json(req) = payload?;
    let from = SkillTier::from(req.tier);
    let dir = req.dir_name.clone();
    logged("archive", &dir, from, do_archive(&s, req)).await
}

async fn do_archive(s: &SkillsState, req: ArchiveSkillReq) -> Result<SkillActionResp, SkillsError> {
    validate_dir_name(&req.dir_name).map_err(SkillsError::BadRequest)?;
    let from = SkillTier::from(req.tier);
    let to = from.archive().expect("mutable tiers have an archive");
    let src_root = require_root(&s.roots, from)?;
    let dst_root = require_root(&s.roots, to)?;
    let _guard = s.write_lock.lock().await;

    let lib = load_library(s).await?;
    let skill = find_skill(&lib, from, &req.dir_name, &src_root)?;
    refuse_symlink(skill)?;
    if skill.pinned {
        return Err(SkillsError::Unprocessable(format!(
            "skill {:?} is pinned (`pinned: true`); unpin it before archiving",
            skill.name
        )));
    }
    check_reminders(s, &reminder_needles(from, &skill.dir_name, Some(&skill.name))).await?;

    let dir = req.dir_name.clone();
    let today = chrono::Local::now().date_naive();
    let (src, dst) = (src_root.clone(), dst_root.clone());
    let new_dir = blocking(move || {
        std::fs::create_dir_all(&dst)
            .map_err(|e| SkillsError::Io(format!("creating {}: {e}", dst.display())))?;
        let name = archive_dest_name(&dst, &dir, today);
        relocate(&src, &dir, &dst, &name)?;
        Ok(name)
    })
    .await?;
    let git = if from == SkillTier::Personal {
        commit_private(s, format!("dashboard: archive skill {} as .archive/{new_dir}", req.dir_name)).await
    } else {
        GitOutcome::NotApplicable
    };
    Ok(SkillActionResp {
        action: SkillAction::Archive,
        from,
        dir_name: req.dir_name.clone(),
        to: Some(to),
        path: Some(skill_md_path(&dst_root, &new_dir)),
        new_dir_name: Some(new_dir),
        git,
        note: None,
    })
}

async fn restore_skill(
    State(s): State<Arc<SkillsState>>,
    payload: Result<Json<ArchivedSkillReq>, JsonRejection>,
) -> Result<Json<SkillActionResp>, SkillsError> {
    let Json(req) = payload?;
    let from = SkillTier::from(req.tier);
    let dir = req.dir_name.clone();
    logged("restore", &dir, from, do_restore(&s, req)).await
}

async fn do_restore(s: &SkillsState, req: ArchivedSkillReq) -> Result<SkillActionResp, SkillsError> {
    validate_dir_name(&req.dir_name).map_err(SkillsError::BadRequest)?;
    let from = SkillTier::from(req.tier);
    let to = from.active().expect("archive tiers have an active tier");
    let src_root = require_root(&s.roots, from)?;
    let dst_root = require_root(&s.roots, to)?;
    let _guard = s.write_lock.lock().await;

    let lib = load_library(s).await?;
    let skill = find_skill(&lib, from, &req.dir_name, &src_root)?;
    refuse_symlink(skill)?;
    let restore_name = skill.restore_name.clone().unwrap_or_else(|| skill.dir_name.clone());
    validate_dir_name(&restore_name).map_err(SkillsError::Unprocessable)?;
    let keys = [restore_name.as_str(), skill.name.as_str()];
    let holders = lib.active_tiers_named(&keys, None);
    if !holders.is_empty() {
        return Err(SkillsError::Conflict(format!(
            "a skill named {restore_name:?} already exists in {}; restoring would create a duplicate",
            tier_list(&holders)
        )));
    }

    let dir = req.dir_name.clone();
    let name = restore_name.clone();
    let (src, dst) = (src_root.clone(), dst_root.clone());
    let removed_note = blocking(move || {
        let new_dir = relocate(&src, &dir, &dst, &name)?;
        let note = new_dir.join(ARCHIVED_NOTE);
        match std::fs::symlink_metadata(&note) {
            Ok(m) if !m.is_dir() => {
                std::fs::remove_file(&note)
                    .map_err(|e| SkillsError::Io(format!("removing {}: {e}", note.display())))?;
                Ok(true)
            }
            _ => Ok(false),
        }
    })
    .await?;
    let git = if to == SkillTier::Personal {
        commit_private(s, format!("dashboard: restore skill .archive/{} as {restore_name}", req.dir_name)).await
    } else {
        GitOutcome::NotApplicable
    };
    Ok(SkillActionResp {
        action: SkillAction::Restore,
        from,
        dir_name: req.dir_name.clone(),
        to: Some(to),
        path: Some(skill_md_path(&dst_root, &restore_name)),
        new_dir_name: Some(restore_name),
        git,
        note: removed_note.then(|| format!("removed {ARCHIVED_NOTE}")),
    })
}

async fn delete_skill(
    State(s): State<Arc<SkillsState>>,
    payload: Result<Json<ArchivedSkillReq>, JsonRejection>,
) -> Result<Json<SkillActionResp>, SkillsError> {
    let Json(req) = payload?;
    let from = SkillTier::from(req.tier);
    let dir = req.dir_name.clone();
    logged("delete", &dir, from, do_delete(&s, req)).await
}

async fn do_delete(s: &SkillsState, req: ArchivedSkillReq) -> Result<SkillActionResp, SkillsError> {
    validate_dir_name(&req.dir_name).map_err(SkillsError::BadRequest)?;
    let from = SkillTier::from(req.tier);
    let root = require_root(&s.roots, from)?;
    let _guard = s.write_lock.lock().await;

    let dir = req.dir_name.clone();
    let removed_link = blocking(move || {
        let canon_root = std::fs::canonicalize(&root)
            .map_err(|_| SkillsError::NotFound(format!("no archived skill {dir:?} in {from}")))?;
        let entry = canon_root.join(&dir);
        let meta = std::fs::symlink_metadata(&entry)
            .map_err(|_| SkillsError::NotFound(format!("no archived skill {dir:?} in {from}")))?;
        if meta.file_type().is_symlink() {
            // Remove the link only; never follow it.
            std::fs::remove_file(&entry)
                .map_err(|e| SkillsError::Io(format!("removing link {}: {e}", entry.display())))?;
            Ok(true)
        } else if meta.is_dir() {
            std::fs::remove_dir_all(&entry)
                .map_err(|e| SkillsError::Io(format!("removing {}: {e}", entry.display())))?;
            Ok(false)
        } else {
            Err(SkillsError::Unprocessable(format!("{dir:?} in {from} is not a directory")))
        }
    })
    .await?;

    let (git, mut note) = if from == SkillTier::PersonalArchive {
        let git = commit_private(s, format!("dashboard: delete archived skill .archive/{}", req.dir_name)).await;
        let note = match &git {
            GitOutcome::Committed { sha } => format!(
                "deleted; recoverable from the .nucleus git history (the parent of commit {sha})"
            ),
            _ => "deleted; recoverable from the .nucleus git history only if it was committed before"
                .to_string(),
        };
        (git, note)
    } else {
        (
            GitOutcome::NotApplicable,
            "deleted permanently; the global archive is not under version control".to_string(),
        )
    };
    if removed_link {
        note = format!("removed the symlink only; its target was not touched. {note}");
    }
    Ok(SkillActionResp {
        action: SkillAction::Delete,
        from,
        dir_name: req.dir_name.clone(),
        to: None,
        new_dir_name: None,
        path: None,
        git,
        note: Some(note),
    })
}

// ─── write helpers ─────────────────────────────────────────────────────────

async fn logged(
    action: &str,
    dir: &str,
    tier: SkillTier,
    fut: impl std::future::Future<Output = Result<SkillActionResp, SkillsError>>,
) -> Result<Json<SkillActionResp>, SkillsError> {
    match fut.await {
        Ok(resp) => {
            tracing::info!(
                "skills: {action} {dir:?} ({tier}) → {:?} {:?}; git: {:?}",
                resp.to,
                resp.new_dir_name,
                resp.git
            );
            Ok(Json(resp))
        }
        Err(e) => {
            tracing::warn!("skills: {action} {dir:?} ({tier}) refused: {e:?}");
            Err(e)
        }
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, SkillsError> + Send + 'static,
) -> Result<T, SkillsError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SkillsError::Io(format!("join: {e}")))?
}

async fn load_library(s: &SkillsState) -> Result<SkillLibrary, SkillsError> {
    let roots = s.roots.clone();
    blocking(move || Ok(read_library(&roots))).await
}

fn require_root(roots: &LibraryRoots, tier: SkillTier) -> Result<PathBuf, SkillsError> {
    roots.root(tier).map(Path::to_path_buf).ok_or_else(|| {
        SkillsError::Unavailable(format!("HOME is not set, so the {tier} tier is unavailable"))
    })
}

fn find_skill<'a>(
    lib: &'a SkillLibrary,
    tier: SkillTier,
    dir_name: &str,
    root: &Path,
) -> Result<&'a Skill, SkillsError> {
    lib.find(tier, dir_name).ok_or_else(|| {
        let what = if root.join(dir_name).is_dir() {
            format!("{dir_name:?} in {tier} has no {SKILL_FILE}, so it is not a skill")
        } else {
            format!("no skill {dir_name:?} in {tier}")
        };
        SkillsError::NotFound(what)
    })
}

fn refuse_symlink(skill: &Skill) -> Result<(), SkillsError> {
    match &skill.symlink_target {
        Some(target) => Err(SkillsError::Unprocessable(format!(
            "{:?} in {} is a symlink to {target}; the dashboard does not move symlinked skills",
            skill.dir_name, skill.tier
        ))),
        None => Ok(()),
    }
}

fn tier_list(tiers: &[SkillTier]) -> String {
    tiers.iter().map(|t| t.as_str()).collect::<Vec<_>>().join(", ")
}

fn skill_md_path(root: &Path, dir: &str) -> String {
    root.join(dir).join(SKILL_FILE).to_string_lossy().into_owned()
}

/// Move `<src_root>/<src_dir>` to `<dst_root>/<dst_dir>` and return the new
/// directory. Both names must already be validated. Refuses a missing
/// source, a symlinked or non-directory source, and an existing destination.
/// Uses `rename`; across filesystems it copies (symlinks inside are
/// recreated, not followed) and then removes the source.
fn relocate(src_root: &Path, src_dir: &str, dst_root: &Path, dst_dir: &str) -> Result<PathBuf, SkillsError> {
    let canon_src_root = std::fs::canonicalize(src_root)
        .map_err(|_| SkillsError::NotFound(format!("no skill {src_dir:?}: {} does not exist", src_root.display())))?;
    let src = canon_src_root.join(src_dir);
    let meta = std::fs::symlink_metadata(&src)
        .map_err(|_| SkillsError::NotFound(format!("no skill directory {src_dir:?}")))?;
    if meta.file_type().is_symlink() {
        return Err(SkillsError::Unprocessable(format!("{src_dir:?} is a symlink; not moving it")));
    }
    if !meta.is_dir() {
        return Err(SkillsError::Unprocessable(format!("{src_dir:?} is not a directory")));
    }
    std::fs::create_dir_all(dst_root)
        .map_err(|e| SkillsError::Io(format!("creating {}: {e}", dst_root.display())))?;
    let canon_dst_root = std::fs::canonicalize(dst_root)
        .map_err(|e| SkillsError::Io(format!("canonicalizing {}: {e}", dst_root.display())))?;
    let dst = canon_dst_root.join(dst_dir);
    if std::fs::symlink_metadata(&dst).is_ok() {
        return Err(SkillsError::Conflict(format!(
            "{} already exists; not overwriting it",
            dst.display()
        )));
    }
    match std::fs::rename(&src, &dst) {
        Ok(()) => Ok(dst),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            if let Err(e) = copy_tree(&src, &dst) {
                let _ = std::fs::remove_dir_all(&dst);
                return Err(SkillsError::Io(format!("copying {} to {}: {e}", src.display(), dst.display())));
            }
            std::fs::remove_dir_all(&src).map_err(|e| {
                SkillsError::Io(format!("copied to {} but removing {} failed: {e}", dst.display(), src.display()))
            })?;
            Ok(dst)
        }
        Err(e) => Err(SkillsError::Io(format!("moving {} to {}: {e}", src.display(), dst.display()))),
    }
}

/// Recursive copy that recreates symlinks instead of following them.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = std::fs::symlink_metadata(&from)?.file_type();
        if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
        } else if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Archive directory name for `dir`: `dir` itself when free, else
/// `dir-YYYY-MM-DD`, then `dir-YYYY-MM-DD-2`, `-3`, …
fn archive_dest_name(archive_root: &Path, dir: &str, today: chrono::NaiveDate) -> String {
    let taken = |name: &str| std::fs::symlink_metadata(archive_root.join(name)).is_ok();
    if !taken(dir) {
        return dir.to_string();
    }
    let dated = format!("{dir}-{}", today.format("%Y-%m-%d"));
    if !taken(&dated) {
        return dated;
    }
    (2..)
        .map(|n| format!("{dated}-{n}"))
        .find(|c| !taken(c))
        .expect("an unbounded range yields a free name")
}

// ─── reminder references ───────────────────────────────────────────────────

/// What to search reminder rows for before moving or archiving a skill.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReminderNeedles {
    /// Path fragment naming the skill directory. Reminder commands spell the
    /// root in several ways (`$NUCLEUS_WORKSPACE_ROOT/…`, an absolute path,
    /// `~/…`, `$HOME/…`); every spelling ends in this fragment.
    path: String,
    /// Skill name whose mention in a `system_prompt` (`/name` or the bare
    /// name) also counts (archive only: an archived skill no longer loads; a
    /// moved one does).
    invocation: Option<String>,
}

fn reminder_needles(tier: SkillTier, dir_name: &str, invocation: Option<&str>) -> ReminderNeedles {
    let path = match tier {
        SkillTier::Personal => format!(".nucleus/.claude/skills/{dir_name}"),
        _ => format!(".claude/skills/{dir_name}"),
    };
    ReminderNeedles { path, invocation: invocation.map(str::to_string) }
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// True when `text` contains `needle` followed by a character that cannot
/// continue a directory name (so `skills/foo` does not match `skills/foo-2`).
fn mentions_path(text: &str, needle: &str) -> bool {
    text.match_indices(needle)
        .any(|(i, _)| text[i + needle.len()..].chars().next().is_none_or(|c| !is_name_char(c)))
}

/// True when `text` names the skill as a whole word, with or without a
/// leading `/`: fire prompts invoke skills both as `/name` and by bare name
/// ("Run name FS, post the result"). The name must not be preceded or
/// followed by a name character, so `foo` matches neither `foo-2` nor
/// `my-foo`.
fn mentions_invocation(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(i, _)| {
        let before_ok = text[..i].chars().next_back().is_none_or(|c| !is_name_char(c));
        let after_ok = text[i + name.len()..].chars().next().is_none_or(|c| !is_name_char(c));
        before_ok && after_ok
    })
}

/// Live reminders (status active, paused, pending) that mention the skill.
async fn find_reminder_refs(
    pool: &SqlitePool,
    needles: &ReminderNeedles,
) -> Result<Vec<ReminderRef>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i64,
        title: Option<String>,
        condition_cmd: Option<String>,
        fallback_cmd: Option<String>,
        system_prompt: Option<String>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, title, condition_cmd, fallback_cmd, system_prompt
           FROM reminders
          WHERE status IN ('active', 'paused', 'pending')
          ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for Row { id, title, condition_cmd, fallback_cmd, system_prompt } in rows {
        let fields = [
            (ReminderRefField::ConditionCmd, condition_cmd),
            (ReminderRefField::FallbackCmd, fallback_cmd),
            (ReminderRefField::SystemPrompt, system_prompt),
        ];
        for (field, text) in fields {
            let Some(text) = text else { continue };
            let hit = mentions_path(&text, &needles.path)
                || (field == ReminderRefField::SystemPrompt
                    && needles.invocation.as_deref().is_some_and(|n| mentions_invocation(&text, n)));
            if hit {
                out.push(ReminderRef { id, title: title.clone(), field });
            }
        }
    }
    Ok(out)
}

async fn check_reminders(s: &SkillsState, needles: &ReminderNeedles) -> Result<(), SkillsError> {
    let pool = s.reminders.as_ref().ok_or_else(|| {
        SkillsError::Unavailable(
            "reminders.db is not available, so reminder references to this skill cannot be checked".into(),
        )
    })?;
    let refs = find_reminder_refs(pool, needles)
        .await
        .map_err(|e| SkillsError::Unavailable(format!("reading reminders.db: {e}")))?;
    if refs.is_empty() {
        return Ok(());
    }
    let mut ids: Vec<String> = refs.iter().map(|r| format!("#{}", r.id)).collect();
    ids.dedup();
    Err(SkillsError::ReminderRefs {
        message: format!(
            "reminder(s) {} reference this skill; update or cancel them first",
            ids.join(", ")
        ),
        refs,
    })
}

// ─── .nucleus git ──────────────────────────────────────────────────────────

async fn commit_private(s: &SkillsState, message: String) -> GitOutcome {
    let repo = s.private_repo.clone();
    tokio::task::spawn_blocking(move || commit_private_sync(&repo, &message))
        .await
        .unwrap_or_else(|e| GitOutcome::Failed { error: format!("join: {e}") })
}

/// Stage and commit everything under `.claude/skills` in the `.nucleus`
/// repo. Only that path is committed, so unrelated staged changes stay
/// staged. The identity is passed explicitly, so the commit works without a
/// git config.
fn commit_private_sync(repo: &Path, message: &str) -> GitOutcome {
    use std::process::Command;
    if !repo.join(".git").exists() {
        return GitOutcome::Skipped { reason: format!("{} is not a git repository", repo.display()) };
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.name=nucleus-dashboard",
                "-c",
                "user.email=nucleus-dashboard@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
    };
    let fail = |step: &str, out: std::io::Result<std::process::Output>| -> GitOutcome {
        let error = match out {
            Ok(o) => format!(
                "git {step} exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => format!("git {step}: {e}"),
        };
        GitOutcome::Failed { error }
    };

    let add = git(&["add", "-A", "--", ".claude/skills"]);
    if !add.as_ref().is_ok_and(|o| o.status.success()) {
        return fail("add", add);
    }
    let diff = git(&["diff", "--cached", "--quiet", "--", ".claude/skills"]);
    match diff {
        Ok(o) if o.status.success() => {
            return GitOutcome::Skipped { reason: "no changes under .claude/skills to commit".into() };
        }
        Ok(o) if o.status.code() == Some(1) => {}
        other => return fail("diff", other),
    }
    let commit = git(&["commit", "-q", "-m", message, "--", ".claude/skills"]);
    if !commit.as_ref().is_ok_and(|o| o.status.success()) {
        return fail("commit", commit);
    }
    match git(&["rev-parse", "HEAD"]) {
        Ok(o) if o.status.success() => GitOutcome::Committed {
            sha: String::from_utf8_lossy(&o.stdout).trim().to_string(),
        },
        other => fail("rev-parse", other),
    }
}

// ─── errors ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum SkillsError {
    /// 500 — unexpected filesystem failure.
    Io(String),
    /// 403 — `/body` path outside the skill roots.
    OutsideRoots,
    /// 400 — invalid request (bad directory name).
    BadRequest(String),
    /// 400/415/422 — body not parseable as the request type.
    Rejected(StatusCode, String),
    /// 404 — no such skill.
    NotFound(String),
    /// 409 — the destination or the name is taken.
    Conflict(String),
    /// 409 — live reminders reference the skill.
    ReminderRefs { message: String, refs: Vec<ReminderRef> },
    /// 422 — the skill cannot take this action (symlink, pinned, not a dir).
    Unprocessable(String),
    /// 503 — a dependency (HOME, reminders.db) is missing.
    Unavailable(String),
}

impl From<JsonRejection> for SkillsError {
    fn from(r: JsonRejection) -> Self {
        Self::Rejected(r.status(), r.body_text())
    }
}

impl IntoResponse for SkillsError {
    fn into_response(self) -> axum::response::Response {
        let mut reminders = Vec::new();
        let (code, error) = match self {
            Self::Io(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
            Self::OutsideRoots => (
                StatusCode::FORBIDDEN,
                "path is not a SKILL.md inside a skills tree".to_string(),
            ),
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Self::Rejected(code, m) => (code, m),
            Self::NotFound(m) => (StatusCode::NOT_FOUND, m),
            Self::Conflict(m) => (StatusCode::CONFLICT, m),
            Self::ReminderRefs { message, refs } => {
                reminders = refs;
                (StatusCode::CONFLICT, message)
            }
            Self::Unprocessable(m) => (StatusCode::UNPROCESSABLE_ENTITY, m),
            Self::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
        };
        (code, Json(SkillsErrorBody { error, reminders })).into_response()
    }
}

#[cfg(test)]
mod tests;
