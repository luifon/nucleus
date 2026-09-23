//! Skills surface — walk the operator-personal (`.nucleus/.claude/skills`,
//! gitignored) and repo-committed (`.claude/skills`) skill directories under
//! the workspace root and expose them as JSON. Discovery + frontmatter parsing live
//! in `nucleus_core::skills` (shared with the skill-gap learner, ADR-017) so
//! the dashboard and the learner read skills identically.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use nucleus_core::skills::{default_roots, read_skills, Skill, SKILL_FILE};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct SkillsState {
    /// `(root, tier)` pairs, from `nucleus_core::skills::default_roots`.
    pub roots: Vec<(PathBuf, &'static str)>,
}

impl SkillsState {
    pub fn new(workspace_root: &Path) -> Self {
        Self { roots: default_roots(workspace_root) }
    }
}

pub fn router(state: Arc<SkillsState>) -> Router {
    Router::new()
        .route("/list", get(list_skills))
        .route("/body", get(get_body))
        .with_state(state)
}

async fn list_skills(State(s): State<Arc<SkillsState>>) -> Result<Json<Vec<Skill>>, SkillsError> {
    let roots = s.roots.clone();
    // read_skills is sync fs; keep it off the async executor.
    let mut out = tokio::task::spawn_blocking(move || {
        roots.iter().flat_map(|(root, tier)| read_skills(root, tier)).collect::<Vec<_>>()
    })
    .await
    .map_err(|e| SkillsError::Io(format!("join: {e}")))?;
    out.sort_by(|a, b| a.tier.cmp(&b.tier).then_with(|| a.name.cmp(&b.name)));
    Ok(Json(out))
}

#[derive(Deserialize)]
struct BodyQ {
    path: String,
}

/// Returns raw SKILL.md content (frontmatter + body). Path-traversal guarded
/// by requiring the canonicalized path to sit inside one of the skills roots.
async fn get_body(
    State(s): State<Arc<SkillsState>>,
    Query(q): Query<BodyQ>,
) -> Result<String, SkillsError> {
    let requested = PathBuf::from(&q.path);
    let canonical = tokio::fs::canonicalize(&requested)
        .await
        .map_err(|e| SkillsError::Io(format!("canonicalizing {}: {}", q.path, e)))?;
    let mut inside = false;
    for (root, _) in &s.roots {
        if canonical_under(&canonical, root).await {
            inside = true;
            break;
        }
    }
    if !inside {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let md = dir.join(SKILL_FILE);
        std::fs::write(&md, format!("---\nname: {name}\ndescription: d\n---\nbody\n")).unwrap();
        md
    }

    #[tokio::test]
    async fn lists_both_workspace_trees_and_guards_body_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        let personal = write_skill(&nucleus_core::skills::personal_skills_root(&ws), "mine");
        let repo = write_skill(&nucleus_core::skills::repo_skills_root(&ws), "shared");
        let outside = write_skill(&ws.join("elsewhere"), "stray");
        let state = Arc::new(SkillsState::new(&ws));

        let Json(list) = list_skills(State(state.clone())).await.unwrap();
        let tiers: Vec<(String, String)> = list.iter().map(|s| (s.tier.clone(), s.name.clone())).collect();
        assert_eq!(
            tiers,
            vec![("personal".into(), "mine".into()), ("repo".into(), "shared".into())]
        );

        for ok in [&personal, &repo] {
            let q = BodyQ { path: ok.to_string_lossy().into_owned() };
            assert!(get_body(State(state.clone()), Query(q)).await.is_ok(), "{}", ok.display());
        }
        let q = BodyQ { path: outside.to_string_lossy().into_owned() };
        assert!(matches!(get_body(State(state), Query(q)).await, Err(SkillsError::OutsideRoots)));
    }
}
