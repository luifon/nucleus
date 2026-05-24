//! Daily inbox sweep — spawns one long-lived JARVIS session, has it
//! search / classify / label / trash via the Gmail MCP, then posts a
//! one-line digest to Discord.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use chrono_tz::Tz;
use croner::Cron;
use nucleus_core::{
    claude::PermissionMode,
    claude_session::{AskOptions, Session, SpawnOptions},
    config::{self, Settings},
    diary, discord_sdk,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use crate::store;

const AGENT_NAME: &str = "gmail-metabolism";
const DB_PATH: &str = "memory/gmail.db";
/// Max lookback for `newer_than:` Gmail search when no watermark is
/// recorded yet (first run). Older mail is left to manual review — the
/// job is for go-forward metabolism, not historical cleanup.
const FIRST_RUN_LOOKBACK_DAYS: i64 = 1;

/// Subset of the JSON tally JARVIS returns.
#[derive(Debug, Deserialize)]
struct Tally {
    counts: BTreeMap<String, i64>,
    trashed: i64,
    #[serde(default)]
    killlist_candidates: Vec<String>,
}

pub async fn run(settings: &Settings, workspace_root: &Path) -> Result<()> {
    let pool = store::open(&workspace_root.join(DB_PATH)).await?;

    let now = Utc::now();
    let watermark = store::read_watermark(&pool).await?;

    // Cron gate — launchd ticks this binary hourly; the cron decides
    // whether THIS tick should actually do the work. Same pattern as
    // reminders, picked deliberately to avoid `StartCalendarInterval`
    // (the launchd-TZ-bootstrap and codesign-cache pitfalls).
    if !cron_window_open(&settings.gmail.metabolism_cron, watermark, now)? {
        tracing::debug!(
            cron = %settings.gmail.metabolism_cron,
            ?watermark,
            "metabolism: cron window not open this tick; exiting"
        );
        return Ok(());
    }

    let lookback_from = watermark.unwrap_or_else(|| {
        now - ChronoDuration::days(FIRST_RUN_LOOKBACK_DAYS)
    });
    let killlist = store::killlist(&pool).await?;

    tracing::info!(
        watermark = %lookback_from.to_rfc3339(),
        killlist_size = killlist.len(),
        "metabolism: starting JARVIS run"
    );

    let persona = config::resolve_persona(&settings.identity, "gmail", None)
        .context("resolving Gmail persona (ADR-009)")?;
    // ${GMAIL_ACCOUNT} substitution stays here — resolve_persona handles only
    // ${USER_NAME}; per-venue placeholders remain venue-local.
    let persona = config::substitute_gmail(&persona.body, &settings.gmail);

    let prompt = build_prompt(
        &lookback_from.to_rfc3339(),
        &killlist,
        &settings.gmail.account,
    );

    let mut session = Session::spawn(SpawnOptions {
        workspace_root: workspace_root.to_path_buf(),
        append_system_prompt: Some(persona),
        permission_mode: Some(PermissionMode::Auto),
        disallowed_tools: settings.claude.disallowed_tools.clone(),
        // Pre-approve the Gmail MCP tools JARVIS needs to do the sweep
        // without per-call classifier prompting. List + read + label
        // ops are all expected; without this, the run stalls on the
        // first mutation.
        allowed_tools: vec![
            "mcp__claude_ai_Gmail__list_labels".into(),
            "mcp__claude_ai_Gmail__create_label".into(),
            "mcp__claude_ai_Gmail__search_threads".into(),
            "mcp__claude_ai_Gmail__get_thread".into(),
            "mcp__claude_ai_Gmail__label_thread".into(),
            "mcp__claude_ai_Gmail__unlabel_thread".into(),
        ],
        tmux_session: "nucleus-jarvis".into(),
        window_name: Some("metabolism".into()),
        ready_timeout: Duration::from_secs(20),
        agent_label: Some("gmail-metabolism".into()),
        ..SpawnOptions::default()
    })
    .await
    .context("spawning JARVIS session for inbox metabolism")?;

    let raw = session
        .ask(
            &prompt,
            AskOptions {
                // Bulk classification can run minutes; keep generous
                // ceiling. Idempotent: if we time out, watermark stays
                // put and the next 5am cron retries.
                max_wait: Duration::from_secs(60 * 8),
                quiescent_window: Duration::from_secs(5),
            },
        )
        .await;
    let _ = session.close().await;
    let raw = raw.context("JARVIS metabolism turn")?;

    let tally = parse_tally(&raw).with_context(|| {
        format!("could not parse JARVIS tally from reply: {raw}")
    })?;

    // Promote candidates whose junk-hit counter has crossed the threshold.
    let threshold = settings.gmail.killlist_auto_promote_threshold as i64;
    let mut promoted: Vec<String> = Vec::new();
    for sender in &tally.killlist_candidates {
        let hits = store::bump_junk_hit(&pool, sender).await?;
        if hits >= threshold {
            let promoted_now = store::killlist_add(
                &pool,
                sender,
                Some(&format!("auto-promoted after {hits} junk hits")),
                "classifier",
            )
            .await?;
            if promoted_now {
                promoted.push(sender.clone());
            }
        }
    }

    // Watermark advance — only on a clean parse; partial JARVIS runs
    // (timeout / parse-failure) leave the watermark in place so the
    // next tick re-scans the same window.
    store::write_watermark(&pool, now).await?;

    if !promoted.is_empty() {
        tracing::info!(?promoted, "metabolism: promoted senders to killlist");
    }

    post_digest(settings, &tally).await?;

    let _ = diary::record_observation(
        workspace_root,
        AGENT_NAME,
        "fired",
        &format!(
            "metabolized {} threads ({} trashed); promoted {} sender(s)",
            tally.counts.values().sum::<i64>(),
            tally.trashed,
            promoted.len()
        ),
        diary::Tag::Observation,
    );
    Ok(())
}

/// True when the next match of `cron_expr` after the prior watermark
/// (or 48h ago, if there's no prior run) is now in the past. This is
/// the same fire-late behavior the reminders ticker uses.
fn cron_window_open(
    cron_expr: &str,
    watermark: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<bool> {
    let tz = nucleus_tz();
    let baseline = watermark.unwrap_or(now - ChronoDuration::hours(48));
    let parsed = Cron::from_str(cron_expr)
        .map_err(|e| anyhow!("invalid metabolism_cron {:?}: {e}", cron_expr))?;
    let local = baseline.with_timezone(&tz);
    let next = parsed
        .find_next_occurrence(&local, false)
        .map_err(|e| anyhow!("metabolism_cron has no next match: {e}"))?;
    Ok(next.with_timezone(&Utc) <= now)
}

fn nucleus_tz() -> Tz {
    let candidates = [std::env::var("NUCLEUS_TZ").ok(), std::env::var("TZ").ok()];
    for c in candidates.iter().flatten() {
        if c.is_empty() {
            continue;
        }
        if let Ok(tz) = c.parse::<Tz>() {
            return tz;
        }
    }
    chrono_tz::America::Sao_Paulo
}

fn build_prompt(watermark_rfc3339: &str, killlist: &[String], account: &str) -> String {
    let killlist_json = serde_json::to_string(killlist).unwrap_or_else(|_| "[]".into());
    format!(
        r#"Run today's inbox metabolism on {account}.

LOOKBACK
  Walk every UNREAD thread newer than {watermark}.

LABEL TAXONOMY (pick exactly one per thread, create them if missing)
  nucleus/transactional       receipts, 2FA, order confirmations
  nucleus/newsletter/keep     high-signal newsletters worth reading
  nucleus/newsletter/skim     low-priority newsletters
  nucleus/human               from an actual person, not a domain
  nucleus/junk                mass-sent marketing / spam shape
  nucleus/review              you're uncertain — let me decide
  nucleus/unsubscribed        receipt of List-Unsubscribe action
If `mcp__claude_ai_Gmail__create_label` rejects nested names with `/`, fall
back to flat names like `nucleus-newsletter-keep` and use those throughout.

WORKFLOW
  1. Use `mcp__claude_ai_Gmail__list_labels` to see what already exists; create
     any taxonomy labels that are missing via `mcp__claude_ai_Gmail__create_label`.
  2. Use `mcp__claude_ai_Gmail__search_threads` with query
     `is:unread newer_than:{watermark}` to enumerate threads.
  3. For each thread, fetch its top-level details with
     `mcp__claude_ai_Gmail__get_thread`, classify into ONE label, and apply
     via `mcp__claude_ai_Gmail__label_thread`.
  4. TRASH (apply system label `TRASH` via `mcp__claude_ai_Gmail__label_thread`)
     when EITHER:
       a) the From-address is in this kill-list: {killlist_json}
       b) classification is `nucleus/junk` AND your confidence is high
     Senders you marked junk that are NOT in the kill-list go in
     `killlist_candidates` so the loop can auto-promote repeat offenders.

OUTPUT (REPLY WITH ONLY THIS JSON, NO PROSE, NO MARKDOWN FENCES)
  {{
    "counts": {{
      "transactional": N,
      "newsletter/keep": N,
      "newsletter/skim": N,
      "human": N,
      "junk": N,
      "review": N,
      "unsubscribed": N
    }},
    "trashed": N,
    "killlist_candidates": ["sender@example.com", ...]
  }}

If zero unread threads matched, return all-zero counts. Do not invent counts.
"#,
        account = account,
        watermark = watermark_rfc3339,
        killlist_json = killlist_json,
    )
}

fn parse_tally(raw: &str) -> Result<Tally> {
    // JARVIS may bracket the JSON in fences or stray prose. Slice the
    // outermost balanced object.
    let cleaned = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let start = cleaned
        .find('{')
        .ok_or_else(|| anyhow!("no JSON object in reply"))?;
    let end = cleaned
        .rfind('}')
        .ok_or_else(|| anyhow!("no closing brace in reply"))?;
    let slice = &cleaned[start..=end];
    let tally: Tally = serde_json::from_str(slice)
        .with_context(|| format!("parsing tally JSON slice: {slice}"))?;
    Ok(tally)
}

async fn post_digest(settings: &Settings, tally: &Tally) -> Result<()> {
    if settings.discord.home_channel_id.is_empty() {
        tracing::warn!("DISCORD_HOME_CHANNEL_ID not set; skipping digest");
        return Ok(());
    }

    let total: i64 = tally.counts.values().sum();
    if total == 0 && tally.trashed == 0 {
        tracing::info!("metabolism: nothing to report; skipping digest");
        return Ok(());
    }

    // One-line digest per ADR — mention the user only if any human mail landed.
    let human = tally.counts.get("human").copied().unwrap_or(0);
    let prefix = if human > 0 {
        settings
            .discord
            .allowed_user_ids
            .first()
            .map(|id| format!("<@{}> ", id))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let parts: Vec<String> = ["transactional", "newsletter/keep", "newsletter/skim", "human", "junk", "review", "unsubscribed"]
        .iter()
        .filter_map(|key| {
            let n = tally.counts.get(*key).copied().unwrap_or(0);
            (n > 0).then(|| format!("{n} {key}"))
        })
        .collect();
    let body_summary = if parts.is_empty() {
        "no unread mail".to_string()
    } else {
        parts.join(", ")
    };
    let trashed_suffix = if tally.trashed > 0 {
        format!(" → {} trashed", tally.trashed)
    } else {
        String::new()
    };

    let body = format!(
        "{}▸ overnight email: {}{}",
        prefix, body_summary, trashed_suffix
    );
    discord_sdk::send_announcement(&settings.discord.home_channel_id, &body)
        .await
        .map(|_| ())
}
