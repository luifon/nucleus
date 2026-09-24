//! End-to-end refresh over synthetic transcripts in a temp dir: parser
//! dedupe, subagent attribution, worktree → project mapping, cost-state
//! reconciliation, Codex cumulative handling, Nucleus labels, incremental
//! and idempotent re-reads.

use super::*;
use crate::config::UsageConfig;
use std::io::Write;

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
          WHERE key IN (SELECT key FROM usage_keys WHERE session_id='s-main')
             OR (kind != 'response' AND session_id='s-main')",
    )
    .await;
    // The fork repeats msg-b and its cost-state covers it: no residual for it.
    assert_eq!(scalar_i(&pool, "SELECT COUNT(*) FROM usage_rows WHERE kind='residual'").await, 1);
    assert_eq!(scalar_i(&pool, "SELECT COUNT(*) FROM usage_keys WHERE key='claude:msg-b'").await, 2);
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

#[tokio::test]
async fn second_refresh_is_refused_while_the_lock_is_fresh() {
    let fx = fixture().await;
    std::fs::write(fx.ws.join(LOCK_PATH), "123\n").unwrap();
    let err = refresh(&fx.ws, &fx.cfg, RefreshOptions::default()).await.unwrap_err();
    assert!(err.to_string().contains("already running"));
}
