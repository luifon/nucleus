//! Pricing and cost-state reconciliation (ADR-034).
//!
//! Two Claude sources describe the same spending:
//!
//! - **Responses** (`O`): the deduped assistant lines of the main transcript
//!   and its subagent transcripts. Timestamped per API response; the basis
//!   of every time-bucketed view. A response is stored once (first session
//!   file to record it owns the row), but `usage_keys` lists every session
//!   whose files contain it, and `O` is computed over those keys: a resumed
//!   or forked session repeats responses another file already owns, and they
//!   still count as observed for its cost-state.
//! - **cost-state** (`C`): Claude Code's running totals for one process run
//!   (`startTime`), per model, with its own dollar estimate `costUSD`. It
//!   also counts calls that never appear as assistant lines (title
//!   generation and other background calls, usually on a small model). It
//!   is NOT a session total: a resumed session starts a new run, and the
//!   counter does not always carry the previous run's totals, so long
//!   resumed sessions show `C` far below `O`.
//!
//! Rule, per (session, run, model), over the run's window
//! `[startTime, snapshot]` and per token category:
//!
//! - tokens counted = `max(O, C)`: the responses, plus a **residual** row of
//!   `max(0, C − O)` for what cost-state saw and the transcript did not.
//! - dollars = `costUSD` for everything cost-state covers, and the price
//!   table for observed tokens beyond it. Implemented as table prices on
//!   every response and residual row plus one **adjustment** row carrying
//!   `costUSD − table(C)`. Summed: `table(O) + table(max(0,C−O)) + costUSD −
//!   table(C) = costUSD + table(max(0, O−C))`.
//!
//! Nothing is counted twice: residual tokens are only the excess of `C` over
//! `O`, and the adjustment replaces (not adds to) the table's price of `C`.
//! When the table matches Claude Code's prices the adjustment is ~0; its
//! size is reported as the table's drift. A model the table does not price
//! gets its dollars entirely from the adjustment where cost-state covers
//! it. Cost-state records its cache writes without the 5-minute/1-hour
//! split; `C`'s split is taken from the window's responses (1-hour when the
//! window has none, Claude Code's default).
//!
//! Residual and adjustment rows are derived: every refresh deletes and
//! recomputes all of them, so they never drift from their inputs.

use super::pricing::{PriceTable, Tokens, cost};
use super::store::Local;
use anyhow::Result;
use sqlx::SqlitePool;

/// Price every response row whose cost is unset, or all of them when the
/// effective price table changed. Records the resolved price per model.
pub async fn price_rows(pool: &SqlitePool, table: &PriceTable) -> Result<()> {
    let fingerprint = format!("{table:?}");
    if super::store::meta_get(pool, "price_fingerprint").await?.as_deref() != Some(fingerprint.as_str()) {
        sqlx::query("UPDATE usage_rows SET cost_usd = NULL WHERE kind = 'response'")
            .execute(pool)
            .await?;
        super::store::meta_set(pool, "price_fingerprint", &fingerprint).await?;
    }

    let models: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT model FROM usage_rows WHERE cost_usd IS NULL AND kind = 'response'")
            .fetch_all(pool)
            .await?;
    for model in models {
        let (i, o, cr, w5, w1) = match table.lookup(&model) {
            Some((_, p, _)) => (p.input, p.output, p.cache_read, p.cache_write_5m, p.cache_write_1h),
            None => (0.0, 0.0, 0.0, 0.0, 0.0),
        };
        sqlx::query(
            "UPDATE usage_rows SET cost_usd =
               (input * ?1 + output * ?2 + cache_read * ?3 + cache_write_5m * ?4 + cache_write_1h * ?5) / 1000000.0
             WHERE cost_usd IS NULL AND kind = 'response' AND model = ?6",
        )
        .bind(i)
        .bind(o)
        .bind(cr)
        .bind(w5)
        .bind(w1)
        .bind(&model)
        .execute(pool)
        .await?;
    }

    // The price listing covers every model seen in either source.
    let all: Vec<String> = sqlx::query_scalar(
        "SELECT model FROM usage_rows WHERE kind = 'response' GROUP BY model
         UNION SELECT model FROM cost_runs GROUP BY model",
    )
    .fetch_all(pool)
    .await?;
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM prices").execute(&mut *tx).await?;
    for model in all {
        let hit = table.lookup(&model);
        sqlx::query(
            "INSERT INTO prices (model, matched_key, source, input, output, cache_read, cache_write_5m, cache_write_1h)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(&model)
        .bind(hit.map(|h| h.0.to_string()))
        .bind(hit.map(|h| h.2.as_str()))
        .bind(hit.map(|h| h.1.input))
        .bind(hit.map(|h| h.1.output))
        .bind(hit.map(|h| h.1.cache_read))
        .bind(hit.map(|h| h.1.cache_write_5m))
        .bind(hit.map(|h| h.1.cache_write_1h))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ReconcileStats {
    pub runs: usize,
    pub residual_rows: usize,
    pub residual_tokens: i64,
    /// Sum of `costUSD − table(C)` over all runs.
    pub adjustment_usd: f64,
    /// Sum of `costUSD` over all runs.
    pub cost_state_usd: f64,
}

#[derive(sqlx::FromRow)]
struct Run {
    session_id: String,
    start_ms: i64,
    model: String,
    snapshot_ts_ms: Option<i64>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    cost_usd: f64,
}

/// Pure per-run computation: (residual tokens, C with its cache-write
/// split resolved).
pub fn residual(observed: &Tokens, c_input: i64, c_output: i64, c_cache_read: i64, c_cache_write: i64) -> (Tokens, Tokens) {
    let o_writes = observed.cache_write_5m + observed.cache_write_1h;
    let share_1h = if o_writes > 0 { observed.cache_write_1h as f64 / o_writes as f64 } else { 1.0 };
    let split = |total: i64| -> (i64, i64) {
        let h = (total as f64 * share_1h).round() as i64;
        (total - h, h)
    };
    let (c5, c1) = split(c_cache_write);
    let c = Tokens {
        input: c_input,
        cache_write_5m: c5,
        cache_write_1h: c1,
        cache_read: c_cache_read,
        output: c_output,
    };
    let (r5, r1) = split((c_cache_write - o_writes).max(0));
    let r = Tokens {
        input: (c_input - observed.input).max(0),
        cache_write_5m: r5,
        cache_write_1h: r1,
        cache_read: (c_cache_read - observed.cache_read).max(0),
        output: (c_output - observed.output).max(0),
    };
    (r, c)
}

pub async fn reconcile(pool: &SqlitePool, table: &PriceTable, local: Local) -> Result<ReconcileStats> {
    let mut stats = ReconcileStats::default();
    let runs: Vec<Run> = sqlx::query_as(
        "SELECT session_id, start_ms, model, snapshot_ts_ms, input, output, cache_read, cache_write, cost_usd
           FROM cost_runs",
    )
    .fetch_all(pool)
    .await?;

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM usage_rows WHERE kind IN ('residual', 'adjustment')")
        .execute(&mut *tx)
        .await?;

    for run in runs {
        stats.runs += 1;
        stats.cost_state_usd += run.cost_usd;
        let end = run.snapshot_ts_ms.unwrap_or(i64::MAX);
        let o: (i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(input),0), COALESCE(SUM(cache_write_5m),0), COALESCE(SUM(cache_write_1h),0),
                    COALESCE(SUM(cache_read),0), COALESCE(SUM(output),0)
               FROM usage_rows
              WHERE kind = 'response' AND vendor = 'claude' AND model = ?2
                AND ts_ms >= ?3 AND ts_ms <= ?4
                AND key IN (SELECT key FROM usage_keys WHERE session_id = ?1)",
        )
        .bind(&run.session_id)
        .bind(&run.model)
        .bind(run.start_ms)
        .bind(end)
        .fetch_one(&mut *tx)
        .await?;
        let observed = Tokens {
            input: o.0,
            cache_write_5m: o.1,
            cache_write_1h: o.2,
            cache_read: o.3,
            output: o.4,
        };
        let (res, c) = residual(&observed, run.input, run.output, run.cache_read, run.cache_write);
        let price = table.lookup(&run.model).map(|h| h.1);
        let ts = if end == i64::MAX { run.start_ms } else { end };
        let (day, hour, dow) = local.fields(ts);

        if res.total() > 0 {
            stats.residual_rows += 1;
            stats.residual_tokens += res.total();
            let res_cost = price.map(|p| cost(&p, &res)).unwrap_or(0.0);
            insert_derived(
                &mut tx,
                &format!("residual:{}:{}:{}", run.session_id, run.start_ms, run.model),
                "residual",
                &run,
                ts,
                (&day, hour, dow),
                &res,
                res_cost,
            )
            .await?;
        }

        let adjustment = run.cost_usd - price.map(|p| cost(&p, &c)).unwrap_or(0.0);
        stats.adjustment_usd += adjustment;
        if adjustment.abs() > 1e-9 {
            insert_derived(
                &mut tx,
                &format!("adjust:{}:{}:{}", run.session_id, run.start_ms, run.model),
                "adjustment",
                &run,
                ts,
                (&day, hour, dow),
                &Tokens::default(),
                adjustment,
            )
            .await?;
        }
    }
    tx.commit().await?;
    Ok(stats)
}

#[allow(clippy::too_many_arguments)]
async fn insert_derived(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    key: &str,
    kind: &str,
    run: &Run,
    ts: i64,
    local: (&str, i64, i64),
    t: &Tokens,
    cost_usd: f64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO usage_rows
           (key, vendor, kind, session_id, subagent_id, ts_ms, local_day, local_hour, local_dow,
            model, input, cache_write_5m, cache_write_1h, cache_read, output, reasoning, cost_usd)
         VALUES (?1, 'claude', ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 0, ?14)",
    )
    .bind(key)
    .bind(kind)
    .bind(&run.session_id)
    .bind(ts)
    .bind(local.0)
    .bind(local.1)
    .bind(local.2)
    .bind(&run.model)
    .bind(t.input)
    .bind(t.cache_write_5m)
    .bind(t.cache_write_1h)
    .bind(t.cache_read)
    .bind(t.output)
    .bind(cost_usd)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn residual_is_only_the_excess_of_cost_state() {
        let observed = Tokens { input: 100, cache_write_5m: 0, cache_write_1h: 1000, cache_read: 50_000, output: 900 };
        // Cost-state saw more input/cache-read (background calls) and less
        // output (counter reset mid-session).
        let (r, c) = residual(&observed, 600, 800, 60_000, 1000);
        assert_eq!(r.input, 500);
        assert_eq!(r.cache_read, 10_000);
        assert_eq!(r.output, 0, "never negative");
        assert_eq!(r.cache_write_5m + r.cache_write_1h, 0);
        assert_eq!((c.cache_write_5m, c.cache_write_1h), (0, 1000), "C's split follows the window");
    }

    #[test]
    fn unobserved_model_residual_is_all_of_cost_state() {
        let (r, c) = residual(&Tokens::default(), 1400, 12, 0, 0);
        assert_eq!((r.input, r.output), (1400, 12));
        assert_eq!(c.input, 1400);
    }
}
