//! End-to-end refresh over synthetic transcripts in a temp dir: parser
//! dedupe, subagent attribution, worktree → project mapping, cost-state
//! reconciliation, Codex cumulative handling, Nucleus labels, incremental
//! and idempotent re-reads.

use super::*;
use crate::config::UsageConfig;
use std::io::Write;
use std::os::unix::fs::MetadataExt;

fn assistant(id: &str, ts: &str, model: &str, input: i64, cw1h: i64, cr: i64, out: i64, cwd: Option<&str>) -> String {
    let cwd = cwd.map(|c| format!(r#""cwd":"{c}","#)).unwrap_or_default();
    format!(
        r#"{{"type":"assistant",{cwd}"timestamp":"{ts}","requestId":"req-{id}","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{input},"cache_creation_input_tokens":{cw1h},"cache_read_input_tokens":{cr},"output_tokens":{out},"cache_creation":{{"ephemeral_1h_input_tokens":{cw1h},"ephemeral_5m_input_tokens":0}}}},"content":[{{"type":"text","text":"x"}}]}}}}"#
    )
}

fn user(ts: &str, cwd: &str) -> String {
    format!(r#"{{"type":"user","cwd":"{cwd}","timestamp":"{ts}","message":{{"role":"user","content":"hi"}}}}"#)
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = std::fs::File::create(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

struct Fixture {
    _tmp: tempfile::TempDir,
    ws: PathBuf,
    cfg: UsageConfig,
    main: PathBuf,
}

const T0: &str = "2026-09-01T10:00:00.000Z";

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let ws = base.join("ws");
    std::fs::create_dir_all(ws.join("memory/logs/distiller")).unwrap();
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    let alpha = base.join("code/alpha");
    std::fs::create_dir_all(alpha.join(".git")).unwrap();
    std::fs::create_dir_all(alpha.join("src")).unwrap();
    let beta = base.join("code/beta");
    std::fs::create_dir_all(beta.join(".git")).unwrap();

    let projects = base.join("claude");
    let alpha_src = alpha.join("src").to_string_lossy().into_owned();
    let enc = project::encode_cwd(&alpha_src);
    let main = projects.join(&enc).join("s-main.jsonl");
    let start_ms = parse_ts("2026-09-01T09:59:00Z");
    write_lines(
        &main,
        &[
            user(T0, &alpha_src),
            // msg-a: streamed partial, then two content-block lines
            assistant("msg-a", "2026-09-01T10:00:01Z", "claude-opus-5", 10, 1000, 20000, 1, None),
            assistant("msg-a", "2026-09-01T10:00:03Z", "claude-opus-5", 10, 1000, 20000, 500, None),
            assistant("msg-a", "2026-09-01T10:00:03Z", "claude-opus-5", 10, 1000, 20000, 500, None),
            assistant("msg-b", "2026-09-01T10:01:00Z", "claude-opus-5", 5, 0, 21000, 300, None),
            format!(
                r#"{{"type":"cost-state","sessionId":"s-main","totalCostUSD":0.06905,"startTime":{start_ms},"modelUsage":{{"claude-opus-5[1m]":{{"inputTokens":18,"outputTokens":900,"cacheReadInputTokens":46000,"cacheCreationInputTokens":1200,"webSearchRequests":0,"costUSD":0.06759}},"claude-haiku-4-5-20251001":{{"inputTokens":1400,"outputTokens":12,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"webSearchRequests":0,"costUSD":0.00146}}}}}}"#
            ),
            // after the snapshot: outside every cost-state window
            assistant("msg-c", "2026-09-01T11:00:00Z", "claude-opus-5", 1, 0, 0, 100, None),
        ],
    );
    // A resumed/forked copy repeating msg-b, with a cost-state that covers
    // exactly that response. msg-b is owned by one session; the fork's
    // cost-state must still see it as observed (no residual).
    write_lines(
        &projects.join(&enc).join("s-fork.jsonl"),
        &[
            user(T0, &alpha_src),
            assistant("msg-b", "2026-09-01T10:01:00Z", "claude-opus-5", 5, 0, 21000, 300, None),
            format!(
                r#"{{"type":"cost-state","sessionId":"s-fork","totalCostUSD":0.018025,"startTime":{start_ms},"modelUsage":{{"claude-opus-5":{{"inputTokens":5,"outputTokens":300,"cacheReadInputTokens":21000,"cacheCreationInputTokens":0,"costUSD":0.018025}}}}}}"#
            ),
        ],
    );
    let sub = projects.join(&enc).join("s-main/subagents/agent-x1.jsonl");
    write_lines(&sub, &[assistant("msg-s1", "2026-09-01T10:00:30Z", "claude-opus-5", 3, 200, 5000, 100, Some(&alpha_src))]);
    std::fs::write(sub.with_extension("meta.json"), r#"{"agentType":"Explore","toolUseId":"t1"}"#).unwrap();

    // Claude session in a deleted nested worktree of alpha.
    let wt = alpha.join(".claude/worktrees/agent-9").to_string_lossy().into_owned();
    write_lines(
        &projects.join(project::encode_cwd(&wt)).join("s-wt.jsonl"),
        &[user(T0, &wt), assistant("msg-w", "2026-09-02T10:00:00Z", "claude-sonnet-5", 100, 0, 0, 50, None)],
    );

    // Nucleus reminder fire, in the workspace.
    let ws_s = ws.to_string_lossy().into_owned();
    write_lines(
        &projects.join(project::encode_cwd(&ws_s)).join("s-fire.jsonl"),
        &[user(T0, &ws_s), assistant("msg-f", "2026-09-03T08:00:00Z", "claude-opus-5", 50, 0, 1000, 80, None)],
    );
    write_lines(
        &projects.join(project::encode_cwd(&ws_s)).join("s-distill.jsonl"),
        &[user(T0, &ws_s), assistant("msg-d", "2026-09-03T04:00:00Z", "claude-opus-5", 50, 0, 1000, 80, None)],
    );
    std::fs::write(
        ws.join("memory/logs/distiller/runs.jsonl"),
        r#"{"run_id":"r1","agent":"distiller","session_id":"s-distill"}"#.to_string() + "\n",
    )
    .unwrap();
    let rpool = crate::db::open(&ws.join("memory/reminders.db")).await.unwrap();
    for sql in [
        "CREATE TABLE reminders (id INTEGER PRIMARY KEY, body TEXT NOT NULL, title TEXT, cron TEXT, status TEXT, created_by TEXT, system_prompt TEXT)",
        "CREATE TABLE reminder_fires (id INTEGER PRIMARY KEY, reminder_id INTEGER, fired_at TEXT, channel TEXT, success INTEGER, msg_id TEXT)",
        "INSERT INTO reminders VALUES (7, '', 'Daily digest', '0 8 * * *', 'pending', 'user', 'run the digest')",
        "INSERT INTO reminder_fires VALUES (1, 7, '2026-09-03T08:00:00Z', 'discord-home', 1, 'skill-fire:s-fire|silent')",
    ] {
        sqlx::query(sql).execute(&rpool).await.unwrap();
    }
    rpool.close().await;

    // Codex thread in beta, plus a subagent thread of it.
    let codex = base.join("codex/2026/09/01");
    let beta_s = beta.to_string_lossy().into_owned();
    let tc = |total: (i64, i64, i64), last: (i64, i64, i64)| {
        let u = |(i, c, o): (i64, i64, i64)| {
            format!(r#"{{"input_tokens":{i},"cached_input_tokens":{c},"output_tokens":{o},"reasoning_output_tokens":0,"total_tokens":{}}}"#, i + o)
        };
        format!(
            r#"{{"timestamp":"2026-09-01T12:00:00Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{},"last_token_usage":{}}},"rate_limits":{{"primary":{{"used_percent":42.0,"window_minutes":10080,"resets_at":1790000000}},"plan_type":"plus"}}}}}}"#,
            u(total),
            u(last)
        )
    };
    write_lines(
        &codex.join("rollout-2026-09-01T12-00-00-t-parent.jsonl"),
        &[
            format!(r#"{{"timestamp":"2026-09-01T11:59:00Z","type":"session_meta","payload":{{"id":"t-parent","cwd":"{beta_s}","originator":"codex-tui","source":"cli"}}}}"#),
            r#"{"timestamp":"2026-09-01T11:59:01Z","type":"turn_context","payload":{"model":"gpt-5.6-sol"}}"#.to_string(),
            tc((1000, 800, 50), (1000, 800, 50)),
            tc((1000, 800, 50), (1000, 800, 50)),
            tc((2600, 2200, 90), (1600, 1400, 40)),
            // a model the price table does not know: counted, never priced
            r#"{"timestamp":"2026-09-01T12:30:00Z","type":"turn_context","payload":{"model":"unlisted-model"}}"#.to_string(),
            tc((2700, 2200, 100), (100, 0, 10)),
        ],
    );
    write_lines(
        &codex.join("rollout-2026-09-01T12-05-00-t-child.jsonl"),
        &[
            format!(r#"{{"timestamp":"2026-09-01T12:05:00Z","type":"session_meta","payload":{{"id":"t-child","cwd":"{beta_s}","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"t-parent"}}}}}}}}}}"#),
            r#"{"timestamp":"2026-09-01T12:05:01Z","type":"turn_context","payload":{"model":"gpt-5.6-luna"}}"#.to_string(),
            tc((90000, 88000, 500), (300, 100, 20)),
        ],
    );

    let cfg = UsageConfig {
        claude_projects_dir: projects.to_string_lossy().into_owned(),
        codex_sessions_dir: base.join("codex").to_string_lossy().into_owned(),
        ..Default::default()
    };
    Fixture { _tmp: tmp, ws, cfg, main }
}

fn parse_ts(s: &str) -> i64 {
    records::parse_ts_ms(s).unwrap()
}

async fn scalar_f(pool: &SqlitePool, sql: &str) -> f64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn scalar_i(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn end_to_end_refresh() {
    let fx = fixture().await;
    let stats = refresh(&fx.ws, &fx.cfg, RefreshOptions::default()).await.unwrap();
    assert_eq!(stats.files_seen, 8);
    assert_eq!(stats.files_read, 8);
    let pool = open(&fx.ws).await.unwrap();

    // Dedupe: msg-a counted once with the final output.
    let a_out = scalar_i(&pool, "SELECT output FROM usage_rows WHERE key = 'claude:msg-a'").await;
    assert_eq!(a_out, 500);
    assert_eq!(scalar_i(&pool, "SELECT COUNT(*) FROM usage_rows WHERE key LIKE 'claude:%'").await, 7);

    // Subagent: attributed to the parent session, typed.
    let (sid, sub): (String, String) =
        sqlx::query_as("SELECT session_id, subagent_id FROM usage_rows WHERE key = 'claude:msg-s1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((sid.as_str(), sub.as_str()), ("s-main", "x1"));
    let at: String = sqlx::query_scalar("SELECT agent_type FROM subagents WHERE subagent_id = 'x1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(at, "Explore");

    // Projects: the subdirectory and the deleted nested worktree both map to alpha.
    let names: Vec<(String, String)> =
        sqlx::query_as("SELECT session_id, project_name FROM sessions ORDER BY session_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    let get = |s: &str| names.iter().find(|(id, _)| id == s).map(|(_, p)| p.clone()).unwrap();
    assert_eq!(get("s-main"), "alpha");
    assert_eq!(get("s-wt"), "alpha");
    assert_eq!(get("t-parent"), "beta");
    assert_eq!(get("s-fire"), "ws");

    // Reconciliation for s-main:
    //   opus: O == C in tokens; costUSD is 0.01 above the table → adjustment
    //   haiku: never in the transcript → residual 1400 in / 12 out, priced
    //   msg-c: after the snapshot → table price only
    let adj = scalar_f(&pool, "SELECT SUM(cost_usd) FROM usage_rows WHERE kind='adjustment'").await;
    assert!((adj - 0.01).abs() < 1e-9, "adjustment {adj}");
    let (ri, ro): (i64, i64) =
        sqlx::query_as("SELECT input, output FROM usage_rows WHERE kind='residual'").fetch_one(&pool).await.unwrap();
    assert_eq!((ri, ro), (1400, 12));
    let s_main_cost =
        scalar_f(
        &pool,
        "SELECT SUM(cost_usd) FROM usage_rows
          WHERE key IN (SELECT key FROM usage_obs WHERE session_id='s-main')
             OR (kind != 'response' AND session_id='s-main')",
    )
    .await;
    // The fork repeats msg-b and its cost-state covers it: no residual for it.
    assert_eq!(scalar_i(&pool, "SELECT COUNT(*) FROM usage_rows WHERE kind='residual'").await, 1);
    assert_eq!(scalar_i(&pool, "SELECT COUNT(*) FROM usage_obs WHERE key='claude:msg-b'").await, 2);
    assert_eq!(scalar_i(&pool, "SELECT COUNT(*) FROM usage_rows WHERE key='claude:msg-b'").await, 1);
    // costUSD of the run (0.06759 + 0.00146) + table(msg-c) (1*5 + 100*25)/1e6
    assert!((s_main_cost - (0.06759 + 0.00146 + 0.002505)).abs() < 1e-9, "cost {s_main_cost}");

    // Codex: repeat skipped, child attributed to parent, input excludes cached.
    let (n, input, cached, out): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), SUM(input), SUM(cache_read), SUM(output) FROM usage_rows WHERE vendor='codex'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 4);
    assert_eq!(cached, 800 + 1400 + 100);
    assert_eq!(input, 200 + 200 + 200 + 100);
    assert_eq!(out, 50 + 40 + 20 + 10);
    assert_eq!(
        scalar_i(&pool, "SELECT COUNT(*) FROM usage_rows WHERE vendor='codex' AND session_id='t-parent'").await,
        4
    );

    // Nucleus labels.
    let (agent, rid): (String, i64) =
        sqlx::query_as("SELECT agent, reminder_id FROM sessions WHERE session_id='s-fire'").fetch_one(&pool).await.unwrap();
    assert_eq!((agent.as_str(), rid), ("reminders-fire", 7));
    let n = query::nucleus(&pool, 0, &fx.ws.to_string_lossy(), query::VendorFilter::All).await.unwrap();
    assert!(n.agents.iter().any(|a| a.agent == "distiller"));
    assert_eq!(n.reminders[0].title.as_deref(), Some("Daily digest"));

    // Views run on the data.
    let sum = query::summary(&pool, 0, query::VendorFilter::All).await.unwrap();
    assert_eq!(sum.codex_quota[0].used_percent, 42.0);
    assert!(!sum.heatmap.is_empty());
    let projects = query::projects(&pool, 0, query::VendorFilter::All).await.unwrap();
    assert_eq!(projects[0].project, "alpha");

    // Tool filter: every view aggregates over the selected tool only.
    use query::VendorFilter as V;
    let codex_total: i64 = scalar_i(
        &pool,
        "SELECT SUM(input+cache_write_5m+cache_write_1h+cache_read+output) FROM usage_rows WHERE vendor='codex'",
    )
    .await;
    let cx = query::summary(&pool, 0, V::Codex).await.unwrap();
    assert_eq!(cx.by_vendor.len(), 1);
    assert_eq!(cx.by_vendor[0].vendor, "codex");
    assert_eq!(cx.range.current.tokens, codex_total);
    assert!(cx.models.iter().all(|m| m.vendor == "codex"));
    assert!(cx.daily.iter().all(|p| p.vendor == "codex"));
    assert!(!cx.codex_quota.is_empty());
    // Unpriced tokens are reported, not silently priced at zero.
    assert_eq!(cx.range.current.unpriced_tokens, 110);
    assert_eq!(
        scalar_f(&pool, "SELECT cost_usd FROM usage_rows WHERE model='unlisted-model'").await,
        0.0
    );
    let unpriced_row = cx.models.iter().find(|m| m.model == "unlisted-model").unwrap();
    assert_eq!(unpriced_row.totals.unpriced_tokens, 110);
    let st = query::status(Some(&pool), &fx.ws).await.unwrap();
    assert!(st.prices.iter().any(|p| p.model == "unlisted-model" && p.matched_key.is_none()));
    let cl = query::summary(&pool, 0, V::Claude).await.unwrap();
    assert!(cl.by_vendor.iter().all(|v| v.vendor == "claude"));
    assert!(cl.codex_quota.is_empty(), "Codex quota is not applicable to a Claude-only view");
    let heat_all: i64 = sum.heatmap.iter().map(|c| c.tokens).sum();
    let heat_cl: i64 = cl.heatmap.iter().map(|c| c.tokens).sum();
    let heat_cx: i64 = cx.heatmap.iter().map(|c| c.tokens).sum();
    assert_eq!(heat_all, heat_cl + heat_cx);
    let p = query::projects(&pool, 0, V::Codex).await.unwrap();
    assert_eq!(p.iter().map(|r| r.project.as_str()).collect::<Vec<_>>(), vec!["beta"]);
    let s = query::sessions(&pool, 0, 50, V::Claude).await.unwrap();
    assert!(!s.is_empty() && s.iter().all(|r| r.vendor == "claude"));
    let l = query::limits(&pool, 0, V::Claude).await.unwrap();
    assert!(l.codex_daily.is_empty());
    let n = query::nucleus(&pool, 0, &fx.ws.to_string_lossy(), V::Codex).await.unwrap();
    assert!(n.agents.is_empty() && n.reminders.is_empty());

    // Idempotent: nothing changed → nothing read; totals unchanged.
    let before = scalar_f(&pool, "SELECT SUM(cost_usd) FROM usage_rows").await;
    let tokens_before = scalar_i(&pool, "SELECT SUM(input+output+cache_read) FROM usage_rows").await;
    pool.close().await;
    let again = refresh(&fx.ws, &fx.cfg, RefreshOptions::default()).await.unwrap();
    assert_eq!(again.files_read, 0);
    let full = refresh(&fx.ws, &fx.cfg, RefreshOptions { full: true }).await.unwrap();
    assert_eq!(full.files_read, 8);
    let pool = open(&fx.ws).await.unwrap();
    assert!((scalar_f(&pool, "SELECT SUM(cost_usd) FROM usage_rows").await - before).abs() < 1e-12);
    assert_eq!(scalar_i(&pool, "SELECT SUM(input+output+cache_read) FROM usage_rows").await, tokens_before);
    pool.close().await;

    // Incremental: a half-written line is left for later; completing it
    // counts it once. The response's final line arrives in a later refresh
    // than its partial and still wins.
    let partial = assistant("msg-e", "2026-09-01T12:00:00Z", "claude-opus-5", 1, 0, 0, 1, None);
    let fin = assistant("msg-e", "2026-09-01T12:00:02Z", "claude-opus-5", 1, 0, 0, 700, None);
    append(&fx.main, &format!("{partial}\n{}", &fin[..40]));
    refresh(&fx.ws, &fx.cfg, RefreshOptions::default()).await.unwrap();
    let pool = open(&fx.ws).await.unwrap();
    assert_eq!(scalar_i(&pool, "SELECT output FROM usage_rows WHERE key='claude:msg-e'").await, 1);
    pool.close().await;
    append(&fx.main, &format!("{}\n", &fin[40..]));
    refresh(&fx.ws, &fx.cfg, RefreshOptions::default()).await.unwrap();
    let pool = open(&fx.ws).await.unwrap();
    assert_eq!(scalar_i(&pool, "SELECT output FROM usage_rows WHERE key='claude:msg-e'").await, 700);
    assert_eq!(scalar_i(&pool, "SELECT COUNT(*) FROM usage_rows WHERE key='claude:msg-e'").await, 1);
}

// ─── lock ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn refresh_is_refused_while_another_holds_the_lock_and_never_steals_it() {
    let fx = fixture().await;
    let held = RefreshLock::acquire(&fx.ws).await.unwrap();
    assert!(refresh_running(&fx.ws));
    // An old mtime on the lock file means nothing: there is no staleness
    // timer to expire while the holder is alive.
    let f = std::fs::File::options().write(true).open(fx.ws.join(LOCK_PATH)).unwrap();
    f.set_modified(std::time::SystemTime::UNIX_EPOCH).unwrap();
    let err = refresh(&fx.ws, &fx.cfg, RefreshOptions::default()).await.unwrap_err();
    assert!(err.to_string().contains("already running"), "{err}");
    assert!(fx.ws.join(LOCK_PATH).exists(), "the refused refresh must not delete the holder's lock");
    drop(held);
    assert!(!refresh_running(&fx.ws));
    refresh(&fx.ws, &fx.cfg, RefreshOptions::default()).await.unwrap();
    assert!(!refresh_running(&fx.ws), "released when the refresh ends");
}

// ─── convergence: incremental == --full ─────────────────────────────────────

fn codex_meta(id: &str, cwd: &str) -> String {
    format!(r#"{{"timestamp":"2026-09-05T09:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"{cwd}","originator":"codex-tui","source":"cli"}}}}"#)
}

fn codex_ctx(model: &str) -> String {
    format!(r#"{{"timestamp":"2026-09-05T09:00:01Z","type":"turn_context","payload":{{"model":"{model}"}}}}"#)
}

/// A token_count event; `total` and `last` are (input, cached, output).
fn codex_tc(ts: &str, total: (i64, i64, i64), last: (i64, i64, i64), used: f64, resets: Option<i64>) -> String {
    let u = |(i, c, o): (i64, i64, i64)| {
        format!(r#"{{"input_tokens":{i},"cached_input_tokens":{c},"output_tokens":{o},"reasoning_output_tokens":0,"total_tokens":{}}}"#, i + o)
    };
    let resets = resets.map(|r| r.to_string()).unwrap_or_else(|| "null".into());
    format!(
        r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{},"last_token_usage":{}}},"rate_limits":{{"primary":{{"used_percent":{used},"window_minutes":10080,"resets_at":{resets}}},"plan_type":"plus"}}}}}}"#,
        u(total),
        u(last)
    )
}

fn cost_state(session: &str, start_ms: i64, model: &str, c: (i64, i64, i64, i64), usd: f64) -> String {
    format!(
        r#"{{"type":"cost-state","sessionId":"{session}","startTime":{start_ms},"modelUsage":{{"{model}":{{"inputTokens":{},"outputTokens":{},"cacheReadInputTokens":{},"cacheCreationInputTokens":{},"costUSD":{usd}}}}}}}"#,
        c.0, c.1, c.2, c.3
    )
}

fn write_text(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Rewrite in place (same inode, O_TRUNC) and move mtime forward, so the
/// change is visible even within one second.
fn rewrite_in_place(path: &Path, text: &str, bump_secs: u64) {
    let mut f = std::fs::OpenOptions::new().write(true).truncate(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(bump_secs)).unwrap();
}

fn lines(ls: &[String]) -> String {
    ls.iter().map(|l| format!("{l}\n")).collect()
}

/// Every counted value, in a canonical order: what `--full` must reproduce.
async fn dump(ws: &Path) -> Vec<String> {
    let pool = crate::db::open_read_only(&ws.join(DB_PATH)).await.unwrap();
    let mut out = Vec::new();
    let rows: Vec<(String, String, String, Option<String>, i64, String, i64, i64, i64, i64, i64, f64)> = sqlx::query_as(
        "SELECT key, kind, session_id, subagent_id, ts_ms, model, input, cache_write_5m, cache_write_1h,
                cache_read, output, COALESCE(cost_usd, -1) FROM usage_rows ORDER BY key",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for r in rows {
        out.push(format!("row {} {} {} {:?} {} {} {} {} {} {} {} {:.9}", r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9, r.10, r.11));
    }
    let limits: Vec<(String, i64, Option<i64>)> =
        sqlx::query_as("SELECT DISTINCT key, ts_ms, resets_at FROM limit_events ORDER BY key").fetch_all(&pool).await.unwrap();
    out.extend(limits.into_iter().map(|l| format!("limit {l:?}")));
    let rates: Vec<(String, i64, f64, i64)> = sqlx::query_as(
        "SELECT slot, reset_key, used_percent, MAX(ts_ms) FROM rate_snapshots GROUP BY slot, reset_key, used_percent ORDER BY 1, 2, 3",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    out.extend(rates.into_iter().map(|r| format!("rate {r:?}")));
    let sessions: Vec<(String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT session_id, project_name, agent FROM sessions ORDER BY session_id").fetch_all(&pool).await.unwrap();
    out.extend(sessions.into_iter().map(|s| format!("session {s:?}")));
    pool.close().await;
    out
}

#[tokio::test]
async fn incremental_refresh_equals_full_through_rewrites_forks_and_resets() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo = base.join("code/gamma");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let repo_s = repo.to_string_lossy().into_owned();
    let projects = base.join("claude");
    let pdir = projects.join(project::encode_cwd(&repo_s));
    let codex_root = base.join("codex");
    let cdir = codex_root.join("2026/09/05");
    let ws_inc = base.join("ws-inc");
    let ws_full = base.join("ws-full");
    for ws in [&ws_inc, &ws_full] {
        std::fs::create_dir_all(ws.join("memory")).unwrap();
    }
    let cfg = UsageConfig {
        claude_projects_dir: projects.to_string_lossy().into_owned(),
        codex_sessions_dir: codex_root.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let inc = |step: &'static str| {
        let (ws, cfg) = (ws_inc.clone(), cfg.clone());
        async move {
            let s = refresh(&ws, &cfg, RefreshOptions::default()).await.unwrap();
            assert!(!s.partial(), "{step}: {:?}", s.warnings);
        }
    };
    let start = parse_ts("2026-09-05T08:00:00Z");
    let main = pdir.join("s-one.jsonl");
    let a = |id: &str, ts: &str, out: i64| assistant(id, ts, "claude-opus-5", 10, 100, 1000, out, None);

    // 1. Initial files.
    write_text(
        &main,
        &lines(&[user("2026-09-05T08:00:00Z", &repo_s), a("m1", "2026-09-05T08:01:00Z", 500), a("m2", "2026-09-05T08:02:00Z", 300)]),
    );
    let rollout = cdir.join("rollout-2026-09-05T09-00-00-t-one.jsonl");
    write_text(
        &rollout,
        &lines(&[
            codex_meta("t-one", &repo_s),
            codex_ctx("gpt-5.6-sol"),
            codex_tc("2026-09-05T09:01:00Z", (1000, 800, 50), (1000, 800, 50), 10.0, None),
            codex_tc("2026-09-05T09:01:00Z", (1000, 800, 50), (1000, 800, 50), 10.0, None),
        ]),
    );
    inc("initial").await;

    // 2. Append, ending in a half-written line.
    let m3 = a("m3", "2026-09-05T08:03:00Z", 700);
    append(&main, &format!("{}\n{}", a("m3", "2026-09-05T08:03:00Z", 1), &m3[..30]));
    inc("append").await;
    append(&main, &format!("{}\n", &m3[30..]));
    inc("append rest").await;

    // 3. Same-size rewrite: m2's output 300 → 900, identical length.
    let text = std::fs::read_to_string(&main).unwrap();
    let rewritten = text.replace(r#""output_tokens":300,"#, r#""output_tokens":900,"#);
    assert_eq!(rewritten.len(), text.len());
    rewrite_in_place(&main, &rewritten, 10);
    inc("same-size rewrite").await;
    {
        let pool = open(&ws_inc).await.unwrap();
        assert_eq!(scalar_i(&pool, "SELECT output FROM usage_rows WHERE key='claude:m2'").await, 900);
        pool.close().await;
    }

    // 4. Truncate and regrow past the old offset: m1 is gone, new responses
    //    and a cost-state run follow. Same inode.
    let regrown = lines(&[
        user("2026-09-05T08:00:00Z", &repo_s),
        a("m2", "2026-09-05T08:02:00Z", 900),
        a("m3", "2026-09-05T08:03:00Z", 700),
        a("m4", "2026-09-05T08:04:00Z", 250),
        a("m5", "2026-09-05T08:05:00Z", 125),
        a("m6", "2026-09-05T08:06:00Z", 60),
        cost_state("s-one", start, "claude-opus-5", (60, 3000, 6000, 600), 0.5),
    ]);
    assert!(regrown.len() as u64 > std::fs::metadata(&main).unwrap().len());
    rewrite_in_place(&main, &regrown, 20);
    inc("truncate and regrow").await;
    {
        let pool = open(&ws_inc).await.unwrap();
        assert_eq!(
            scalar_i(&pool, "SELECT COUNT(*) FROM usage_rows WHERE key='claude:m1'").await,
            0,
            "a response no file observes any more is removed"
        );
        pool.close().await;
    }

    // 5. Fork (new session file repeating m5/m6, with a counter that
    //    continues s-one's run) and a resume of s-one (new run).
    write_text(
        &pdir.join("s-fork.jsonl"),
        &lines(&[
            user("2026-09-05T08:10:00Z", &repo_s),
            a("m5", "2026-09-05T08:05:00Z", 125),
            a("m6", "2026-09-05T08:06:00Z", 60),
            a("f1", "2026-09-05T08:11:00Z", 400),
            cost_state("s-fork", start, "claude-opus-5", (80, 3500, 7000, 700), 0.62),
        ]),
    );
    append(
        &main,
        &lines(&[
            a("m7", "2026-09-05T10:00:00Z", 80),
            cost_state("s-one", parse_ts("2026-09-05T09:59:00Z"), "claude-opus-5", (15, 90, 1000, 100), 0.07),
        ]),
    );
    inc("fork and resume").await;

    // 6. Codex: counter reset whose total collides with the previous total,
    //    readings without a reset time, then a replace-by-rename rewrite
    //    (new inode) that keeps the content and adds an event.
    append(
        &rollout,
        &lines(&[
            codex_tc("2026-09-05T09:10:00Z", (1040, 0, 10), (1040, 0, 10), 12.0, None),
            codex_tc("2026-09-05T09:11:00Z", (1040, 0, 10), (1040, 0, 10), 12.0, None),
            codex_tc("2026-09-05T09:20:00Z", (2000, 500, 60), (960, 500, 50), 15.0, Some(1_790_000_000)),
        ]),
    );
    inc("codex reset").await;
    let mut ctext = std::fs::read_to_string(&rollout).unwrap();
    ctext.push_str(&lines(&[codex_tc("2026-09-05T09:30:00Z", (3000, 1500, 90), (1000, 1000, 30), 18.0, Some(1_790_000_000))]));
    let tmpf = cdir.join("rollout.tmp");
    std::fs::write(&tmpf, &ctext).unwrap();
    std::fs::rename(&tmpf, &rollout).unwrap();
    inc("codex rename").await;

    let incremental = dump(&ws_inc).await;
    {
        let pool = open(&ws_inc).await.unwrap();
        // Codex: repeat skipped, colliding reset counted, rewrite added one.
        assert_eq!(scalar_i(&pool, "SELECT COUNT(*) FROM usage_rows WHERE vendor='codex'").await, 4);
        assert_eq!(scalar_i(&pool, "SELECT SUM(output) FROM usage_rows WHERE vendor='codex'").await, 50 + 10 + 50 + 30);
        // Null-reset readings deduplicate: 10% and 12% once each.
        assert_eq!(
            scalar_i(&pool, "SELECT COUNT(*) FROM rate_snapshots WHERE resets_at IS NULL").await,
            2
        );
        // m5 and m6 are counted once although two files hold them.
        assert_eq!(scalar_i(&pool, "SELECT COUNT(*) FROM usage_rows WHERE key IN ('claude:m5','claude:m6')").await, 2);
        pool.close().await;
    }

    // A fresh database read with --full, and --full over the incremental one.
    refresh(&ws_full, &cfg, RefreshOptions { full: true }).await.unwrap();
    assert_eq!(dump(&ws_full).await, incremental, "fresh --full differs from the incremental history");
    refresh(&ws_inc, &cfg, RefreshOptions { full: true }).await.unwrap();
    assert_eq!(dump(&ws_inc).await, incremental, "--full over the incremental DB changed it");
}

/// A same-length rewrite in place that keeps the file's mtime (set back to
/// the exact previous value, as a tool that preserves timestamps would, or
/// as two writes within one timestamp tick look) is still re-read: ctime
/// moved, so the fingerprint is re-verified.
#[tokio::test]
async fn same_size_rewrite_with_the_same_mtime_is_reread() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo = base.join("code/delta");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let repo_s = repo.to_string_lossy().into_owned();
    let projects = base.join("claude");
    let main = projects.join(project::encode_cwd(&repo_s)).join("s-same.jsonl");
    let (ws_inc, ws_full) = (base.join("ws-inc"), base.join("ws-full"));
    for ws in [&ws_inc, &ws_full] {
        std::fs::create_dir_all(ws.join("memory")).unwrap();
    }
    let cfg = UsageConfig {
        claude_projects_dir: projects.to_string_lossy().into_owned(),
        codex_sessions_dir: base.join("codex").to_string_lossy().into_owned(),
        ..Default::default()
    };
    let a = |out: i64| assistant("n1", "2026-09-06T08:01:00Z", "claude-opus-5", 10, 100, 1000, out, None);
    write_text(&main, &lines(&[user("2026-09-06T08:00:00Z", &repo_s), a(300)]));
    refresh(&ws_inc, &cfg, RefreshOptions::default()).await.unwrap();

    let before = std::fs::metadata(&main).unwrap();
    let text = std::fs::read_to_string(&main).unwrap();
    let rewritten = text.replace(r#""output_tokens":300,"#, r#""output_tokens":900,"#);
    assert_eq!(rewritten.len(), text.len());
    {
        let mut f = std::fs::OpenOptions::new().write(true).truncate(true).open(&main).unwrap();
        f.write_all(rewritten.as_bytes()).unwrap();
        f.set_modified(before.modified().unwrap()).unwrap();
    }
    let after = std::fs::metadata(&main).unwrap();
    assert_eq!((after.ino(), after.len(), after.modified().unwrap()), (before.ino(), before.len(), before.modified().unwrap()));

    let s = refresh(&ws_inc, &cfg, RefreshOptions::default()).await.unwrap();
    assert_eq!(s.files_read, 1, "the rewrite must not take the unchanged fast path");
    {
        let pool = open(&ws_inc).await.unwrap();
        assert_eq!(scalar_i(&pool, "SELECT output FROM usage_rows WHERE key='claude:n1'").await, 900);
        pool.close().await;
    }
    refresh(&ws_full, &cfg, RefreshOptions { full: true }).await.unwrap();
    assert_eq!(dump(&ws_full).await, dump(&ws_inc).await, "incremental differs from --full");
}

// ─── partial refresh ────────────────────────────────────────────────────────

#[tokio::test]
async fn unreadable_files_and_malformed_lines_make_a_partial_refresh() {
    let fx = fixture().await;
    // A malformed assistant line in the middle of a transcript.
    append(&fx.main, "{\"type\":\"assistant\",\"message\":{\"id\":\"broken\"\n");
    // An unreadable Codex file.
    let locked = PathBuf::from(&fx.cfg.codex_sessions_dir).join("2026/09/01/rollout-locked.jsonl");
    std::fs::write(&locked, "{}\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&locked).is_ok() {
        return; // running as root: permissions do not apply
    }

    let s = refresh(&fx.ws, &fx.cfg, RefreshOptions::default()).await.unwrap();
    assert!(s.partial());
    assert_eq!(s.files_failed, 1);
    assert_eq!(s.malformed_lines, 1);
    assert!(s.warnings.iter().any(|w| w.contains("rollout-locked.jsonl")));

    let pool = crate::db::open_read_only(&fx.ws.join(DB_PATH)).await.unwrap();
    let st = query::status(Some(&pool), &fx.ws).await.unwrap();
    let last = st.last_refresh.unwrap();
    assert_eq!((last.files_failed, last.malformed_lines), (1, 1));
    assert!(last.warnings.unwrap().contains("rollout-locked.jsonl"));
    // The malformed line stays reported after later refreshes read nothing.
    pool.close().await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    let again = refresh(&fx.ws, &fx.cfg, RefreshOptions::default()).await.unwrap();
    assert!(!again.partial());
    let pool = crate::db::open_read_only(&fx.ws.join(DB_PATH)).await.unwrap();
    let st = query::status(Some(&pool), &fx.ws).await.unwrap();
    assert_eq!(st.malformed_lines_total, 1);
}

// ─── batching ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_file_larger_than_one_batch_is_counted_whole() {
    let fx = fixture().await;
    let n = ingest::BATCH_RECORDS * 2 + 7;
    let mut text = String::new();
    for i in 0..n {
        text.push_str(&assistant(&format!("bulk-{i}"), "2026-09-04T10:00:00Z", "claude-opus-5", 1, 0, 0, 2, None));
        text.push('\n');
    }
    append(&fx.main, &text);
    refresh(&fx.ws, &fx.cfg, RefreshOptions::default()).await.unwrap();
    let pool = open(&fx.ws).await.unwrap();
    assert_eq!(scalar_i(&pool, "SELECT COUNT(*) FROM usage_rows WHERE key LIKE 'claude:bulk-%'").await, n as i64);
}
