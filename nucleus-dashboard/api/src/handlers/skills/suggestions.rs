//! Skill suggestions on the Skills page (ADR-038): the suggestions the
//! skill-gap learner (or a session) filed, for the operator to accept,
//! reject or edit.
//!
//!   GET  /skills/api/suggestions               → `SuggestionSummary[]`, newest first, every status
//!   GET  /skills/api/suggestions/detail?id=    → `SuggestionReview`
//!   POST /skills/api/suggestions/accept {id, draft_hash} → `SuggestionAcceptResp`
//!   POST /skills/api/suggestions/reject {id, reason}  → `SuggestionReview`
//!   POST /skills/api/suggestions/edit   {id, draft}   → `SuggestionReview`
//!
//! Every read and every state change goes through
//! `nucleus_core::skill_learner::suggestions`, which owns
//! `memory/skill_learner.db` and the rules: the draft checks, the patch
//! base-hash binding, the library write lock, the status transitions. This
//! module only maps its results and refusals to HTTP. A refusal because of
//! the draft (`InvalidDraft`) is 422 with each problem in `issues`; any
//! other refusal (not pending, the draft changed since the page showed it,
//! the library lock is held, a symlink) is 409 with core's text.
//!
//! Reads open the store read-only and treat a missing store as empty.
//! Writes follow the Work and Tasks threat model (ADR-033 §7): a JSON body
//! only, and a request a browser marks as cross-site is refused. An accept
//! writes into the personal tree, so it is committed in the `.nucleus`
//! repository like the page's other personal-tier changes.

use super::{commit_private, GitOutcome, SkillsError, SkillsState};
use axum::{
    extract::{rejection::JsonRejection, Query, State},
    http::HeaderMap,
    response::Json,
};
use nucleus_core::skill_learner::{
    self,
    suggestions::{self, AcceptOutcome, InvalidDraft, SuggestionReview, SuggestionSummary},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// `POST /skills/api/suggestions/accept`.
#[derive(Debug, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct AcceptSuggestionReq {
    #[ts(type = "number")]
    pub id: i64,
    /// `SuggestionReview.draft_hash` of the draft on screen. Accept is
    /// refused (409) when the stored draft is a different one.
    pub draft_hash: String,
}

/// `POST /skills/api/suggestions/reject`. Later reviews see the reason.
#[derive(Debug, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct RejectSuggestionReq {
    #[ts(type = "number")]
    pub id: i64,
    /// Empty or null: no reason.
    pub reason: Option<String>,
}

/// `POST /skills/api/suggestions/edit` — replace the draft of a pending
/// suggestion. Checked like any draft; the ownership fields are set by core.
#[derive(Debug, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct EditSuggestionReq {
    #[ts(type = "number")]
    pub id: i64,
    pub draft: String,
}

/// Result of an accept.
#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct SuggestionAcceptResp {
    pub accepted: AcceptOutcome,
    /// The commit of the new SKILL.md in `.nucleus`.
    pub git: GitOutcome,
}

#[derive(Deserialize)]
pub(super) struct DetailQ {
    id: i64,
}

pub(super) async fn list(State(s): State<Arc<SkillsState>>) -> Result<Json<Vec<SuggestionSummary>>, SkillsError> {
    let Some(pool) = skill_learner::open_read_only(s.workspace_root()).await.map_err(io)? else {
        return Ok(Json(Vec::new()));
    };
    Ok(Json(suggestions::list(&pool, &[]).await.map_err(io)?))
}

pub(super) async fn detail(
    State(s): State<Arc<SkillsState>>,
    Query(q): Query<DetailQ>,
) -> Result<Json<SuggestionReview>, SkillsError> {
    let pool = skill_learner::open_read_only(s.workspace_root())
        .await
        .map_err(io)?
        .ok_or_else(|| not_found(q.id))?;
    let review = suggestions::review(&pool, s.workspace_root(), &s.roots, q.id).await.map_err(io)?;
    review.map(Json).ok_or_else(|| not_found(q.id))
}

pub(super) async fn accept(
    State(s): State<Arc<SkillsState>>,
    headers: HeaderMap,
    payload: Result<Json<AcceptSuggestionReq>, JsonRejection>,
) -> Result<Json<SuggestionAcceptResp>, SkillsError> {
    same_origin(&headers)?;
    let Json(req) = payload?;
    let pool = writable(&s, req.id).await?;
    // Both locks are held through the `.nucleus` commit, as for a move or
    // archive: the page's own lock, and the cross-program library lock, so
    // the commit holds no change another writer made after the accept.
    let _guard = s.write_lock.lock().await;
    let library = s.library_guard().await?;
    let accepted = match suggestions::accept_locked(&pool, s.workspace_root(), &s.roots, req.id, &req.draft_hash, &library).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("skills: accept suggestion #{} refused: {e:#}", req.id);
            return Err(refusal(e));
        }
    };
    let git = commit_private(
        &s,
        format!("dashboard: accept skill suggestion #{} ({})", req.id, accepted.name),
    )
    .await;
    tracing::info!("skills: accepted suggestion #{} → {}; git: {git:?}", req.id, accepted.path);
    Ok(Json(SuggestionAcceptResp { accepted, git }))
}

pub(super) async fn reject(
    State(s): State<Arc<SkillsState>>,
    headers: HeaderMap,
    payload: Result<Json<RejectSuggestionReq>, JsonRejection>,
) -> Result<Json<SuggestionReview>, SkillsError> {
    same_origin(&headers)?;
    let Json(req) = payload?;
    let pool = writable(&s, req.id).await?;
    let reason = req.reason.as_deref().map(str::trim).filter(|r| !r.is_empty());
    if let Err(e) = suggestions::reject(&pool, req.id, reason).await {
        tracing::warn!("skills: reject suggestion #{} refused: {e:#}", req.id);
        return Err(refusal(e));
    }
    tracing::info!("skills: rejected suggestion #{}", req.id);
    reviewed(&s, &pool, req.id).await
}

pub(super) async fn edit(
    State(s): State<Arc<SkillsState>>,
    headers: HeaderMap,
    payload: Result<Json<EditSuggestionReq>, JsonRejection>,
) -> Result<Json<SuggestionReview>, SkillsError> {
    same_origin(&headers)?;
    let Json(req) = payload?;
    let pool = writable(&s, req.id).await?;
    if let Err(e) = suggestions::edit(&pool, s.workspace_root(), &s.roots, req.id, &req.draft).await {
        tracing::warn!("skills: edit suggestion #{} refused: {e:#}", req.id);
        return Err(refusal(e));
    }
    tracing::info!("skills: replaced the draft of suggestion #{}", req.id);
    reviewed(&s, &pool, req.id).await
}

// ─── helpers ───────────────────────────────────────────────────────────────

/// The store for a write, after checking that suggestion `id` exists (404
/// otherwise). A write never creates the store: without it there is no
/// suggestion to decide.
async fn writable(s: &SkillsState, id: i64) -> Result<sqlx::SqlitePool, SkillsError> {
    if !s.workspace_root().join(skill_learner::DB_PATH).exists() {
        return Err(not_found(id));
    }
    let pool = skill_learner::open(s.workspace_root()).await.map_err(io)?;
    if suggestions::get(&pool, id).await.map_err(io)?.is_none() {
        return Err(not_found(id));
    }
    Ok(pool)
}

async fn reviewed(s: &SkillsState, pool: &sqlx::SqlitePool, id: i64) -> Result<Json<SuggestionReview>, SkillsError> {
    let review = suggestions::review(pool, s.workspace_root(), &s.roots, id).await.map_err(io)?;
    review.map(Json).ok_or_else(|| not_found(id))
}

fn same_origin(headers: &HeaderMap) -> Result<(), SkillsError> {
    match crate::handlers::tasks::cross_site_reason(headers) {
        Some(why) => Err(SkillsError::Forbidden(why)),
        None => Ok(()),
    }
}

fn not_found(id: i64) -> SkillsError {
    SkillsError::NotFound(format!("there is no suggestion #{id}"))
}

fn io(e: anyhow::Error) -> SkillsError {
    SkillsError::Io(format!("{e:#}"))
}

/// Map a refusal from core: the draft's problems (422, listed), a database
/// or filesystem failure (500), anything else is a state conflict (409).
fn refusal(e: anyhow::Error) -> SkillsError {
    if let Some(d) = e.downcast_ref::<InvalidDraft>() {
        return SkillsError::Invalid { message: d.to_string(), issues: d.issues.clone() };
    }
    if e.downcast_ref::<sqlx::Error>().is_some() || e.downcast_ref::<std::io::Error>().is_some() {
        return io(e);
    }
    SkillsError::Conflict(format!("{e:#}"))
}
