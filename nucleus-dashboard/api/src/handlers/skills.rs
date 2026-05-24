//! Skills surface — walk the operator-personal and repo-committed skill
//! directories and expose them as JSON. Discovery + frontmatter parsing live
//! in `nucleus_core::skills` (shared with the skill-gap learner, ADR-017) so
//! the dashboard and the learner read skills identically.
//!
//! Per ADR-008 storage convention:
//!   - `~/.claude/skills/<name>/SKILL.md`   — operator-personal (not committed)
//!   - `<repo>/.claude/skills/<name>/SKILL.md` — committed (ships with repo)

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use nucleus_core::skills::{read_skills, Skill, SKILL_FILE};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct SkillsState {
    pub operator_root: PathBuf,
    pub repo_root: PathBuf,
}

pub fn router(state: Arc<SkillsState>) -> Router {
    Router::new()
        .route("/list", get(list_skills))
        .route("/body", get(get_body))
        .with_state(state)
}

async fn list_skills(State(s): State<Arc<SkillsState>>) -> Result<Json<Vec<Skill>>, SkillsError> {
    let operator = s.operator_root.clone();
    let repo = s.repo_root.clone();
    // read_skills is sync fs; keep it off the async executor.
    let mut out = tokio::task::spawn_blocking(move || {
        let mut v = read_skills(&operator, "personal");
        v.extend(read_skills(&repo, "repo"));
        v
    })
    .await
    .map_err(|e| SkillsError::Io(format!("join: {e}")))?;
    // Stable order: tier first, then name.
    out.sort_by(|a, b| a.tier.cmp(&b.tier).then_with(|| a.name.cmp(&b.name)));
    Ok(Json(out))
}

#[derive(Deserialize)]
struct BodyQ {
    path: String,
}

/// Returns raw SKILL.md content (frontmatter + body). Path-traversal guarded
/// by requiring the canonicalized path to sit inside one of the two roots.
async fn get_body(
    State(s): State<Arc<SkillsState>>,
    Query(q): Query<BodyQ>,
) -> Result<String, SkillsError> {
    let requested = PathBuf::from(&q.path);
    let canonical = tokio::fs::canonicalize(&requested)
        .await
        .map_err(|e| SkillsError::Io(format!("canonicalizing {}: {}", q.path, e)))?;
    let in_operator = canonical_under(&canonical, &s.operator_root).await;
    let in_repo = canonical_under(&canonical, &s.repo_root).await;
    if !in_operator && !in_repo {
        return Err(SkillsError::OutsideRoots);
    }
    if canonical.file_name().and_then(|n| n.to_str()) != Some(SKILL_FILE) {
        return Err(SkillsError::OutsideRoots);
    }
    tokio::fs::read_to_string(&canonical)
        .await
        .map_err(|e| SkillsError::Io(format!("reading {}: {}", canonical.display(), e)))
}

async fn canonical_under(p: &Path, root: &Path) -> bool {
    match tokio::fs::canonicalize(root).await {
        Ok(canon_root) => p.starts_with(&canon_root),
        Err(_) => false,
    }
}

#[derive(Debug)]
pub enum SkillsError {
    Io(String),
    OutsideRoots,
}

impl IntoResponse for SkillsError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Io(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
            Self::OutsideRoots => (
                StatusCode::FORBIDDEN,
                "path is not inside either skills tree".to_string(),
            ),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
