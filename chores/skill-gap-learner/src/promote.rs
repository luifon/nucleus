//! Candidate workflows: expiry and promotion (ADR-038).
//!
//! Session reviews name candidate workflows and add evidence to them
//! (`review`, `nucleus_core::skill_learner::candidates`). This step decides
//! what happens to them, in code:
//!
//! 1. an open candidate with no new evidence for `candidate_expire_days` is
//!    expired;
//! 2. an open candidate with evidence on `candidate_min_days` distinct local
//!    dates is promoted: code reads the session parts at its evidence
//!    positions, and one review session drafts the suggestion (origin
//!    `candidate`) through the same checks and filing as every review. The
//!    candidate then points at the suggestion it became, or at the pending
//!    suggestion it became evidence for.
//!
//! A promotion whose answer is unusable, or whose one item fails its
//! checks, counts a failure; after `MAX_PROMOTION_FAILURES` the candidate is
//! `failed`. A reviewer that cannot be reached stops the step without
//! counting anything.

use crate::parts;
use crate::review::{self, Backend, Excerpt, Library, ReviewError, Task};
use crate::sweep::Env;
use anyhow::Result;
use nucleus_core::claude_session::transcript_events;
use nucleus_core::session_index;
use nucleus_core::skill_learner::candidates::{self, Candidate, CandidateEvidence};
use nucleus_core::skill_learner::ledger::{self, RunStart, Source};
use nucleus_core::skill_learner::suggestions::SuggestionOrigin;

/// Most excerpts one promotion prompt carries.
const MAX_EXCERPTS: usize = 6;
/// The excerpts' share of the prompt: a session part's size.
const EXCERPTS_BUDGET: usize = review::MAX_WINDOW_BYTES;

/// What the step did.
#[derive(Debug, Default)]
pub struct PromoteReport {
    /// (candidate id, suggestion id).
    pub promoted: Vec<(i64, i64)>,
    /// Suggestions filed or revised (the notice counts them).
    pub filed: Vec<i64>,
    /// Candidates whose promotion failed this run.
    pub failed: Vec<i64>,
    /// Candidates expired: (id, name).
    pub expired: Vec<(i64, String)>,
    /// Ready candidates left for the next run (the limit, or an error).
    pub remaining: usize,
    pub error: Option<String>,
    pub lines: Vec<String>,
}

impl PromoteReport {
    pub fn summary(&self) -> String {
        let promoted: Vec<String> = self.promoted.iter().map(|(c, s)| format!("candidate #{c} → #{s}")).collect();
        let mut s = format!(
            "candidates: {} promoted{}, {} failed, {} expired{}",
            self.promoted.len(),
            if promoted.is_empty() { String::new() } else { format!(" ({})", promoted.join(", ")) },
            self.failed.len(),
            self.expired.len(),
            crate::sweep::ids_named(&self.expired),
        );
        if self.remaining > 0 {
            s.push_str(&format!("; {} left for the next run", self.remaining));
        }
        if let Some(e) = &self.error {
            s.push_str(&format!("; error: {e}"));
        }
        s
    }
}

/// The evidence a promotion prompt shows: one per local date first (the
/// days are the reason it repeats), newest days first, then more of the
/// same days, at most [`MAX_EXCERPTS`].
fn pick(evidence: Vec<CandidateEvidence>) -> Vec<CandidateEvidence> {
    let mut by_day: Vec<(String, Vec<CandidateEvidence>)> = Vec::new();
    for e in evidence {
        match by_day.iter_mut().find(|(d, _)| *d == e.local_date) {
            Some((_, v)) => v.push(e),
            None => by_day.push((e.local_date.clone(), vec![e])),
        }
    }
    by_day.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = Vec::new();
    let mut round = 0;
    while out.len() < MAX_EXCERPTS {
        let mut any = false;
        for (_, v) in &by_day {
            if let Some(e) = v.get(round) {
                any = true;
                if out.len() < MAX_EXCERPTS {
                    out.push(e.clone());
                }
            }
        }
        if !any {
            break;
        }
        round += 1;
    }
    out.sort_by(|a, b| (&a.local_date, a.from_pos).cmp(&(&b.local_date, b.from_pos)));
    out
}

/// The rendered session part at `ev`'s positions, cut to `budget` bytes at
/// a line boundary; a note when it cannot be read any more.
async fn excerpt(env: &Env<'_>, ev: &CandidateEvidence, budget: usize) -> String {
    let rendered: Result<String> = async {
        let parts = match ev.source {
            Source::Transcript => {
                let path = env.transcripts_dir.join(format!("{}.jsonl", ev.session_id));
                if !path.exists() {
                    anyhow::bail!("the transcript no longer exists");
                }
                let read = tokio::task::spawn_blocking(move || transcript_events(&path, 0)).await??;
                parts::transcript_parts(&read.events, ev.from_pos, ev.to_pos, usize::MAX, &[])
            }
            Source::Index => {
                let Some(index) = env.index else { anyhow::bail!("there is no session index") };
                let turns = session_index::session_turns(index, &ev.session_id, 0).await?;
                parts::index_parts(&turns, ev.from_pos, ev.to_pos, usize::MAX)
            }
        };
        Ok(parts.into_iter().map(|p| p.text).collect::<Vec<_>>().join(""))
    }
    .await;
    match rendered {
        Ok(text) if !text.trim().is_empty() => review::budgeted(&text, budget),
        Ok(_) => "(this part has no conversation text)".into(),
        Err(e) => format!("(the part cannot be read: {e:#})"),
    }
}

/// Expire stale candidates, then promote at most `limit` ready ones. A dry
/// run lists what it would do and writes nothing.
pub async fn run(env: &Env<'_>, limit: u32, dry_run: bool, backend: &dyn Backend) -> Result<PromoteReport> {
    let mut report = PromoteReport::default();
    let cutoff = chrono::DateTime::from_timestamp(env.now - i64::from(env.cfg.candidate_expire_days) * 86_400, 0)
        .unwrap_or_default()
        .to_rfc3339();
    let cutoff = nucleus_core::timestamp::to_sortable(&cutoff);
    if dry_run {
        for c in candidates::open(env.pool).await? {
            if c.last_evidence_at < cutoff {
                report.lines.push(format!("expire  candidate #{} {} (last evidence {})", c.id, c.name, c.last_evidence_at));
            }
        }
    } else {
        report.expired = candidates::expire(env.pool, &cutoff).await?;
    }
    let ready = candidates::ready(env.pool, env.cfg.candidate_min_days).await?;
    for (i, c) in ready.iter().enumerate() {
        if i as u32 >= limit {
            report.remaining = ready.len() - i;
            break;
        }
        if dry_run {
            report.lines.push(format!("promote candidate #{} {} (seen on {})", c.id, c.name, c.days.join(", ")));
            continue;
        }
        // An error part-way keeps the report: the suggestions filed before
        // it are stored and still need their notice.
        let stop = match promote_one(env, c, backend, &mut report).await {
            Ok(Step::Done) => None,
            Ok(Step::Stop(e)) => Some(e),
            Err(e) => Some(format!("candidate #{}: {e:#}", c.id)),
        };
        if let Some(e) = stop {
            report.error = Some(e);
            report.remaining = ready.len() - i;
            break;
        }
    }
    Ok(report)
}

enum Step {
    Done,
    /// The reviewer could not be reached: the rest wait.
    Stop(String),
}

async fn promote_one(env: &Env<'_>, c: &Candidate, backend: &dyn Backend, report: &mut PromoteReport) -> Result<Step> {
    let evidence = pick(candidates::evidence(env.pool, c.id).await?);
    let budget = EXCERPTS_BUDGET / evidence.len().max(1);
    let mut excerpts = Vec::new();
    for ev in evidence {
        let text = excerpt(env, &ev, budget).await;
        excerpts.push(Excerpt { evidence: ev, text });
    }
    let run_id =
        ledger::start_run(env.pool, &RunStart { pass: "promote", candidate_id: Some(c.id), ..RunStart::default() }).await?;
    let lib = Library::load(env.pool, env.roots, (env.library_summary)()).await?;
    let task = Task::Promote { candidate: c, excerpts: &excerpts };
    match review::review(backend, env.pool, env.ws, env.roots, &lib, &task, SuggestionOrigin::Candidate).await {
        Ok(o) => {
            report.filed.extend(o.created.iter().map(|(id, _, _)| *id));
            report.filed.extend(o.revised.iter().map(|(_, new)| *new));
            let detail = review::describe(&o);
            ledger::finish_run(
                env.pool,
                run_id,
                "ok",
                o.review_session_id.as_deref(),
                o.created.len() as i64,
                o.evidence_for.len() as i64,
                Some(&detail),
            )
            .await?;
            match o.first_suggestion() {
                Some(sid) => {
                    candidates::mark_promoted(env.pool, c.id, sid).await?;
                    report.promoted.push((c.id, sid));
                    report.lines.push(format!("candidate #{} {}: {detail}", c.id, c.name));
                }
                None => {
                    let failures = candidates::record_failure(env.pool, c.id, &detail).await?;
                    report.failed.push(c.id);
                    report.lines.push(format!("candidate #{} {}: not promoted ({detail}; failure {failures})", c.id, c.name));
                }
            }
            Ok(Step::Done)
        }
        Err(ReviewError::Unparsable(e) | ReviewError::TooLarge(e)) => {
            ledger::finish_run(env.pool, run_id, "unparsable", None, 0, 0, Some(&e)).await?;
            let failures = candidates::record_failure(env.pool, c.id, &e).await?;
            report.failed.push(c.id);
            report.lines.push(format!("candidate #{} {}: unusable answer ({e}; failure {failures})", c.id, c.name));
            Ok(Step::Done)
        }
        Err(e @ ReviewError::Infra(_)) => {
            let msg = e.to_string();
            ledger::finish_run(env.pool, run_id, "failed", None, 0, 0, Some(&msg)).await?;
            Ok(Step::Stop(msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(date: &str, from: i64) -> CandidateEvidence {
        CandidateEvidence {
            session_id: format!("s-{date}-{from}"),
            source: Source::Transcript,
            agent: None,
            from_pos: from,
            to_pos: from + 1,
            local_date: date.into(),
            note: String::new(),
        }
    }

    #[test]
    fn excerpts_cover_every_day_first() {
        let mut all = Vec::new();
        for day in ["2026-01-01", "2026-01-02", "2026-01-03", "2026-01-04"] {
            for k in 0..3 {
                all.push(ev(day, k));
            }
        }
        let picked = pick(all);
        assert_eq!(picked.len(), MAX_EXCERPTS);
        let mut days: Vec<&str> = picked.iter().map(|e| e.local_date.as_str()).collect();
        days.dedup();
        assert_eq!(days, vec!["2026-01-01", "2026-01-02", "2026-01-03", "2026-01-04"], "every day is shown");
        assert_eq!(picked.iter().filter(|e| e.local_date == "2026-01-04").count(), 2, "then the newest days again");
    }
}
