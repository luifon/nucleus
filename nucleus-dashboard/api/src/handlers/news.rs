//! News surface — read API + admin views.
//!
//! Lifted from the standalone `news/api/` crate (ADR-015 §"Migration").
//! All routes mount under `/news/api/*` — including the operator views
//! (recent fetch runs, source health); there is no `/admin/*` prefix.
//! Behind the tailnet post-ADR-011 (not public).

use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use chrono::Utc;
use news_fetcher::store::{self, BriefStanding, IncomingVote, LATEST_VOTE_SQL};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;

#[derive(Clone)]
pub struct NewsState {
    pub pool: SqlitePool,
}

pub fn router(state: Arc<NewsState>) -> Router {
    Router::new()
        .route("/items", get(list_items))
        .route("/items/notable", get(list_notable))
        .route("/sources", get(list_sources))
        .route("/runs", get(list_runs))
        .route("/brief", get(day_brief))
        .route("/vote", post(vote))
        .route("/open", post(open))
        .with_state(state)
}

#[derive(Deserialize, Default)]
struct ListQ {
    /// Filter by the date items entered our DB (the "fetch bucket").
    /// Accepts `fetch_date` (preferred) or `day` (legacy alias).
    fetch_date: Option<String>,
    day: Option<String>,
    min_score: Option<f64>,
    limit: Option<i64>,
}

impl ListQ {
    fn fetch_date_or_today(&self) -> String {
        self.fetch_date
            .clone()
            .or_else(|| self.day.clone())
            .unwrap_or_else(|| Utc::now().format("%Y-%m-%d").to_string())
    }
}

#[derive(Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
struct ItemDto {
    id: String,
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    source_id: i64,
    source_name: String,
    url: String,
    article_url: Option<String>,
    title: String,
    summary: Option<String>,
    published_at: String,
    published_date: String,
    fetch_date: String,
    notable_score: Option<f64>,
    notable_reason: Option<String>,
    /// Display label for the underlying event (ADR-031). Never a dedup key.
    event_slug: Option<String>,
    /// 1 when the ranker judged this resurfaced old content. Stale items are
    /// kept in the DB and withheld from the widget.
    #[ts(type = "number")]
    stale: i64,
    /// Effective vote: the latest vote row for this item, 0 when there is
    /// none. Votes supersede rather than accumulate (ADR-031), so this is a
    /// state, not a tally.
    #[ts(type = "number")]
    vote: i64,
    /// The reason on the effective vote, one of the fetcher's `VOTE_REASONS`.
    /// A reason is its own vote row, so a later vote without one clears it.
    vote_reason: Option<String>,
    /// Free text on an `other` reason.
    vote_note: Option<String>,
    /// The reader has opened the item, from the widget or from here.
    opened: bool,
}

/// Every item column the page shows, with the effective vote joined in. The
/// vote query is the fetcher's own, so both surfaces agree on which row wins.
fn item_select(filter: &str) -> String {
    format!(
        r#"
        SELECT i.id, i.source_id, s.name AS source_name,
               i.url, i.article_url,
               i.title, i.summary, i.published_at, i.published_date,
               i.fetch_date, i.notable_score, i.notable_reason, i.event_slug, i.stale,
               COALESCE(ev.vote, 0) AS vote,
               ev.reason_key AS vote_reason,
               ev.note AS vote_note,
               EXISTS(SELECT 1 FROM opens o WHERE o.item_id = i.id) AS opened
        FROM items i
        JOIN sources s ON s.id = i.source_id
        LEFT JOIN ({LATEST_VOTE_SQL}) ev ON ev.item_id = i.id
        {filter}
        "#
    )
}

/// Same-event items next to each other, as the widget shows them (ADR-031).
fn grouped(rows: Vec<ItemDto>) -> Vec<ItemDto> {
    news_fetcher::group_same_event_adjacent(rows, |r| r.event_slug.as_deref().unwrap_or(""))
}

async fn list_items(
    State(s): State<Arc<NewsState>>,
    Query(q): Query<ListQ>,
) -> Result<Json<Vec<ItemDto>>, NewsError> {
    let day = q.fetch_date_or_today();
    let min_score = q.min_score.unwrap_or(0.0);
    let limit = q.limit.unwrap_or(200).clamp(1, 500);
    let rows: Vec<ItemDto> = sqlx::query_as::<_, ItemDto>(&item_select(
        "WHERE i.fetch_date = ?1
           AND COALESCE(i.notable_score, 0) >= ?2
         ORDER BY i.notable_score DESC NULLS LAST, i.published_at DESC
         LIMIT ?3",
    ))
    .bind(day)
    .bind(min_score)
    .bind(limit)
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(grouped(rows)))
}

async fn list_notable(
    State(s): State<Arc<NewsState>>,
    Query(q): Query<ListQ>,
) -> Result<Json<Vec<ItemDto>>, NewsError> {
    let day = q.fetch_date_or_today();
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let rows: Vec<ItemDto> = sqlx::query_as::<_, ItemDto>(&item_select(
        "WHERE i.fetch_date = ?1
           AND i.stale = 0
           AND COALESCE(i.notable_score, 0) >= 0.6
         ORDER BY i.notable_score DESC, i.published_at DESC
         LIMIT ?2",
    ))
    .bind(day)
    .bind(limit)
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(grouped(rows)))
}

#[derive(Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
struct SourceDto {
    #[ts(type = "number")]
    id: i64,
    name: String,
    url: String,
    #[ts(type = "number")]
    enabled: i64,
    last_fetched_at: Option<String>,
    last_error: Option<String>,
}

async fn list_sources(State(s): State<Arc<NewsState>>) -> Result<Json<Vec<SourceDto>>, NewsError> {
    let rows: Vec<SourceDto> = sqlx::query_as::<_, SourceDto>(
        "SELECT id, name, url, enabled, last_fetched_at, last_error FROM sources ORDER BY name",
    )
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(rows))
}

#[derive(Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
struct RunDto {
    run_id: String,
    started_at: String,
    finished_at: Option<String>,
    #[ts(type = "number")]
    ok: i64,
    error: Option<String>,
    // Per-run funnel (ADR-031): how many entries the feeds produced, what
    // each mechanical filter removed, and what reached the reader.
    #[ts(type = "number")]
    items_input: i64,
    #[ts(type = "number")]
    rejected_stale: i64,
    #[ts(type = "number")]
    rejected_dup_url: i64,
    #[ts(type = "number")]
    rejected_dup_title: i64,
    #[ts(type = "number")]
    items_ranked: i64,
    #[ts(type = "number")]
    items_surfaced: i64,
    #[ts(type = "number")]
    brief_ok: i64,
    /// 1 when the brief blew the widget's word cap twice and the previous
    /// one was kept instead (ADR-031).
    #[ts(type = "number")]
    brief_too_long: i64,
    /// 1 when the brief call failed and the stored brief couldn't stand in
    /// because it named an item the reader has since downvoted. The day went
    /// out with no brief rather than with a retracted recommendation.
    #[ts(type = "number")]
    brief_dropped_downvoted: i64,
    profile_hash: Option<String>,
}

async fn list_runs(State(s): State<Arc<NewsState>>) -> Result<Json<Vec<RunDto>>, NewsError> {
    let rows: Vec<RunDto> = sqlx::query_as::<_, RunDto>(
        "SELECT run_id, started_at, finished_at, ok, error,
                items_input, rejected_stale, rejected_dup_url, rejected_dup_title,
                items_ranked, items_surfaced, brief_ok, brief_too_long,
                brief_dropped_downvoted, profile_hash
           FROM fetcher_runs ORDER BY started_at DESC LIMIT 30",
    )
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(rows))
}

/// The day's brief as the fetcher stored it (ADR-031).
#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct BriefDto {
    run_id: String,
    created_at: String,
    text: String,
    standing: BriefStandingDto,
}

/// Whether the widget could still show the brief. `names_downvoted` means it
/// was written from an item downvoted since; `unverifiable` means it predates
/// the record of its items.
#[derive(Serialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
enum BriefStandingDto {
    Current,
    NamesDownvoted,
    Unverifiable,
}

impl From<BriefStanding> for BriefStandingDto {
    fn from(s: BriefStanding) -> Self {
        match s {
            BriefStanding::Current => Self::Current,
            BriefStanding::NamesDownvoted => Self::NamesDownvoted,
            BriefStanding::Unverifiable => Self::Unverifiable,
        }
    }
}

async fn day_brief(
    State(s): State<Arc<NewsState>>,
    Query(q): Query<ListQ>,
) -> Result<Json<Option<BriefDto>>, NewsError> {
    let brief = store::brief_for_fetch_date(&s.pool, &q.fetch_date_or_today())
        .await
        .map_err(NewsError::Store)?;
    Ok(Json(brief.map(|b| BriefDto {
        run_id: b.run_id,
        created_at: b.created_at,
        text: b.text,
        standing: b.standing.into(),
    })))
}

#[derive(Deserialize, ts_rs::TS)]
#[ts(export)]
struct VoteReq {
    item_id: String,
    vote: i32,
    /// Why, on a downvote: one of the fetcher's `VOTE_REASONS`.
    #[serde(default)]
    #[ts(optional)]
    reason: Option<String>,
    /// Free text, only with the `other` reason.
    #[serde(default)]
    #[ts(optional)]
    note: Option<String>,
}

/// The reason and note a vote may carry, under the widget's rules: a reason
/// only on a downvote and only from the closed set, a note only with `other`,
/// and `other` only with a note. The widget stores an unknown key as NULL
/// because an outbox can come from an older build; this route has one client,
/// so it refuses instead.
fn checked_reason(vote: i64, reason: Option<&str>, note: Option<&str>) -> Result<(Option<String>, Option<String>), String> {
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    if let Some(r) = reason {
        if vote != -1 {
            return Err("a reason goes only with a downvote".into());
        }
        if !store::is_known_vote_reason(r) {
            return Err(format!("unknown reason {r}"));
        }
    }
    match (reason, note) {
        (Some("other"), None) => Err("the `other` reason needs a note".into()),
        (Some("other"), Some(n)) if n.chars().count() > store::MAX_VOTE_NOTE_CHARS => {
            Err(format!("the note is longer than {} characters", store::MAX_VOTE_NOTE_CHARS))
        }
        (_, Some(_)) if reason != Some("other") => Err("a note goes only with the `other` reason".into()),
        _ => Ok((reason.map(str::to_string), note.map(str::to_string))),
    }
}

async fn vote(
    State(s): State<Arc<NewsState>>,
    Json(req): Json<VoteReq>,
) -> Result<Json<serde_json::Value>, NewsError> {
    // Out-of-range votes are refused, as the widget ingest skips them: an
    // invalid request must not become a preference.
    if !(-1..=1).contains(&req.vote) {
        return Err(NewsError::BadRequest(format!("vote must be -1, 0 or 1, not {}", req.vote)));
    }
    let v = i64::from(req.vote);
    let (reason, note) =
        checked_reason(v, req.reason.as_deref(), req.note.as_deref()).map_err(NewsError::BadRequest)?;
    // Votes are append-only and supersede by timestamp (ADR-031): a reversal
    // is a new row, 0 takes a vote back, and a reason is a new downvote row
    // carrying it. The fetcher's insert normalizes the stamp to the sortable
    // form the widget's votes arrive in.
    let vote_id = uuid::Uuid::new_v4().to_string();
    let now = nucleus_core::timestamp::now();
    let inserted = store::insert_vote(
        &s.pool,
        &IncomingVote {
            vote_id: &vote_id,
            item_id: &req.item_id,
            vote: v,
            origin: "dashboard",
            created_at: &now,
            reason_key: reason.as_deref(),
            note: note.as_deref(),
        },
    )
    .await
    .map_err(NewsError::Store)?;
    if !inserted {
        return Err(NewsError::BadRequest(format!("unknown item {}", req.item_id)));
    }
    Ok(Json(serde_json::json!({
        "ok": true, "item_id": req.item_id, "vote": v, "reason": reason, "note": note,
    })))
}

#[derive(Deserialize, ts_rs::TS)]
#[ts(export)]
struct OpenReq {
    item_id: String,
    /// The link that was opened: the item's `url` or its `article_url`.
    url: String,
}

/// Record that the reader opened an item here. Opens are attention, not
/// preference (ADR-031): they only mark the item as read.
async fn open(
    State(s): State<Arc<NewsState>>,
    Json(req): Json<OpenReq>,
) -> Result<Json<serde_json::Value>, NewsError> {
    let links: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT url, article_url FROM items WHERE id = ?1")
            .bind(&req.item_id)
            .fetch_optional(&s.pool)
            .await?;
    let Some((url, article_url)) = links else {
        return Err(NewsError::BadRequest(format!("unknown item {}", req.item_id)));
    };
    if req.url != url && article_url.as_deref() != Some(req.url.as_str()) {
        return Err(NewsError::BadRequest("the URL is not one of the item's links".into()));
    }
    store::insert_open(
        &s.pool,
        &uuid::Uuid::new_v4().to_string(),
        &req.item_id,
        &req.url,
        "dashboard",
        &nucleus_core::timestamp::now(),
    )
    .await
    .map_err(NewsError::Store)?;
    Ok(Json(serde_json::json!({ "ok": true, "item_id": req.item_id })))
}

#[derive(Debug)]
pub enum NewsError {
    Sqlx(sqlx::Error),
    Store(anyhow::Error),
    BadRequest(String),
}

impl From<sqlx::Error> for NewsError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sqlx(e)
    }
}

impl IntoResponse for NewsError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Sqlx(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("db: {}", e),
            ),
            Self::Store(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e:#}")),
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const DAY: &str = "2026-09-13";

    struct Fixture {
        app: Router,
        pool: SqlitePool,
        _dir: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let pool = nucleus_core::db::open(&dir.path().join("news.db")).await.unwrap();
        nucleus_core::migrate::migrate(&pool, store::MIGRATIONS).await.unwrap();
        sqlx::query("INSERT INTO sources (id, name, url) VALUES (1, 'Feed', 'https://feed.example/rss')")
            .execute(&pool)
            .await
            .unwrap();
        let app = router(Arc::new(NewsState { pool: pool.clone() }));
        Fixture { app, pool, _dir: dir }
    }

    async fn item(pool: &SqlitePool, id: &str, score: f64, event: &str) {
        sqlx::query(
            "INSERT INTO items (id, source_id, url, article_url, canonical_url, title, published_at,
                                published_date, fetched_at, fetch_date, notable_score, event_slug)
             VALUES (?1, 1, ?2, ?3, ?3, ?1, ?4, ?5, ?4, ?5, ?6, ?7)",
        )
        .bind(id)
        .bind(format!("https://discuss.example/{id}"))
        .bind(format!("https://article.example/{id}"))
        .bind(format!("{DAY}T09:00:00.000Z"))
        .bind(DAY)
        .bind(score)
        .bind(event)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn call(app: &Router, method: &str, uri: &str, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    async fn post_vote(app: &Router, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        call(app, "POST", "/vote", Some(body)).await
    }

    async fn listed(app: &Router) -> Vec<serde_json::Value> {
        let (status, body) = call(app, "GET", &format!("/items?fetch_date={DAY}"), None).await;
        assert_eq!(status, StatusCode::OK);
        body.as_array().unwrap().clone()
    }

    fn find<'a>(items: &'a [serde_json::Value], id: &str) -> &'a serde_json::Value {
        items.iter().find(|i| i["id"] == id).unwrap()
    }

    #[tokio::test]
    async fn items_of_one_event_are_listed_together_under_the_best_scored_one() {
        let f = fixture().await;
        item(&f.pool, "a", 0.9, "rubygems-attack").await;
        item(&f.pool, "b", 0.7, "model-release").await;
        item(&f.pool, "c", 0.5, "rubygems-attack").await;
        let ids: Vec<String> = listed(&f.app).await.iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, ["a", "c", "b"]);
    }

    #[tokio::test]
    async fn a_downvote_reason_and_its_note_come_back_on_the_item() {
        let f = fixture().await;
        item(&f.pool, "a", 0.9, "e").await;
        let (status, _) = post_vote(&f.app, serde_json::json!({ "item_id": "a", "vote": -1 })).await;
        assert_eq!(status, StatusCode::OK);
        let items = listed(&f.app).await;
        assert_eq!(find(&items, "a")["vote"], -1);
        assert!(find(&items, "a")["vote_reason"].is_null());

        let (status, _) = post_vote(
            &f.app,
            serde_json::json!({ "item_id": "a", "vote": -1, "reason": "other", "note": " covered last week " }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let items = listed(&f.app).await;
        assert_eq!(find(&items, "a")["vote_reason"], "other");
        assert_eq!(find(&items, "a")["vote_note"], "covered last week");

        // Taking the vote back is a new row without a reason.
        post_vote(&f.app, serde_json::json!({ "item_id": "a", "vote": 0 })).await;
        let items = listed(&f.app).await;
        assert_eq!(find(&items, "a")["vote"], 0);
        assert!(find(&items, "a")["vote_reason"].is_null());
    }

    #[tokio::test]
    async fn a_vote_reason_outside_the_widget_rules_is_refused() {
        let f = fixture().await;
        item(&f.pool, "a", 0.9, "e").await;
        let long = "x".repeat(store::MAX_VOTE_NOTE_CHARS + 1);
        for body in [
            serde_json::json!({ "item_id": "a", "vote": 1, "reason": "dup" }),
            serde_json::json!({ "item_id": "a", "vote": -2, "reason": "dup" }),
            serde_json::json!({ "item_id": "a", "vote": 2 }),
            serde_json::json!({ "item_id": "a", "vote": -1, "reason": "dupe" }),
            serde_json::json!({ "item_id": "a", "vote": -1, "reason": "other" }),
            serde_json::json!({ "item_id": "a", "vote": -1, "reason": "dup", "note": "why" }),
            serde_json::json!({ "item_id": "a", "vote": -1, "note": "why" }),
            serde_json::json!({ "item_id": "a", "vote": -1, "reason": "other", "note": long }),
            serde_json::json!({ "item_id": "ghost", "vote": -1, "reason": "dup" }),
        ] {
            let (status, _) = post_vote(&f.app, body.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }
        let votes: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM votes").fetch_one(&f.pool).await.unwrap();
        assert_eq!(votes.0, 0, "a refused vote writes nothing");
    }

    #[tokio::test]
    async fn an_open_marks_the_item_read_and_only_accepts_its_own_links() {
        let f = fixture().await;
        item(&f.pool, "a", 0.9, "e").await;
        item(&f.pool, "b", 0.8, "f").await;
        let (status, _) = call(
            &f.app,
            "POST",
            "/open",
            Some(serde_json::json!({ "item_id": "a", "url": "https://article.example/a" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let items = listed(&f.app).await;
        assert_eq!(find(&items, "a")["opened"], true);
        assert_eq!(find(&items, "b")["opened"], false);
        assert_eq!(find(&items, "a")["vote"], 0, "an open is not a vote");

        for body in [
            serde_json::json!({ "item_id": "b", "url": "https://article.example/a" }),
            serde_json::json!({ "item_id": "ghost", "url": "https://article.example/a" }),
        ] {
            let (status, _) = call(&f.app, "POST", "/open", Some(body.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }
    }

    #[tokio::test]
    async fn the_day_brief_says_when_one_of_its_items_was_downvoted_since() {
        let f = fixture().await;
        item(&f.pool, "a", 0.9, "e").await;
        let (_, body) = call(&f.app, "GET", &format!("/brief?fetch_date={DAY}"), None).await;
        assert!(body.is_null(), "no brief that day");

        sqlx::query("INSERT INTO fetcher_runs (run_id, started_at) VALUES ('r1', ?1)")
            .bind(format!("{DAY}T12:00:00.000Z"))
            .execute(&f.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO briefs (run_id, created_at, text, item_ids) VALUES ('r1', ?1, 'Read a.', '[\"a\"]')")
            .bind(format!("{DAY}T12:00:00.000Z"))
            .execute(&f.pool)
            .await
            .unwrap();
        let (_, body) = call(&f.app, "GET", &format!("/brief?fetch_date={DAY}"), None).await;
        assert_eq!(body["text"], "Read a.");
        assert_eq!(body["standing"], "current");

        post_vote(&f.app, serde_json::json!({ "item_id": "a", "vote": -1 })).await;
        let (_, body) = call(&f.app, "GET", &format!("/brief?fetch_date={DAY}"), None).await;
        assert_eq!(body["standing"], "names_downvoted");
    }
}
