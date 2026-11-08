//! Candidate workflows (ADR-038): work a review saw that may repeat.
//!
//! One review sees one part of one session, so a workflow the operator runs
//! once a day for a week looks like a one-off to each review. A review may
//! therefore name a *candidate*: a short kebab-case name, a one-line
//! description, and, when the work belongs to an existing personal skill,
//! that skill. Code records where it was seen (session, position range,
//! local date in `NUCLEUS_TZ`), and later reviews see the open candidates
//! and add evidence to one instead of opening a duplicate.
//!
//! Code, not the model, decides what happens next:
//!
//! - a candidate with evidence on [`SkillLearnerConfig::candidate_min_days`]
//!   distinct local dates or more is promoted: a review drafts it as a
//!   suggestion (origin `candidate`), and the candidate becomes `promoted`
//!   with the suggestion's id;
//! - a candidate that got no new evidence for
//!   [`SkillLearnerConfig::candidate_expire_days`] becomes `expired` (the
//!   row stays);
//! - a promotion whose review fails [`MAX_PROMOTION_FAILURES`] times
//!   becomes `failed`.
//!
//! Evidence of one day counts once, however many sessions show it. Every
//! write is one `BEGIN IMMEDIATE` transaction that re-checks the status it
//! changes.
//!
//! [`SkillLearnerConfig::candidate_min_days`]: crate::config::SkillLearnerConfig::candidate_min_days
//! [`SkillLearnerConfig::candidate_expire_days`]: crate::config::SkillLearnerConfig::candidate_expire_days

use super::ledger::Source;
use super::suggestions::valid_name;
use anyhow::{Result, bail};
use sqlx::{Row, SqlitePool};

/// Longest candidate description.
pub const MAX_DESCRIPTION_CHARS: usize = 200;
/// Longest evidence note.
pub const MAX_NOTE_CHARS: usize = 300;
/// Failed promotion reviews after which a candidate is given up.
pub const MAX_PROMOTION_FAILURES: i64 = 2;

/// An open (or closed) candidate with the days it was seen on.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: i64,
    pub name: String,
    pub description: String,
    /// The existing personal skill this work belongs to, when the review
    /// tied it to one: promotion drafts a patch of it.
    pub skill: Option<String>,
    pub status: String,
    /// Distinct local dates (`YYYY-MM-DD`) of its evidence, oldest first.
    pub days: Vec<String>,
    pub evidence_count: i64,
    pub suggestion_id: Option<i64>,
    pub failures: i64,
    pub last_evidence_at: String,
}

/// One place a candidate was seen.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateEvidence {
    pub session_id: String,
    pub source: Source,
    pub agent: Option<String>,
    /// The session part: transcript lines, or index turns, `[from, to)`.
    pub from_pos: i64,
    pub to_pos: i64,
    /// Local date (`NUCLEUS_TZ`) of the work, `YYYY-MM-DD`.
    pub local_date: String,
    pub note: String,
}

/// What a review asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Recorded {
    /// A new candidate was opened.
    Opened(i64),
    /// Evidence was added to an open candidate.
    Evidence(i64),
    /// The candidate already had this part as evidence.
    Unchanged(i64),
}

impl Recorded {
    pub fn id(&self) -> i64 {
        match self {
            Recorded::Opened(id) | Recorded::Evidence(id) | Recorded::Unchanged(id) => *id,
        }
    }
}

fn clip(text: &str, max: usize) -> String {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= max {
        return t;
    }
    let mut out: String = t.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn valid_date(d: &str) -> bool {
    chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok()
}

async fn insert_evidence(conn: &mut sqlx::SqliteConnection, id: i64, ev: &CandidateEvidence, now: &str) -> Result<bool> {
    let done = sqlx::query(
        "INSERT OR IGNORE INTO candidate_evidence
            (candidate_id, session_id, source, agent, from_pos, to_pos, local_date, note, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(id)
    .bind(&ev.session_id)
    .bind(ev.source.as_str())
    .bind(ev.agent.as_deref())
    .bind(ev.from_pos)
    .bind(ev.to_pos)
    .bind(&ev.local_date)
    .bind(clip(&ev.note, MAX_NOTE_CHARS))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query("UPDATE candidates SET last_evidence_at = ?2, updated_at = ?2 WHERE id = ?1")
        .bind(id)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    Ok(true)
}

/// Record a candidate named `name` seen at `ev`: evidence for the open
/// candidate of that name, or a new candidate. `skill` ties a new
/// candidate to an existing personal skill (the caller checks that it is
/// one); an open candidate keeps its tie, and gets one when it had none.
pub async fn record(
    pool: &SqlitePool,
    name: &str,
    description: &str,
    skill: Option<&str>,
    ev: &CandidateEvidence,
) -> Result<Recorded> {
    if !valid_name(name) {
        bail!("candidate name {name:?} is not a class-level kebab-case name");
    }
    if let Some(s) = skill {
        if !valid_name(s) {
            bail!("candidate skill {s:?} is not a skill name");
        }
    }
    let description = clip(description, MAX_DESCRIPTION_CHARS);
    if description.is_empty() {
        bail!("candidate `{name}` has no description");
    }
    if !valid_date(&ev.local_date) {
        bail!("evidence date {:?} is not YYYY-MM-DD", ev.local_date);
    }
    let now = crate::timestamp::now();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let open: Option<i64> = sqlx::query_scalar("SELECT id FROM candidates WHERE name = ?1 AND status = 'open'")
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;
    let out = match open {
        Some(id) => {
            if let Some(s) = skill {
                sqlx::query("UPDATE candidates SET skill = ?2 WHERE id = ?1 AND skill IS NULL")
                    .bind(id)
                    .bind(s)
                    .execute(&mut *tx)
                    .await?;
            }
            if insert_evidence(&mut tx, id, ev, &now).await? { Recorded::Evidence(id) } else { Recorded::Unchanged(id) }
        }
        None => {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO candidates (name, description, skill, status, created_at, updated_at, last_evidence_at)
                 VALUES (?1, ?2, ?3, 'open', ?4, ?4, ?4) RETURNING id",
            )
            .bind(name)
            .bind(&description)
            .bind(skill)
            .bind(&now)
            .fetch_one(&mut *tx)
            .await?;
            insert_evidence(&mut tx, id, ev, &now).await?;
            Recorded::Opened(id)
        }
    };
    tx.commit().await?;
    Ok(out)
}

/// Add evidence to open candidate `id`. Refused when it is not open.
pub async fn add_evidence(pool: &SqlitePool, id: i64, ev: &CandidateEvidence) -> Result<Recorded> {
    if !valid_date(&ev.local_date) {
        bail!("evidence date {:?} is not YYYY-MM-DD", ev.local_date);
    }
    let now = crate::timestamp::now();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let open: Option<i64> = sqlx::query_scalar("SELECT id FROM candidates WHERE id = ?1 AND status = 'open'")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if open.is_none() {
        bail!("candidate #{id} is not open");
    }
    let added = insert_evidence(&mut tx, id, ev, &now).await?;
    tx.commit().await?;
    Ok(if added { Recorded::Evidence(id) } else { Recorded::Unchanged(id) })
}

fn from_row(r: &sqlx::sqlite::SqliteRow) -> Candidate {
    let days: Option<String> = r.get("days");
    let mut days: Vec<String> = days.unwrap_or_default().split(',').filter(|d| !d.is_empty()).map(String::from).collect();
    days.sort();
    Candidate {
        id: r.get("id"),
        name: r.get("name"),
        description: r.get("description"),
        skill: r.get("skill"),
        status: r.get("status"),
        days,
        evidence_count: r.get("evidence_count"),
        suggestion_id: r.get("suggestion_id"),
        failures: r.get("failures"),
        last_evidence_at: r.get("last_evidence_at"),
    }
}

const SELECT: &str = "SELECT c.id, c.name, c.description, c.skill, c.status, c.suggestion_id, c.failures,
            c.last_evidence_at,
            (SELECT group_concat(DISTINCT e.local_date) FROM candidate_evidence e WHERE e.candidate_id = c.id) AS days,
            (SELECT COUNT(*) FROM candidate_evidence e WHERE e.candidate_id = c.id) AS evidence_count
       FROM candidates c";

/// The open candidates, oldest first.
pub async fn open(pool: &SqlitePool) -> Result<Vec<Candidate>> {
    let rows = sqlx::query(&format!("{SELECT} WHERE c.status = 'open' ORDER BY c.id")).fetch_all(pool).await?;
    Ok(rows.iter().map(from_row).collect())
}

/// One candidate.
pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Candidate>> {
    let row = sqlx::query(&format!("{SELECT} WHERE c.id = ?1")).bind(id).fetch_optional(pool).await?;
    Ok(row.as_ref().map(from_row))
}

/// A candidate's evidence, oldest first.
pub async fn evidence(pool: &SqlitePool, id: i64) -> Result<Vec<CandidateEvidence>> {
    let rows = sqlx::query(
        "SELECT session_id, source, agent, from_pos, to_pos, local_date, note
           FROM candidate_evidence WHERE candidate_id = ?1 ORDER BY local_date, id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(CandidateEvidence {
                session_id: r.get("session_id"),
                source: Source::parse(&r.get::<String, _>("source"))?,
                agent: r.get("agent"),
                from_pos: r.get("from_pos"),
                to_pos: r.get("to_pos"),
                local_date: r.get("local_date"),
                note: r.get("note"),
            })
        })
        .collect())
}

/// The open candidates seen on at least `min_days` distinct local dates:
/// the ones code promotes.
pub async fn ready(pool: &SqlitePool, min_days: u32) -> Result<Vec<Candidate>> {
    Ok(open(pool).await?.into_iter().filter(|c| c.days.len() >= min_days.max(1) as usize).collect())
}

/// Expire the open candidates whose newest evidence was recorded before
/// `before` (a [`crate::timestamp`] value). Returns their (id, name).
pub async fn expire(pool: &SqlitePool, before: &str) -> Result<Vec<(i64, String)>> {
    let now = crate::timestamp::now();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "UPDATE candidates SET status = 'expired', updated_at = ?2, closed_at = ?2,
                detail = 'no new evidence since ' || last_evidence_at
          WHERE status = 'open' AND last_evidence_at < ?1 RETURNING id, name",
    )
    .bind(before)
    .bind(&now)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Close open candidate `id` as promoted to suggestion `suggestion_id`
/// (a new suggestion, or the pending one it became evidence for).
pub async fn mark_promoted(pool: &SqlitePool, id: i64, suggestion_id: i64) -> Result<()> {
    let now = crate::timestamp::now();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let done = sqlx::query(
        "UPDATE candidates SET status = 'promoted', suggestion_id = ?2, updated_at = ?3, closed_at = ?3
          WHERE id = ?1 AND status = 'open'",
    )
    .bind(id)
    .bind(suggestion_id)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        bail!("candidate #{id} is not open");
    }
    tx.commit().await?;
    Ok(())
}

/// Record a failed promotion of open candidate `id`. After
/// [`MAX_PROMOTION_FAILURES`] the candidate becomes `failed`. Returns the
/// failure count.
pub async fn record_failure(pool: &SqlitePool, id: i64, detail: &str) -> Result<i64> {
    let now = crate::timestamp::now();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let failures: Option<i64> = sqlx::query_scalar(
        "UPDATE candidates SET failures = failures + 1, detail = ?2, updated_at = ?3
          WHERE id = ?1 AND status = 'open' RETURNING failures",
    )
    .bind(id)
    .bind(clip(detail, 600))
    .bind(&now)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(failures) = failures else {
        bail!("candidate #{id} is not open");
    };
    if failures >= MAX_PROMOTION_FAILURES {
        sqlx::query("UPDATE candidates SET status = 'failed', closed_at = ?2 WHERE id = ?1")
            .bind(id)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(failures)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(session: &str, from: i64, date: &str) -> CandidateEvidence {
        CandidateEvidence {
            session_id: session.into(),
            source: Source::Transcript,
            agent: Some("chat".into()),
            from_pos: from,
            to_pos: from + 10,
            local_date: date.into(),
            note: "the operator asked for the weekly export again".into(),
        }
    }

    #[tokio::test]
    async fn candidates_collect_days_and_close_once() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = super::super::open(tmp.path()).await.unwrap();

        let a = record(&pool, "weekly-export", "Export the weekly report", None, &ev("s1", 0, "2026-01-05")).await.unwrap();
        assert!(matches!(a, Recorded::Opened(_)));
        let id = a.id();
        // Same day, another session: evidence, still one day.
        let b = record(&pool, "weekly-export", "ignored", Some("report-ops"), &ev("s2", 0, "2026-01-05")).await.unwrap();
        assert_eq!(b, Recorded::Evidence(id));
        // The same part again is not counted twice.
        assert_eq!(add_evidence(&pool, id, &ev("s2", 0, "2026-01-05")).await.unwrap(), Recorded::Unchanged(id));
        assert!(ready(&pool, 2).await.unwrap().is_empty(), "one day only");
        let c = get(&pool, id).await.unwrap().unwrap();
        assert_eq!((c.days.clone(), c.evidence_count, c.skill.as_deref()), (vec!["2026-01-05".to_string()], 2, Some("report-ops")));

        add_evidence(&pool, id, &ev("s3", 40, "2026-01-07")).await.unwrap();
        let r = ready(&pool, 2).await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].days, vec!["2026-01-05".to_string(), "2026-01-07".to_string()]);
        assert_eq!(evidence(&pool, id).await.unwrap().len(), 3);

        // Refusals.
        assert!(record(&pool, "Not Kebab", "d", None, &ev("s1", 0, "2026-01-05")).await.is_err());
        assert!(record(&pool, "other-work", "d", None, &ev("s1", 0, "05/01/2026")).await.is_err());
        assert!(record(&pool, "other-work", "  ", None, &ev("s1", 0, "2026-01-05")).await.is_err());

        // Promotion closes it; a closed candidate takes no evidence, and the
        // name can open a new candidate later.
        let sid: i64 = sqlx::query_scalar(
            "INSERT INTO suggestions (kind, name, draft, rationale, origin, created_at, updated_at)
             VALUES ('create', 'weekly-export', 'd', 'r', 'candidate', 't', 't') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        mark_promoted(&pool, id, sid).await.unwrap();
        assert!(mark_promoted(&pool, id, sid).await.is_err());
        assert!(add_evidence(&pool, id, &ev("s4", 0, "2026-01-09")).await.is_err());
        let again = record(&pool, "weekly-export", "Export it", None, &ev("s4", 0, "2026-01-09")).await.unwrap();
        assert!(matches!(again, Recorded::Opened(n) if n != id));

        // Failures, then expiry.
        let f = record(&pool, "failing-work", "d", None, &ev("s5", 0, "2026-01-09")).await.unwrap().id();
        assert_eq!(record_failure(&pool, f, "unusable answer").await.unwrap(), 1);
        assert_eq!(record_failure(&pool, f, "unusable answer").await.unwrap(), 2);
        assert_eq!(get(&pool, f).await.unwrap().unwrap().status, "failed");
        let expired = expire(&pool, "9999-01-01T00:00:00.000Z").await.unwrap();
        assert_eq!(expired.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(), vec!["weekly-export"]);
        assert!(open(&pool).await.unwrap().is_empty());
        assert_eq!(get(&pool, id).await.unwrap().unwrap().status, "promoted", "expiry touches open candidates only");
    }
}
