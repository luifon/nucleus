//! The two Claude calls a run makes (ADR-031): rank every item against the
//! reader's profile, then write one short brief over what survived.
//!
//! Both go through `SessionProfile::one_shot_utility` — never `claude -p`.

use anyhow::{bail, Context, Result};
use nucleus_core::{
    config::Settings,
    session_profile::{ProfileContext, SessionProfile},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::feed::ParsedItem;

/// Bump when the prompt changes. Stored per item so a later "why was this
/// scored that way" has the prompt generation to hand.
pub const PROMPT_VERSION: &str = "2026-09-13-profile-v1";

/// One call covers a normal day's batch. Beyond this the payload starts
/// competing with the profile for the model's attention, so we chunk —
/// each chunk is validated on its own.
pub const RANK_BATCH_SIZE: usize = 60;

const TMUX_SESSION: &str = "nucleus-news-fetcher";
const AGENT_LABEL: &str = "news-fetcher";

/// The reader profile the ranker judges against, plus the hash recorded on
/// every item it scored.
pub struct Profile {
    pub text: String,
    pub hash: String,
}

/// Read the operator's profile note. A missing or empty note is a hard
/// failure: the note is the entire ranking basis, and a run without it would
/// silently fall back to generic tech-news taste — exactly the monoculture
/// this design replaced.
pub fn load_profile(vault_dir: &Path, note_rel: &str) -> Result<Profile> {
    let path = vault_dir.join(note_rel);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading the news profile note at {}", path.display()))?;
    if text.trim().is_empty() {
        bail!("the news profile note at {} is empty", path.display());
    }
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let hash = hex::encode(&h.finalize()[..8]);
    Ok(Profile { text, hash })
}

/// One item's ranking as the model returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct RankedItem {
    pub id: String,
    pub score: f64,
    pub reason: String,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub stale: bool,
}

/// Rank every item. Returns exactly one result per input, in no particular
/// order. Any batch that fails validation twice fails the whole run — a
/// partial ranking would surface a partial day, which is worse than none.
pub async fn rank_items(
    workspace_root: &PathBuf,
    settings: &Settings,
    profile: &Profile,
    items: &[ParsedItem],
) -> Result<Vec<RankedItem>> {
    let mut out = Vec::with_capacity(items.len());
    for (n, chunk) in items.chunks(RANK_BATCH_SIZE).enumerate() {
        let prompt = ranking_prompt(profile, chunk)?;
        let batch = match run_ranking_batch(workspace_root, settings, &prompt, chunk).await {
            Ok(b) => b,
            Err(first) => {
                tracing::warn!(batch = n, error = %first, "ranking batch failed validation — retrying once");
                run_ranking_batch(workspace_root, settings, &prompt, chunk)
                    .await
                    .with_context(|| format!("ranking batch {n} failed twice"))?
            }
        };
        out.extend(batch);
    }
    Ok(out)
}

async fn run_ranking_batch(
    workspace_root: &PathBuf,
    settings: &Settings,
    prompt: &str,
    chunk: &[ParsedItem],
) -> Result<Vec<RankedItem>> {
    let outcome = SessionProfile::one_shot_utility(&ProfileContext {
        workspace_root,
        claude: &settings.claude,
        tmux_session: TMUX_SESSION,
        agent_label: AGENT_LABEL,
    })
    .window_name("rank")
    .run_one_shot(prompt)
    .await
    .context("ranking session")?;

    let parsed: Vec<RankedItem> = serde_json::from_str(&strip_fences(&outcome.reply))
        .with_context(|| format!("ranking reply was not a JSON array: {}", truncate(&outcome.reply, 400)))?;
    validate(&parsed, chunk)?;
    Ok(parsed)
}

/// Exactly one result per input id, ids matching, scores in range, reasons
/// present. Everything downstream assumes this holds.
fn validate(parsed: &[RankedItem], chunk: &[ParsedItem]) -> Result<()> {
    let expected: HashSet<&str> = chunk.iter().map(|i| i.id.as_str()).collect();
    if parsed.len() != chunk.len() {
        bail!("expected {} results, got {}", chunk.len(), parsed.len());
    }
    let mut seen: HashSet<&str> = HashSet::with_capacity(parsed.len());
    for r in parsed {
        if !expected.contains(r.id.as_str()) {
            bail!("result for unknown id {:?}", r.id);
        }
        if !seen.insert(r.id.as_str()) {
            bail!("duplicate result for id {:?}", r.id);
        }
        if !r.score.is_finite() || !(0.0..=1.0).contains(&r.score) {
            bail!("score {} for id {:?} is outside [0,1]", r.score, r.id);
        }
        if r.reason.trim().is_empty() {
            bail!("empty reason for id {:?}", r.id);
        }
    }
    Ok(())
}

fn ranking_prompt(profile: &Profile, items: &[ParsedItem]) -> Result<String> {
    let payload: Vec<serde_json::Value> = items
        .iter()
        .map(|i| {
            serde_json::json!({
                "id": i.id,
                "title": i.title,
                "source": i.source_name,
                "domain": i.domain(),
                "published_at": i.published_at,
                "summary": i.summary.clone().unwrap_or_default(),
            })
        })
        .collect();

    Ok(format!(
        r#"You are ranking today's tech-news items for one specific reader.

Here is a profile of the reader:

---
{profile}
---

For each item below, return one object. Reply with a JSON array and nothing
else — no prose, no explanation, no markdown fences:

  {{"id": "<id>", "score": <float 0..1>, "reason": "<one short clause>",
    "event": "<short-kebab-slug>", "stale": <true|false>}}

How to score:

- `score` is how relevant this item is to THIS reader, judged against the
  profile above. Not general importance, not how much the tech world is
  talking about it.
- There are no topic caps. Two different high-impact items about the same
  product both score high — a model release and a change to that product's
  usage limits are different news and the reader wants both.
- Breadth is still an objective. When several items are near-duplicates in
  substance (same product, same news angle, nothing new in the second one),
  prefer coverage across more of the profile's interests over stacking one
  topic, and say so in `reason` for the ones you push down.
- `reason` is one short clause explaining the score to the reader. It is
  shown to them, so write it for them, not for a log.
- `event` is a short kebab-case slug naming the underlying event
  (`opus-5-release`, `cloudflare-outage`). Items covering the same event
  should share a slug. It is a display label only.
- `stale` is true when the item is old content resurfacing rather than
  news: an essay from years ago submitted to an aggregator today, a
  re-announcement, a link roundup of old material. The `published_at` field
  on aggregator sources (Hacker News, lobste.rs) is the SUBMISSION time, not
  the article's date — judge the article's age from the title, the summary
  and the domain, not from that timestamp. If the profile makes old content
  newly relevant to this reader, set `stale` false and say why in `reason`.

Items:
{items}
"#,
        profile = profile.text.trim(),
        items = serde_json::to_string_pretty(&payload)?,
    ))
}

/// What the brief writer gets: one surfaced item, flattened.
pub struct BriefInput<'a> {
    pub title: &'a str,
    pub source: &'a str,
    pub score: f64,
    pub reason: &'a str,
    pub event: &'a str,
}

/// Write the one-paragraph state-of-things the widget shows above the list.
/// Errors are the caller's to absorb — a failed brief must not fail the run.
pub async fn write_brief(
    workspace_root: &PathBuf,
    settings: &Settings,
    profile: &Profile,
    items: &[BriefInput<'_>],
) -> Result<String> {
    if items.is_empty() {
        bail!("no surfaced items to brief");
    }
    let listed: Vec<serde_json::Value> = items
        .iter()
        .map(|i| {
            serde_json::json!({
                "title": i.title,
                "source": i.source,
                "score": i.score,
                "reason": i.reason,
                "event": i.event,
            })
        })
        .collect();

    let prompt = format!(
        r#"You are writing the daily news brief for one specific reader.

Here is a profile of the reader:

---
{profile}
---

These are the items that made it through to them today, already ranked:

{items}

Write 2 to 4 sentences, at most about 60 words, in English, that tell the
reader the state of things today and the one to three points that matter
most, naming the items you mean. Plain text only: no preamble, no greeting,
no markdown, no bullet list, no closing line. Reply with the brief itself and
nothing else."#,
        profile = profile.text.trim(),
        items = serde_json::to_string_pretty(&listed)?,
    );

    let outcome = SessionProfile::one_shot_utility(&ProfileContext {
        workspace_root,
        claude: &settings.claude,
        tmux_session: TMUX_SESSION,
        agent_label: AGENT_LABEL,
    })
    .window_name("brief")
    .run_one_shot(&prompt)
    .await
    .context("brief session")?;

    let text = strip_fences(&outcome.reply).trim().to_string();
    if text.is_empty() {
        bail!("brief session returned nothing");
    }
    Ok(text)
}

fn strip_fences(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
        .to_string()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn item(id: &str) -> ParsedItem {
        let now = Utc::now();
        ParsedItem {
            id: id.into(),
            source_id: 1,
            source_name: "Hacker News".into(),
            url: "https://example.com/x".into(),
            article_url: None,
            canonical_url: "https://example.com/x".into(),
            title: "t".into(),
            summary: None,
            published: now,
            published_at: now.to_rfc3339(),
            published_date: now.format("%Y-%m-%d").to_string(),
        }
    }

    fn ranked(id: &str, score: f64, reason: &str) -> RankedItem {
        RankedItem { id: id.into(), score, reason: reason.into(), event: "e".into(), stale: false }
    }

    #[test]
    fn accepts_a_complete_matching_batch() {
        let chunk = [item("a"), item("b")];
        let got = [ranked("a", 0.9, "fits"), ranked("b", 0.0, "no")];
        assert!(validate(&got, &chunk).is_ok());
    }

    #[test]
    fn rejects_missing_dropped_and_unknown_ids() {
        let chunk = [item("a"), item("b")];
        assert!(validate(&[ranked("a", 0.5, "x")], &chunk).is_err(), "short batch");
        assert!(
            validate(&[ranked("a", 0.5, "x"), ranked("c", 0.5, "x")], &chunk).is_err(),
            "unknown id"
        );
        assert!(
            validate(&[ranked("a", 0.5, "x"), ranked("a", 0.5, "x")], &chunk).is_err(),
            "duplicate id"
        );
    }

    #[test]
    fn rejects_out_of_range_scores_and_empty_reasons() {
        let chunk = [item("a")];
        assert!(validate(&[ranked("a", 1.5, "x")], &chunk).is_err());
        assert!(validate(&[ranked("a", -0.1, "x")], &chunk).is_err());
        assert!(validate(&[ranked("a", f64::NAN, "x")], &chunk).is_err());
        assert!(validate(&[ranked("a", 0.5, "  ")], &chunk).is_err());
    }

    #[test]
    fn fences_are_stripped() {
        assert_eq!(strip_fences("```json\n[1]\n```"), "[1]");
        assert_eq!(strip_fences("  [1]  "), "[1]");
    }
}
