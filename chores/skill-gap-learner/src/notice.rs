//! The WhatsApp notice after a nightly run (ADR-038).
//!
//! When the run's passes (sweep, backfill, candidate promotion, gap pass,
//! curator) filed or revised suggestions that are still pending at its end,
//! one message goes to the operator's DM: the count and a dashboard
//! link, from the `[skill_learner.texts]` templates. Code fills `{count}`
//! and `{link}` only. Suggestion names, rationales, drafts and quotes are
//! model output and session text and never go into the message.
//!
//! The row goes through `outbound_queue` with the target `dm`, so the bot's
//! target policy delivers it only to the operator (an operator-only row) and
//! its secret filter reads it like every other outbound text. The dedup key
//! is the store's random id and the set of suggestion ids, so a repeated run
//! with nothing new does not queue the same notice twice, and a recreated
//! store (whose ids restart) is not mistaken for the old one.

use anyhow::Result;
use nucleus_core::config::SkillLearnerTexts;
use nucleus_core::skill_learner::suggestions::{self, SuggestionStatus};
use nucleus_core::whatsapp_queue;
use sqlx::SqlitePool;

/// `outbound_queue.source` of the notice.
pub const SOURCE: &str = "skill-learner";
/// The dashboard page that lists the suggestions.
const DASHBOARD_ROUTE: &str = "/skills";

/// Of the suggestions this run filed or revised (`filed`), the ones still
/// pending, in id order. A suggestion revised later in the same run is
/// superseded and drops out; its revision is in `filed` too.
pub async fn new_pending(pool: &SqlitePool, filed: &[i64]) -> Result<Vec<i64>> {
    let pending = suggestions::list(pool, &[SuggestionStatus::Pending]).await?;
    let mut ids: Vec<i64> = pending.iter().map(|s| s.id).filter(|id| filed.contains(id)).collect();
    ids.sort_unstable();
    Ok(ids)
}

/// The dashboard link: the one suggestion's page when there is one, the
/// Skills page otherwise. Empty when `public_url` is unset.
pub fn link(public_url: Option<&str>, pending: &[i64]) -> String {
    match public_url.map(|u| u.trim().trim_end_matches('/')).filter(|u| !u.is_empty()) {
        Some(base) if pending.len() == 1 => format!("{base}{DASHBOARD_ROUTE}?suggestion={}", pending[0]),
        Some(base) => format!("{base}{DASHBOARD_ROUTE}"),
        None => String::new(),
    }
}

/// The notice text, or `None` when nothing new is pending.
pub fn text(texts: &SkillLearnerTexts, public_url: Option<&str>, pending: &[i64]) -> Option<String> {
    let template = match pending.len() {
        0 => return None,
        1 => &texts.notice_one,
        _ => &texts.notice_many,
    };
    let count = pending.len().to_string();
    let link = link(public_url, pending);
    let filled = nucleus_core::work::fill_once(template, &[("count", &count), ("link", &link)]);
    Some(filled.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Queue the notice for `pending` to the operator's DM. Returns the queue
/// row id, or `None` when nothing is pending. The same set of ids of the
/// same store (`store_id`, `skill_learner::store_id`) queues one row only;
/// a recreated store restarts its ids and has a new store id.
pub async fn queue(
    wa: &SqlitePool,
    store_id: &str,
    texts: &SkillLearnerTexts,
    public_url: Option<&str>,
    pending: &[i64],
) -> Result<Option<i64>> {
    let Some(body) = text(texts, public_url, pending) else {
        return Ok(None);
    };
    let ids: Vec<String> = pending.iter().map(i64::to_string).collect();
    let key = format!("{SOURCE}:notice:{store_id}:{}", ids.join(","));
    let id = whatsapp_queue::enqueue_text_once(wa, whatsapp_queue::INBOX_CHAT_OPERATOR_DM, &body, SOURCE, &key).await?;
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nucleus_core::skill_learner::{
        self,
        suggestions::{EvidenceSource, NewEvidence, NewSuggestion, SuggestionKind, SuggestionOrigin},
    };
    use nucleus_core::skills::LibraryRoots;

    const DRAFT: &str = "---\nname: x\ndescription: \"Deploy a service\"\n---\n\n# When to invoke\nWhen deploying.\n\n# Steps\n1. Build.\n\n# Failure modes\n- The push fails: retry once.\n";
    const URL: &str = "https://dash.example.test/";

    fn suggestion(name: &str, draft: &str) -> NewSuggestion {
        NewSuggestion {
            kind: SuggestionKind::Create,
            name: name.into(),
            draft: draft.into(),
            rationale: "MODEL-RATIONALE-TEXT".into(),
            origin: SuggestionOrigin::Live,
            expected_base: None,
            overrides_rejected: None,
            why_new: None,
            evidence: vec![NewEvidence {
                session_id: Some("sess-1".into()),
                source: EvidenceSource::Transcript,
                agent: None,
                note: "MODEL-NOTE-TEXT".into(),
                quotes: vec!["USER: QUOTED-TURN-TEXT".into()],
            }],
            retires: vec![],
        }
    }

    #[test]
    fn text_has_the_count_and_a_bare_link() {
        let t = SkillLearnerTexts::default();
        assert_eq!(text(&t, Some(URL), &[]), None, "nothing new, no notice");
        assert_eq!(
            text(&t, Some(URL), &[7]).unwrap(),
            "🧩 1 new skill suggestion to review on the dashboard. https://dash.example.test/skills?suggestion=7"
        );
        let many = text(&t, Some(URL), &[3, 4, 9]).unwrap();
        assert_eq!(many, "🧩 3 new skill suggestions to review on the dashboard. https://dash.example.test/skills");
        assert!(!many.contains('*'), "a link inside bold markers breaks on WhatsApp");
    }

    #[test]
    fn no_link_when_the_public_url_is_unset() {
        let t = SkillLearnerTexts::default();
        assert_eq!(text(&t, None, &[3, 4]).unwrap(), "🧩 2 new skill suggestions to review on the dashboard.");
        assert_eq!(text(&t, Some("  "), &[3]).unwrap(), "🧩 1 new skill suggestion to review on the dashboard.");
        assert_eq!(link(None, &[3]), "");
    }

    #[tokio::test]
    async fn queues_one_notice_for_the_new_pending_suggestions_without_their_text() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let roots = LibraryRoots::new(&ws, Some(&tmp.path().join("home")));
        let pool = skill_learner::open(&ws).await.unwrap();
        let a = suggestions::create(&pool, &ws, &roots, &suggestion("model-named-skill", DRAFT)).await.unwrap();
        let b = suggestions::create(&pool, &ws, &roots, &suggestion("other-model-skill", DRAFT)).await.unwrap();
        // Revised in the same run: `a` is superseded by its revision `c`.
        let c = suggestions::revise(&pool, &ws, &roots, a, &suggestion("model-named-skill", &DRAFT.replace("Build.", "Build twice.")))
            .await
            .unwrap();
        // Filed before this run: not counted.
        let older = suggestions::create(&pool, &ws, &roots, &suggestion("older-skill", DRAFT)).await.unwrap();
        // Filed and rejected in this run window: not pending, not counted.
        let d = suggestions::create(&pool, &ws, &roots, &suggestion("rejected-skill", DRAFT)).await.unwrap();
        suggestions::reject(&pool, d, None).await.unwrap();

        let pending = new_pending(&pool, &[a, b, d, c]).await.unwrap();
        assert_eq!(pending, vec![b, c]);
        assert!(!pending.contains(&older));

        let wa = whatsapp_queue::open(&ws).await.unwrap();
        let t = SkillLearnerTexts::default();
        let store = skill_learner::store_id(&pool).await.unwrap();
        let row = queue(&wa, &store, &t, Some(URL), &pending).await.unwrap().unwrap();
        // The same run again (a retry) queues nothing new.
        assert_eq!(queue(&wa, &store, &t, Some(URL), &pending).await.unwrap(), Some(row));
        assert_eq!(queue(&wa, &store, &t, Some(URL), &[]).await.unwrap(), None);

        let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT target, source, body FROM outbound_queue")
            .fetch_all(&wa)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let (target, source, body) = &rows[0];
        assert_eq!((target.as_str(), source.as_str()), ("dm", SOURCE), "operator-only target");
        assert_eq!(body, "🧩 2 new skill suggestions to review on the dashboard. https://dash.example.test/skills");
        for model_text in ["model-named-skill", "other-model-skill", "MODEL-RATIONALE-TEXT", "MODEL-NOTE-TEXT", "QUOTED-TURN-TEXT", "Build"] {
            assert!(!body.contains(model_text), "{model_text} in {body}");
        }
    }

    #[tokio::test]
    async fn a_recreated_store_gets_its_own_notices() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let roots = LibraryRoots::new(&ws, Some(&tmp.path().join("home")));
        let wa = whatsapp_queue::open(&ws).await.unwrap();
        let t = SkillLearnerTexts::default();

        let pool = skill_learner::open(&ws).await.unwrap();
        let first = skill_learner::store_id(&pool).await.unwrap();
        assert_eq!(first.len(), 32);
        let id = suggestions::create(&pool, &ws, &roots, &suggestion("first-skill", DRAFT)).await.unwrap();
        assert!(queue(&wa, &first, &t, None, &[id]).await.unwrap().is_some());
        pool.close().await;

        // The store is deleted and created again: the ids restart at 1.
        for f in ["skill_learner.db", "skill_learner.db-wal", "skill_learner.db-shm"] {
            let _ = std::fs::remove_file(ws.join("memory").join(f));
        }
        let pool = skill_learner::open(&ws).await.unwrap();
        let second = skill_learner::store_id(&pool).await.unwrap();
        assert_ne!(first, second);
        let again = suggestions::create(&pool, &ws, &roots, &suggestion("second-skill", DRAFT)).await.unwrap();
        assert_eq!(again, id, "ids restart in a new store");
        assert!(queue(&wa, &second, &t, None, &[again]).await.unwrap().is_some());
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbound_queue").fetch_one(&wa).await.unwrap();
        assert_eq!(rows, 2, "the new store's notice is not dropped as a duplicate");
    }
}
