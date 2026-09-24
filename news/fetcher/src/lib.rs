//! news-fetcher — twice-daily news pull, ranked against a profile of the
//! reader and delivered to the notch widget (ADR-031).
//!
//! A run is: ingest the widget's outboxes → fetch every enabled feed → drop
//! anything stale or duplicated (mechanically, no model involved) → ask
//! Claude to rank what's left against the operator's profile note → ask it
//! for a two-sentence brief over what he hasn't rejected → write `news.json`
//! for the widget. Nothing is posted anywhere.
//!
//! The run either produces a complete day or leaves yesterday's `news.json`
//! untouched. There is no partial write.

mod canonical;
mod feed;
mod rank;
mod store;
mod widget;

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use nucleus_core::{config::Settings, db, diary};
use sqlx::SqlitePool;
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

use canonical::{same_event, title_tokens};
use feed::ParsedItem;
use store::{Ranking, RunDiagnostics};

const AGENT_NAME: &str = "news-fetcher";
const DB_PATH: &str = "memory/news.db";

/// An item is news for this long after it was published. Aggregator entries
/// are timestamped at submission, so this is a freshness bound on the feed
/// entry, not on the underlying article — the ranker judges article age.
const FRESHNESS_HOURS: i64 = 48;

/// How far back event suppression looks for something we already showed.
const SUPPRESSION_DAYS: i64 = 7;

/// The widget shows one rolling day, which spans both of a day's runs.
const SURFACE_WINDOW_HOURS: i64 = 24;

const USAGE: &str = "\
nucleus news-fetcher — pull feeds, rank them against the reader profile, and
write the widget feed.

Usage:
  nucleus news-fetcher                 run the full pipeline
  nucleus news-fetcher --ingest        drain the widget outboxes and exit
                                       (--ingest-votes is the old name for it)
";

/// Entry point for this subcommand of the `nucleus` binary.
pub async fn run(args: Vec<std::ffi::OsString>) -> Result<()> {
    let flags: Vec<String> = args.iter().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    let mut ingest_only = false;
    for flag in &flags {
        match flag.as_str() {
            // `--ingest-votes` predates the opens outbox and is documented in
            // ADR-031, so it keeps working; it now drains both.
            "--ingest" | "--ingest-votes" => ingest_only = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument: {other}\n\n{USAGE}"),
        }
    }

    nucleus_core::init_tracing();
    let settings = Settings::load().context("loading nucleus.toml + .env")?;
    let workspace_root = settings.workspace_root()?;
    let pool = db::open(&workspace_root.join(DB_PATH)).await?;

    nucleus_core::migrate::migrate(&pool, store::MIGRATIONS)
        .await
        .context("migrating news.db")?;
    // Data seeding, not schema — re-applied every boot so a change to the
    // default source list takes effect without hand-editing the DB.
    store::seed_sources(&pool).await?;

    let feed_dir = settings.news.widget_feed_path();

    // The outboxes come back before anything else, so today's feed carries the
    // reader's latest verdicts — and so the brief knows what he has rejected
    // before it is written. `--ingest` is the same code, stopped early.
    let ingested = widget::ingest_outboxes(&pool, &feed_dir).await?;
    if ingest_only {
        tracing::info!(ingested, "outbox ingest only — exiting");
        return Ok(());
    }

    // A stuck window from a previous run would make the next session attach
    // to a dead pane instead of spawning.
    let _ = tokio::process::Command::new("tmux")
        .args(["kill-session", "-t", "nucleus-news-fetcher"])
        .output()
        .await;

    let run_id = uuid::Uuid::new_v4().to_string();
    store::record_run_start(&pool, &run_id).await?;

    let mut diag = RunDiagnostics::default();
    match pipeline(&pool, &workspace_root, &settings, &run_id, &mut diag).await {
        Ok(()) => {
            store::record_run_finish(&pool, &run_id, &diag, None).await?;
            tracing::info!(
                input = diag.items_input,
                stale = diag.rejected_stale,
                dup_url = diag.rejected_dup_url,
                dup_title = diag.rejected_dup_title,
                ranked = diag.items_ranked,
                surfaced = diag.items_surfaced,
                brief_ok = diag.brief_ok,
                "run complete"
            );
            let _ = diary::record_observation(
                &workspace_root,
                AGENT_NAME,
                "fetcher run",
                &format!(
                    "ok: {} fetched, {} stale, {} dup-url, {} dup-title, {} ranked, {} surfaced, brief {}",
                    diag.items_input,
                    diag.rejected_stale,
                    diag.rejected_dup_url,
                    diag.rejected_dup_title,
                    diag.items_ranked,
                    diag.items_surfaced,
                    if diag.brief_ok { "ok" } else { "reused" },
                ),
                diary::Tag::Routine,
            );
            Ok(())
        }
        Err(e) => {
            // The existing news.json is deliberately left alone — the widget
            // keeps showing the last good day rather than an empty one.
            let msg = format!("{e:#}");
            store::record_run_finish(&pool, &run_id, &diag, Some(&msg)).await?;
            tracing::error!(error = %msg, "run failed — widget feed left untouched");
            let _ = diary::record_observation(
                &workspace_root,
                AGENT_NAME,
                "fetcher run failed",
                &msg,
                diary::Tag::Routine,
            );
            Err(e)
        }
    }
}

async fn pipeline(
    pool: &SqlitePool,
    workspace_root: &PathBuf,
    settings: &Settings,
    run_id: &str,
    diag: &mut RunDiagnostics,
) -> Result<()> {
    // The profile is the whole rubric. Load it before spending a single
    // network request, so a missing note fails in a second rather than after
    // a full fetch.
    let profile = rank::load_profile(
        &settings.obsidian.vault_dir(),
        &settings.news.profile_note,
    )?;
    diag.profile_hash = profile.hash.clone();

    let sources = store::enabled_sources(pool).await?;
    tracing::info!(count = sources.len(), "pulling sources");

    let http = feed::http_client()?;
    let mut fetched: Vec<ParsedItem> = Vec::new();
    for src in &sources {
        match feed::fetch_source(&http, src).await {
            Ok(items) => {
                tracing::info!(source = %src.name, parsed = items.len(), "fetched");
                store::mark_source_ok(pool, src.id).await?;
                fetched.extend(items);
            }
            Err(e) => {
                tracing::warn!(source = %src.name, error = %e, "source failed");
                store::mark_source_error(pool, src.id, &e.to_string()).await?;
            }
        }
    }
    if let Some(re) = settings.news.exclude_regex() {
        fetched.retain(|it| !matches_excluded_topic(it, &re));
    }
    diag.items_input = fetched.len();

    let known_urls = store::known_canonical_urls(pool).await?;
    let recent_titles = store::recent_titles(pool, SUPPRESSION_DAYS).await?;
    let fresh = select_new_items(fetched, &known_urls, &recent_titles, diag);

    // A run that finds nothing new still rebuilds the feed. The surfaced set
    // is a rolling 24h window, not this run's catch, so the evening run
    // refreshes the day — and a brief that needs rewriting (too long, or a
    // session that failed this morning) gets another chance without waiting
    // for new items to show up.
    if fresh.is_empty() {
        tracing::info!("nothing new survived freshness and dedup — refreshing the feed in place");
    } else {
        tracing::info!(count = fresh.len(), "ranking");
        store::insert_items(pool, &fresh).await?;

        let ranked = rank::rank_items(workspace_root, settings, &profile, &fresh).await?;
        diag.items_ranked = ranked.len();
        let rankings: Vec<Ranking<'_>> = ranked
            .iter()
            .map(|r| Ranking {
                item_id: &r.id,
                score: r.score,
                reason: r.reason.trim(),
                event_slug: r.event.trim(),
                stale: r.stale,
            })
            .collect();
        store::save_rankings(pool, &rankings, &profile.hash, rank::PROMPT_VERSION).await?;
    }

    let surfaced = group_same_event_adjacent(
        store::surfaced_items(
            pool,
            settings.news.min_score,
            Duration::hours(SURFACE_WINDOW_HOURS),
        )
        .await?,
    );
    diag.items_surfaced = surfaced.len();

    if surfaced.is_empty() {
        tracing::warn!(
            min_score = settings.news.min_score,
            "nothing cleared the score floor — widget feed left untouched"
        );
        return Ok(());
    }

    let brief = write_brief(pool, workspace_root, settings, &profile, &surfaced, run_id, diag).await?;

    let feed = widget::Feed {
        as_of: chrono::Local::now().to_rfc3339(),
        brief,
        count: surfaced.len(),
        items: surfaced
            .iter()
            .map(|r| widget::FeedItem {
                id: r.id.clone(),
                title: r.title.clone(),
                source: r.source_name.clone(),
                url: r.canonical_url.clone(),
                published_at: r.published_at.clone(),
                score: r.notable_score,
                reason: r.notable_reason.clone(),
                event: r.event_slug.clone(),
                vote: r.vote,
                vote_reason: r.vote_reason.clone(),
                vote_note: r.vote_note.clone(),
                opened: r.opened,
            })
            .collect(),
    };
    widget::write_feed(&settings.news.widget_feed_path(), &feed)?;
    tracing::info!(count = feed.count, "wrote widget feed");
    Ok(())
}

/// Write the day's brief over the items the reader hasn't rejected.
///
/// A downvote is the clearest instruction the widget can send, and a brief
/// that goes on to recommend the downvoted item reads as the system ignoring
/// it. So downvoted items are not brief inputs, and a stored brief is only
/// reusable while none of the items it was written from has been downvoted
/// since. When neither is available the day ships with an empty brief — the
/// items are the product, the sentence is the framing.
async fn write_brief(
    pool: &SqlitePool,
    workspace_root: &PathBuf,
    settings: &Settings,
    profile: &rank::Profile,
    surfaced: &[store::SurfacedRow],
    run_id: &str,
    diag: &mut RunDiagnostics,
) -> Result<String> {
    let accepted: Vec<&store::SurfacedRow> = surfaced.iter().filter(|r| r.vote != -1).collect();
    let excluded = surfaced.len() - accepted.len();
    if excluded > 0 {
        tracing::info!(excluded, "downvoted items withheld from the brief");
    }
    if accepted.is_empty() {
        // Every surfaced item is one he rejected. There is nothing to say
        // about the day that isn't a contradiction.
        tracing::warn!("every surfaced item is downvoted — writing an empty brief");
        return Ok(String::new());
    }

    let inputs: Vec<rank::BriefInput<'_>> = accepted
        .iter()
        .map(|r| rank::BriefInput {
            title: &r.title,
            source: &r.source_name,
            score: r.notable_score,
            reason: &r.notable_reason,
            event: &r.event_slug,
        })
        .collect();

    match rank::write_brief(workspace_root, settings, profile, &inputs).await {
        Ok(text) => {
            diag.brief_ok = true;
            let ids: Vec<String> = accepted.iter().map(|r| r.id.clone()).collect();
            store::save_brief(pool, run_id, &text, &ids).await?;
            Ok(text)
        }
        Err(e) => {
            if matches!(e, rank::BriefFailure::TooLong { .. }) {
                diag.brief_too_long = true;
            }
            match store::last_reusable_brief(pool).await? {
                store::BriefFallback::Reuse(text) => {
                    tracing::warn!(error = %e, "brief unusable — reusing the last one");
                    Ok(text)
                }
                store::BriefFallback::Blocked => {
                    diag.brief_dropped_downvoted = true;
                    tracing::warn!(
                        error = %e,
                        "brief unusable and the stored one names a downvoted item — shipping no brief"
                    );
                    Ok(String::new())
                }
                store::BriefFallback::Empty => {
                    tracing::warn!(error = %e, "brief unusable and nothing stored to reuse");
                    Ok(String::new())
                }
            }
        }
    }
}

/// Pull same-event items together without reordering the events themselves.
///
/// Two write-ups of one story that both survive dedup are still two items —
/// ADR-031 is explicit that a slug never collapses anything — but showing them
/// six rows apart reads as two separate stories. Each event keeps the position
/// of its highest-scoring item; the rest of its items follow immediately, in
/// the order they already had.
fn group_same_event_adjacent(rows: Vec<store::SurfacedRow>) -> Vec<store::SurfacedRow> {
    let mut out: Vec<store::SurfacedRow> = Vec::with_capacity(rows.len());
    let mut placed = vec![false; rows.len()];
    for i in 0..rows.len() {
        if placed[i] {
            continue;
        }
        placed[i] = true;
        let slug = rows[i].event_slug.clone();
        out.push(rows[i].clone());
        if slug.is_empty() {
            continue;
        }
        for (j, row) in rows.iter().enumerate().skip(i + 1) {
            if !placed[j] && row.event_slug == slug {
                placed[j] = true;
                out.push(row.clone());
            }
        }
    }
    out
}

/// Everything mechanical that decides what reaches the ranker: the 48h
/// freshness bound, canonical-URL identity, and title-overlap event
/// suppression against both this run and the last week.
///
/// Kept pure and separate from the pipeline so it can be reasoned about — and
/// tested — without a database or a network.
fn matches_excluded_topic(item: &ParsedItem, pattern: &regex::Regex) -> bool {
    pattern.is_match(&item.title)
        || item.summary.as_deref().is_some_and(|s| pattern.is_match(s))
}

fn select_new_items(
    fetched: Vec<ParsedItem>,
    known_urls: &[String],
    recent_titles: &[String],
    diag: &mut RunDiagnostics,
) -> Vec<ParsedItem> {
    let cutoff = Utc::now() - Duration::hours(FRESHNESS_HOURS);
    let known: HashSet<&str> = known_urls.iter().map(|s| s.as_str()).collect();
    let mut seen_urls: HashSet<String> = HashSet::new();
    let mut seen_titles: Vec<BTreeSet<String>> =
        recent_titles.iter().map(|t| title_tokens(t)).collect();

    // Newest first, so when two entries describe the same event we keep the
    // more recent one.
    let mut candidates = fetched;
    candidates.sort_by(|a, b| b.published.cmp(&a.published));

    let mut kept = Vec::new();
    for item in candidates {
        if item.published < cutoff {
            diag.rejected_stale += 1;
            continue;
        }
        if known.contains(item.canonical_url.as_str()) || !seen_urls.insert(item.canonical_url.clone())
        {
            diag.rejected_dup_url += 1;
            continue;
        }
        let tokens = title_tokens(&item.title);
        if seen_titles.iter().any(|prev| same_event(&tokens, prev)) {
            diag.rejected_dup_title += 1;
            continue;
        }
        seen_titles.push(tokens);
        kept.push(item);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use canonical::canonicalize;
    use chrono::DateTime;

    #[test]
    fn excluded_topic_matches_title_and_summary() {
        let re = regex::Regex::new(
            r"(?i)\brust\b|rust-?lang|crates\.io|\brustc\b|\bclippy\b|\bcargo\b",
        )
        .unwrap();
        let mut rusty = item("Stabilizing Rust's never type", "https://ex.com/a", 1);
        assert!(matches_excluded_topic(&rusty, &re));
        rusty.title = "A tool published to crates.io".into();
        assert!(matches_excluded_topic(&rusty, &re));

        let mut agentic = item("Claude Opus 5 ships agentic coding", "https://ex.com/b", 1);
        assert!(!matches_excluded_topic(&agentic, &re));
        agentic.summary = Some("Uses cargo cults as a metaphor".into());
        assert!(matches_excluded_topic(&agentic, &re));
    }

    fn item(title: &str, url: &str, age_hours: i64) -> ParsedItem {
        let published = Utc::now() - Duration::hours(age_hours);
        let canonical_url = canonicalize(url);
        ParsedItem {
            id: feed::item_id(&canonical_url),
            source_id: 1,
            source_name: "Hacker News".into(),
            url: url.into(),
            article_url: None,
            canonical_url,
            title: title.into(),
            summary: None,
            published,
            published_at: published.to_rfc3339(),
            published_date: published.format("%Y-%m-%d").to_string(),
        }
    }

    #[test]
    fn drops_items_past_the_freshness_bound() {
        let mut diag = RunDiagnostics::default();
        let kept = select_new_items(
            vec![
                item("Fresh thing", "https://a.dev/1", 2),
                item("Old thing", "https://a.dev/2", FRESHNESS_HOURS + 1),
            ],
            &[],
            &[],
            &mut diag,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(diag.rejected_stale, 1);
        assert_eq!(kept[0].title, "Fresh thing");
    }

    #[test]
    fn collapses_the_same_article_arriving_via_two_aggregators() {
        let mut diag = RunDiagnostics::default();
        let kept = select_new_items(
            vec![
                item("Post about agents", "https://a.dev/post?utm_source=hn", 1),
                item("Agents, a post", "https://www.a.dev/post/", 2),
            ],
            &[],
            &[],
            &mut diag,
        );
        assert_eq!(kept.len(), 1, "canonical URL is the identity");
        assert_eq!(diag.rejected_dup_url, 1);
    }

    #[test]
    fn suppresses_a_reworded_headline_for_the_same_event() {
        let mut diag = RunDiagnostics::default();
        let kept = select_new_items(
            vec![
                item("Cloudflare outage takes down half the internet", "https://a.dev/1", 1),
                item("Half the internet down in Cloudflare outage", "https://b.dev/2", 3),
            ],
            &[],
            &[],
            &mut diag,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(diag.rejected_dup_title, 1);
        assert_eq!(kept[0].title, "Cloudflare outage takes down half the internet", "newest wins");
    }

    #[test]
    fn keeps_two_different_stories_about_one_product() {
        let mut diag = RunDiagnostics::default();
        let kept = select_new_items(
            vec![
                item("Anthropic releases Claude Opus 5", "https://a.dev/1", 1),
                item("Anthropic tightens weekly rate limits for Max", "https://b.dev/2", 2),
            ],
            &[],
            &[],
            &mut diag,
        );
        assert_eq!(kept.len(), 2, "no topic caps — both are news");
        assert_eq!(diag.rejected_dup_title, 0);
    }

    #[test]
    fn suppresses_against_history_as_well_as_within_the_run() {
        let mut diag = RunDiagnostics::default();
        let kept = select_new_items(
            vec![item("Cloudflare outage takes down half the internet", "https://a.dev/1", 1)],
            &[],
            &["Half the internet down in Cloudflare outage".to_string()],
            &mut diag,
        );
        assert!(kept.is_empty());
        assert_eq!(diag.rejected_dup_title, 1);
    }

    #[test]
    fn a_url_already_in_the_database_is_not_refetched() {
        let mut diag = RunDiagnostics::default();
        let known = vec![canonicalize("https://a.dev/post")];
        let kept = select_new_items(
            vec![item("Something", "https://www.a.dev/post/?utm_medium=rss", 1)],
            &known,
            &[],
            &mut diag,
        );
        assert!(kept.is_empty());
        assert_eq!(diag.rejected_dup_url, 1);
    }

    fn surfaced(id: &str, score: f64, event: &str, vote: i64) -> store::SurfacedRow {
        store::SurfacedRow {
            id: id.into(),
            title: format!("title {id}"),
            source_name: "Hacker News".into(),
            canonical_url: format!("https://example.com/{id}"),
            published_at: "2026-09-13T09:00:00.000Z".into(),
            notable_score: score,
            notable_reason: "r".into(),
            event_slug: event.into(),
            vote,
            vote_reason: None,
            vote_note: None,
            opened: false,
        }
    }

    fn ids(rows: &[store::SurfacedRow]) -> Vec<&str> {
        rows.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn same_event_items_are_pulled_under_the_higher_scoring_one() {
        // The RubyGems shape: two write-ups of one story, 0.72 and 0.50, with
        // four unrelated items scored between them.
        let rows = vec![
            surfaced("homebrew", 0.82, "homebrew-7-release", 0),
            surfaced("rubygems-a", 0.72, "rubygems-agent-attack", 0),
            surfaced("fde", 0.68, "forward-deployed-engineer", 0),
            surfaced("openrouter", 0.62, "openrouter-pitfalls", 0),
            surfaced("rubygems-b", 0.50, "rubygems-agent-attack", 0),
        ];
        assert_eq!(
            ids(&group_same_event_adjacent(rows)),
            ["homebrew", "rubygems-a", "rubygems-b", "fde", "openrouter"],
        );
    }

    #[test]
    fn grouping_keeps_every_item_and_leaves_distinct_events_in_score_order() {
        let rows = vec![
            surfaced("a", 0.9, "one", 0),
            surfaced("b", 0.8, "two", 0),
            surfaced("c", 0.7, "three", 0),
        ];
        let grouped = group_same_event_adjacent(rows.clone());
        assert_eq!(ids(&grouped), ["a", "b", "c"]);
        assert_eq!(grouped.len(), rows.len(), "nothing is collapsed by slug");
    }

    #[test]
    fn an_empty_slug_groups_with_nothing() {
        // Pre-ADR rows and anything the ranker left blank must not all clump.
        let rows = vec![
            surfaced("a", 0.9, "", 0),
            surfaced("b", 0.8, "real", 0),
            surfaced("c", 0.7, "", 0),
        ];
        assert_eq!(ids(&group_same_event_adjacent(rows)), ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn the_brief_is_written_over_what_the_reader_has_not_rejected() {
        let f = store::testdb::fixture().await;
        for id in ["keep", "reject"] {
            store::testdb::add_item(&f.pool, id).await;
        }
        sqlx::query("INSERT INTO fetcher_runs (run_id, started_at) VALUES ('r1', '2026-09-13T09:00:00.000Z')")
            .execute(&f.pool)
            .await
            .unwrap();
        store::save_brief(&f.pool, "r1", "Yesterday's sentence about reject.", &["reject".into()])
            .await
            .unwrap();
        store::insert_vote(
            &f.pool,
            &store::IncomingVote {
                vote_id: "v1",
                item_id: "reject",
                vote: -1,
                origin: "widget",
                created_at: "2026-09-13T11:26:10-03:00",
                reason_key: Some("dup"),
                note: None,
            },
        )
        .await
        .unwrap();

        // The fallback path is what a failed brief session lands on, and the
        // stored brief was written over the item he just rejected.
        assert_eq!(store::last_reusable_brief(&f.pool).await.unwrap(), store::BriefFallback::Blocked);

        let rows = store::surfaced_items(&f.pool, 0.0, Duration::hours(24)).await.unwrap();
        let accepted: Vec<&str> =
            rows.iter().filter(|r| r.vote != -1).map(|r| r.id.as_str()).collect();
        assert_eq!(accepted, ["keep"], "a downvoted item is not a brief input");
    }

    #[test]
    fn stored_timestamps_round_trip() {
        let it = item("x", "https://a.dev/1", 1);
        let parsed = DateTime::parse_from_rfc3339(&it.published_at).unwrap();
        assert_eq!(parsed.with_timezone(&Utc).timestamp(), it.published.timestamp());
    }
}
