//! Pricing and cost-state reconciliation (ADR-034 §4).
//!
//! Two Claude sources describe the same spending:
//!
//! - **Responses** (`O`): the counted assistant responses of the main
//!   transcript and its subagent transcripts. Timestamped per API response;
//!   the basis of every time-bucketed view. A response is counted once
//!   (`usage_rows`), but `usage_obs` lists every session whose files contain
//!   it: a resumed or forked session repeats responses of another file.
//! - **cost-state** (`C`): Claude Code's running totals for one process run
//!   (`startTime`), per model, with its own dollar estimate `costUSD`. It
//!   also counts calls that never appear as assistant lines (title
//!   generation and other background calls). It is NOT a session total: a
//!   resumed session starts a new run, and the counter does not always
//!   carry the previous run's totals.
//!
//! Rule, per model, over each run's window `[startTime, snapshot]`:
//!
//! 1. **One owner per response.** Runs are ordered by (start, snapshot,
//!    session). A response in the window of several runs (a fork or resume
//!    that repeats it, or a counter shared by several sessions of one
//!    process) belongs to the first run whose window holds it. Later runs
//!    see it as *carried*.
//! 2. **New counter part.** A run subtracts `D`, what its counter provably
//!    already holds from earlier runs. Proof comes from response identities
//!    and the counter's own identity, never from comparing totals:
//!    - **continued**: an earlier run with the same start time (the same
//!      counter), at least one response key in both windows, and no counter
//!      category above this run's. The latest such run (largest total) is
//!      the one continued. `D` = its totals (background calls included:
//!      they are the same records of the same counter), plus the carried
//!      responses its window does not hold; `D`'s dollars = its `costUSD`
//!      plus the extra responses' price share of the remaining dollars;
//!    - otherwise the counter restarted: `D` = the carried responses'
//!      tokens (capped at the run's counter), and `D`'s dollars = the run's
//!      `costUSD` times `D`'s share of the run's table price (token share
//!      for a model without a price). An earlier run's background tokens
//!      are never subtracted here: nothing shows they are in this counter.
//!
//!    `C' = C − D` and `costUSD' = costUSD − D$` are the run's new part.
//! 3. **Counted.** Tokens: the owned responses, plus a **residual** row of
//!    `max(0, C' − O_own)` per category for what cost-state saw and no
//!    transcript line records. Dollars: table prices on every response and
//!    residual row, plus one **adjustment** row of `costUSD' − table(C')`,
//!    so the sum is `costUSD' + table(max(0, O_own − C'))`.
//!
//! Nothing is counted twice: each response has one owner, each counter's
//! dollars are split between runs by subtraction, residual tokens are only
//! the excess of `C'` over `O_own`, and the adjustment replaces (not adds
//! to) the table's price of `C'`. A model the table does not price gets its
//! dollars entirely from the adjustments, once per counter. Cost-state
//! records cache writes without the 5-minute/1-hour split; `C'`'s split
//! follows the window's responses (1-hour when the window has none).
//!
//! A counter that continued an earlier run under a new start time is
//! treated as restarted: only the responses both windows hold are
//! subtracted, so the earlier run's background calls may be counted twice.
//! The data this was built on shows continued counters keeping the
//! original start time.
//!
//! Residual and adjustment rows are derived: every refresh deletes and
//! recomputes all of them, so they never drift from their inputs.

use super::pricing::{PriceTable, Tokens, cost};
use super::store::Local;
use crate::config::ModelPrice;
use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};

/// Price every response row whose cost is unset, or all of them when the
/// effective price table changed; each row is one request, so OpenAI's
/// long-context rates apply per row. Records the resolved price per model.
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
    let zero = ModelPrice {
        input: 0.0,
        output: 0.0,
        cache_read: 0.0,
        cache_write_5m: 0.0,
        cache_write_1h: 0.0,
        long_context: None,
    };
    for model in models {
        let price = table.lookup(&model).map(|r| r.price).unwrap_or(zero);
        let lc = price.long_context;
        sqlx::query(
            "UPDATE usage_rows SET cost_usd = CASE
               WHEN ?7 IS NOT NULL AND input + cache_read + cache_write_5m + cache_write_1h > ?7
               THEN (input * ?8 + output * ?9 + cache_read * ?10 + (cache_write_5m + cache_write_1h) * ?11) / 1000000.0
               ELSE (input * ?1 + output * ?2 + cache_read * ?3 + cache_write_5m * ?4 + cache_write_1h * ?5) / 1000000.0
             END
             WHERE cost_usd IS NULL AND kind = 'response' AND model = ?6",
        )
        .bind(price.input)
        .bind(price.output)
        .bind(price.cache_read)
        .bind(price.cache_write_5m)
        .bind(price.cache_write_1h)
        .bind(&model)
        .bind(lc.map(|l| l.above_input_tokens))
        .bind(lc.map(|l| l.input).unwrap_or(0.0))
        .bind(lc.map(|l| l.output).unwrap_or(0.0))
        .bind(lc.map(|l| l.cache_read).unwrap_or(0.0))
        .bind(lc.map(|l| l.cache_write).unwrap_or(0.0))
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
        let lc = hit.and_then(|h| h.price.long_context);
        sqlx::query(
            "INSERT INTO prices (model, matched_key, basis, source_url, retrieved, input, output, cache_read,
                                 cache_write_5m, cache_write_1h, cache_write_inferred, cache_read_inferred,
                                 long_context_above, lc_input, lc_output, lc_cache_read, lc_cache_write)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        )
        .bind(&model)
        .bind(hit.map(|h| h.key.to_string()))
        .bind(hit.map(|h| h.provenance.basis.as_str()))
        .bind(hit.and_then(|h| h.provenance.source_url.clone()))
        .bind(hit.and_then(|h| h.provenance.retrieved.clone()))
        .bind(hit.map(|h| h.price.input))
        .bind(hit.map(|h| h.price.output))
        .bind(hit.map(|h| h.price.cache_read))
        .bind(hit.map(|h| h.price.cache_write_5m))
        .bind(hit.map(|h| h.price.cache_write_1h))
        .bind(hit.is_some_and(|h| h.provenance.cache_write_inferred))
        .bind(hit.is_some_and(|h| h.provenance.cache_read_inferred))
        .bind(lc.map(|l| l.above_input_tokens))
        .bind(lc.map(|l| l.input))
        .bind(lc.map(|l| l.output))
        .bind(lc.map(|l| l.cache_read))
        .bind(lc.map(|l| l.cache_write))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ReconcileStats {
    pub runs: usize,
    /// Runs whose window held responses owned by an earlier run.
    pub carried_runs: usize,
    pub residual_rows: usize,
    pub residual_tokens: i64,
    /// Sum of `costUSD' − table(C')` over all runs.
    pub adjustment_usd: f64,
    /// Sum of `costUSD'` over all runs: Claude Code's own estimates, each
    /// counter counted once.
    pub cost_state_usd: f64,
}

/// Cost-state totals as recorded (cache writes not split by duration).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counter {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

impl Counter {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    fn min(self, o: Counter) -> Counter {
        Counter {
            input: self.input.min(o.input),
            output: self.output.min(o.output),
            cache_read: self.cache_read.min(o.cache_read),
            cache_write: self.cache_write.min(o.cache_write),
        }
    }

    fn plus(self, o: Counter) -> Counter {
        Counter {
            input: self.input + o.input,
            output: self.output + o.output,
            cache_read: self.cache_read + o.cache_read,
            cache_write: self.cache_write + o.cache_write,
        }
    }

    /// Every category at least `o`'s: what a counter that continued `o`
    /// holds.
    fn covers(self, o: Counter) -> bool {
        self.input >= o.input
            && self.output >= o.output
            && self.cache_read >= o.cache_read
            && self.cache_write >= o.cache_write
    }

    fn minus(self, o: Counter) -> Counter {
        Counter {
            input: (self.input - o.input).max(0),
            output: (self.output - o.output).max(0),
            cache_read: (self.cache_read - o.cache_read).max(0),
            cache_write: (self.cache_write - o.cache_write).max(0),
        }
    }

    fn of(t: &Tokens) -> Counter {
        Counter {
            input: t.input,
            output: t.output,
            cache_read: t.cache_read,
            cache_write: t.cache_write_5m + t.cache_write_1h,
        }
    }
}

fn add(a: &mut Tokens, b: &Tokens) {
    a.input += b.input;
    a.cache_write_5m += b.cache_write_5m;
    a.cache_write_1h += b.cache_write_1h;
    a.cache_read += b.cache_read;
    a.output += b.output;
}

/// One cost-state run of one model, with the responses in its window.
#[derive(Debug, Clone)]
pub struct RunInput {
    pub session_id: String,
    pub start_ms: i64,
    pub snapshot_ts_ms: Option<i64>,
    pub counter: Counter,
    pub cost_usd: f64,
    /// Responses in the window: (key, tokens).
    pub window: Vec<(String, Tokens)>,
}

/// What a run contributes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RunOutcome {
    pub residual: Tokens,
    /// `costUSD' − table(C')`.
    pub adjustment: f64,
    /// `costUSD'`: the run's new part of Claude Code's estimate.
    pub new_cost_usd: f64,
    pub carried: bool,
}

/// Split one counter's cache writes by the window's 5-minute/1-hour share
/// (1-hour when the window has no cache writes).
fn split(total: i64, window: &Tokens) -> (i64, i64) {
    let writes = window.cache_write_5m + window.cache_write_1h;
    let share_1h = if writes > 0 { window.cache_write_1h as f64 / writes as f64 } else { 1.0 };
    let h = (total as f64 * share_1h).round() as i64;
    (total - h, h)
}

/// Pure per-run computation: (residual tokens, `C` with its cache-write
/// split resolved), given the owned responses `observed` and the split
/// source `window`.
pub fn residual(observed: &Tokens, window: &Tokens, c: Counter) -> (Tokens, Tokens) {
    let (c5, c1) = split(c.cache_write, window);
    let c_tokens = Tokens { input: c.input, cache_write_5m: c5, cache_write_1h: c1, cache_read: c.cache_read, output: c.output };
    let o_writes = observed.cache_write_5m + observed.cache_write_1h;
    let (r5, r1) = split((c.cache_write - o_writes).max(0), window);
    let r = Tokens {
        input: (c.input - observed.input).max(0),
        cache_write_5m: r5,
        cache_write_1h: r1,
        cache_read: (c.cache_read - observed.cache_read).max(0),
        output: (c.output - observed.output).max(0),
    };
    (r, c_tokens)
}

/// Reconcile all runs of ONE model (see the module docs). Runs are
/// processed in (start, snapshot, session) order; the result is in the
/// input order.
pub fn reconcile_model(runs: &[RunInput], price: Option<&ModelPrice>) -> Vec<RunOutcome> {
    let mut order: Vec<usize> = (0..runs.len()).collect();
    order.sort_by(|&a, &b| {
        let (ra, rb) = (&runs[a], &runs[b]);
        (ra.start_ms, ra.snapshot_ts_ms.unwrap_or(i64::MAX), &ra.session_id).cmp(&(
            rb.start_ms,
            rb.snapshot_ts_ms.unwrap_or(i64::MAX),
            &rb.session_id,
        ))
    });
    let weight = |c: &Tokens| match price {
        Some(p) if cost(p, c) > 0.0 => cost(p, c),
        _ => c.total() as f64,
    };
    let share_usd = |part: Counter, whole: Counter, usd: f64, window: &Tokens| {
        let (_, whole_split) = residual(&Tokens::default(), window, whole);
        let (_, part_split) = residual(&Tokens::default(), window, part);
        let w = weight(&whole_split);
        if w > 0.0 { usd * (weight(&part_split) / w).min(1.0) } else { 0.0 }
    };
    let mut owner: HashMap<&str, usize> = HashMap::new();
    let keys: Vec<HashSet<&str>> = runs.iter().map(|r| r.window.iter().map(|(k, _)| k.as_str()).collect()).collect();
    let mut done: Vec<usize> = Vec::new();
    let mut out = vec![RunOutcome::default(); runs.len()];
    for &i in &order {
        let run = &runs[i];
        let c = run.counter;
        let mut own = Tokens::default();
        let mut window = Tokens::default();
        let mut carried_any = false;
        for (key, t) in &run.window {
            add(&mut window, t);
            if owner.contains_key(key.as_str()) {
                carried_any = true;
            } else {
                owner.insert(key.as_str(), i);
                add(&mut own, t);
            }
        }
        // The counter continued an earlier run only when that is provable:
        // the same counter identity (start time), at least one response in
        // both windows, and no category below the earlier counter's.
        let cont = done
            .iter()
            .copied()
            .filter(|&p| {
                runs[p].start_ms == run.start_ms
                    && c.covers(runs[p].counter)
                    && keys[p].iter().any(|k| keys[i].contains(k))
            })
            .max_by_key(|&p| (runs[p].counter.total(), runs[p].snapshot_ts_ms));
        // Carried responses the continued counter does not already hold.
        let mut extra = Tokens::default();
        for (key, t) in &run.window {
            if owner.get(key.as_str()) != Some(&i) && !cont.is_some_and(|p| keys[p].contains(key.as_str())) {
                add(&mut extra, t);
            }
        }
        let (d, d_usd) = match cont {
            Some(p) => {
                let base = c.min(runs[p].counter);
                let base_usd = runs[p].cost_usd.min(run.cost_usd);
                let rest = c.minus(base);
                let more = rest.min(Counter::of(&extra));
                (base.plus(more), base_usd + share_usd(more, rest, run.cost_usd - base_usd, &window))
            }
            None => {
                let d = c.min(Counter::of(&extra));
                (d, share_usd(d, c, run.cost_usd, &window))
            }
        };
        let c_new = c.minus(d);
        let new_cost_usd = (run.cost_usd - d_usd).max(0.0);
        let (res, c_split) = residual(&own, &window, c_new);
        let table_c = price.map(|p| cost(p, &c_split)).unwrap_or(0.0);
        out[i] = RunOutcome {
            residual: res,
            adjustment: new_cost_usd - table_c,
            new_cost_usd,
            carried: carried_any || cont.is_some(),
        };
        done.push(i);
    }
    out
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

pub async fn reconcile(pool: &SqlitePool, table: &PriceTable, local: Local) -> Result<ReconcileStats> {
    let mut stats = ReconcileStats::default();
    // One row per (session, run, model): the same session's file seen at
    // two paths keeps the latest snapshot.
    let rows: Vec<Run> = sqlx::query_as(
        "SELECT session_id, start_ms, model, snapshot_ts_ms, input, output, cache_read, cache_write, cost_usd
           FROM cost_runs
          ORDER BY session_id, start_ms, model, COALESCE(snapshot_ts_ms, 0) DESC, path",
    )
    .fetch_all(pool)
    .await?;
    let mut by_model: std::collections::BTreeMap<String, Vec<Run>> = Default::default();
    let mut last: Option<(String, i64, String)> = None;
    for r in rows {
        let id = (r.session_id.clone(), r.start_ms, r.model.clone());
        if last.as_ref() == Some(&id) {
            continue;
        }
        last = Some(id);
        by_model.entry(r.model.clone()).or_default().push(r);
    }

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM usage_rows WHERE kind IN ('residual', 'adjustment')")
        .execute(&mut *tx)
        .await?;

    for (model, runs) in by_model {
        let mut inputs = Vec::with_capacity(runs.len());
        for run in &runs {
            let end = run.snapshot_ts_ms.unwrap_or(i64::MAX);
            let window: Vec<(String, i64, i64, i64, i64, i64)> = sqlx::query_as(
                "SELECT key, input, cache_write_5m, cache_write_1h, cache_read, output
                   FROM usage_rows
                  WHERE kind = 'response' AND vendor = 'claude' AND model = ?2
                    AND ts_ms >= ?3 AND ts_ms <= ?4
                    AND key IN (SELECT key FROM usage_obs WHERE session_id = ?1)",
            )
            .bind(&run.session_id)
            .bind(&model)
            .bind(run.start_ms)
            .bind(end)
            .fetch_all(&mut *tx)
            .await?;
            inputs.push(RunInput {
                session_id: run.session_id.clone(),
                start_ms: run.start_ms,
                snapshot_ts_ms: run.snapshot_ts_ms,
                counter: Counter {
                    input: run.input,
                    output: run.output,
                    cache_read: run.cache_read,
                    cache_write: run.cache_write,
                },
                cost_usd: run.cost_usd,
                window: window
                    .into_iter()
                    .map(|(k, i, w5, w1, cr, o)| {
                        (k, Tokens { input: i, cache_write_5m: w5, cache_write_1h: w1, cache_read: cr, output: o })
                    })
                    .collect(),
            });
        }
        let price = table.lookup(&model).map(|r| r.price);
        let outcomes = reconcile_model(&inputs, price.as_ref());
        for (run, o) in runs.iter().zip(outcomes) {
            stats.runs += 1;
            stats.carried_runs += o.carried as usize;
            stats.cost_state_usd += o.new_cost_usd;
            stats.adjustment_usd += o.adjustment;
            let end = run.snapshot_ts_ms.unwrap_or(i64::MAX);
            let ts = if end == i64::MAX { run.start_ms } else { end };
            let (day, hour, dow) = local.fields(ts);
            if o.residual.total() > 0 {
                stats.residual_rows += 1;
                stats.residual_tokens += o.residual.total();
                let res_cost = price.map(|p| cost(&p, &o.residual)).unwrap_or(0.0);
                insert_derived(
                    &mut tx,
                    &format!("residual:{}:{}:{}", run.session_id, run.start_ms, run.model),
                    "residual",
                    run,
                    ts,
                    (&day, hour, dow),
                    &o.residual,
                    res_cost,
                )
                .await?;
            }
            if o.adjustment.abs() > 1e-9 {
                insert_derived(
                    &mut tx,
                    &format!("adjust:{}:{}:{}", run.session_id, run.start_ms, run.model),
                    "adjustment",
                    run,
                    ts,
                    (&day, hour, dow),
                    &Tokens::default(),
                    o.adjustment,
                )
                .await?;
            }
        }
    }
    tx.commit().await?;
    super::store::meta_set(pool, "cost_state_usd", &format!("{}", stats.cost_state_usd)).await?;
    super::store::meta_set(pool, "cost_state_carried_runs", &stats.carried_runs.to_string()).await?;
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
        let c = Counter { input: 600, output: 800, cache_read: 60_000, cache_write: 1000 };
        let (r, c) = residual(&observed, &observed, c);
        assert_eq!(r.input, 500);
        assert_eq!(r.cache_read, 10_000);
        assert_eq!(r.output, 0, "never negative");
        assert_eq!(r.cache_write_5m + r.cache_write_1h, 0);
        assert_eq!((c.cache_write_5m, c.cache_write_1h), (0, 1000), "C's split follows the window");
    }

    #[test]
    fn unobserved_model_residual_is_all_of_cost_state() {
        let c = Counter { input: 1400, output: 12, cache_read: 0, cache_write: 0 };
        let (r, c) = residual(&Tokens::default(), &Tokens::default(), c);
        assert_eq!((r.input, r.output), (1400, 12));
        assert_eq!(c.input, 1400);
    }

    fn tok(input: i64, output: i64) -> Tokens {
        Tokens { input, output, ..Tokens::default() }
    }

    fn run(session: &str, start: i64, snap: i64, c: (i64, i64), usd: f64, window: &[(&str, Tokens)]) -> RunInput {
        RunInput {
            session_id: session.into(),
            start_ms: start,
            snapshot_ts_ms: Some(snap),
            counter: Counter { input: c.0, output: c.1, cache_read: 0, cache_write: 0 },
            cost_usd: usd,
            window: window.iter().map(|(k, t)| (k.to_string(), *t)).collect(),
        }
    }

    fn total_usd(runs: &[RunInput], out: &[RunOutcome], price: Option<&ModelPrice>, responses: &[Tokens]) -> f64 {
        let table = |t: &Tokens| price.map(|p| cost(p, t)).unwrap_or(0.0);
        responses.iter().map(table).sum::<f64>()
            + out.iter().map(|o| table(&o.residual) + o.adjustment).sum::<f64>()
            + 0.0 * runs.len() as f64
    }

    #[test]
    fn continued_counter_on_an_unpriced_model_is_counted_once() {
        // Parent run covers R1 ($1.00 by its counter). The fork's counter
        // continues it (same start) and adds R2: $1.50 in total. The model
        // has no table price, so every dollar comes from the adjustments.
        let r1 = tok(1000, 100);
        let r2 = tok(1000, 100);
        let runs = vec![
            run("s-parent", 1, 10, (1000, 100), 1.0, &[("r1", r1)]),
            run("s-fork", 1, 20, (2000, 200), 1.5, &[("r1", r1), ("r2", r2)]),
        ];
        let out = reconcile_model(&runs, None);
        assert!(out[1].carried && !out[0].carried);
        assert!((out[0].new_cost_usd - 1.0).abs() < 1e-12);
        assert!((out[1].new_cost_usd - 0.5).abs() < 1e-12);
        assert!((total_usd(&runs, &out, None, &[r1, r2]) - 1.5).abs() < 1e-12, "not 2.50");
        assert_eq!(out[1].residual.total(), 0, "the fork's new part is exactly R2");
    }

    #[test]
    fn overlapping_windows_with_drift_count_the_shared_response_once() {
        // Priced model with non-zero drift: Claude Code's estimates are 10%
        // above the table. R1 is in both windows; the second counter did not
        // continue the first (it restarted and counted R1 itself).
        let price = ModelPrice {
            input: 10.0,
            output: 50.0,
            cache_read: 1.0,
            cache_write_5m: 12.5,
            cache_write_1h: 20.0,
            long_context: None,
        };
        let r1 = tok(100_000, 10_000); // table $1.50
        let r2 = tok(200_000, 20_000); // table $3.00
        let runs = vec![
            run("s-a", 1, 10, (100_000, 10_000), 1.65, &[("r1", r1)]),
            run("s-b", 5, 20, (300_000, 30_000), 4.95, &[("r1", r1), ("r2", r2)]),
        ];
        let out = reconcile_model(&runs, Some(&price));
        // Different start times: a restarted counter. D = R1 alone, a third
        // of the table price, so the new part is R2 with $3.30.
        assert!((out[1].new_cost_usd - 3.30).abs() < 1e-9);
        let total = total_usd(&runs, &out, Some(&price), &[r1, r2]);
        assert!((total - (1.65 + 3.30)).abs() < 1e-9, "total {total}");
        // Each drift counted once: 0.15 + 0.30.
        assert!((out.iter().map(|o| o.adjustment).sum::<f64>() - 0.45).abs() < 1e-9);
    }

    #[test]
    fn restarted_counter_holding_only_carried_responses_adds_nothing() {
        // A later run whose smaller counter holds only responses the first
        // run already owns contributes no dollars and no residual.
        let r1 = tok(1000, 100);
        let r2 = tok(1000, 100);
        let runs = vec![
            run("s", 1, 30, (2000, 200), 2.0, &[("r1", r1), ("r2", r2)]),
            run("s", 5, 25, (1000, 100), 1.0, &[("r2", r2)]),
        ];
        let out = reconcile_model(&runs, None);
        assert!(out[1].carried);
        assert_eq!(out[1].new_cost_usd, 0.0);
        assert_eq!(out[1].residual.total(), 0);
        assert!((total_usd(&runs, &out, None, &[r1, r2]) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn restarted_larger_counter_subtracts_only_the_shared_response() {
        // Run A: R1 plus 500 background input tokens (title generation),
        // $1.50. Run B restarted its counter (new start time), repeats R1,
        // and adds R2 and R3: a larger total than A's, $3.00. Only R1 is
        // provably in both counters; A's background tokens are not in B's.
        let r1 = tok(1000, 100);
        let r2 = tok(1000, 100);
        let r3 = tok(1000, 100);
        let runs = vec![
            run("s-a", 1, 10, (1500, 100), 1.5, &[("r1", r1)]),
            run("s-b", 5, 30, (3000, 300), 3.0, &[("r1", r1), ("r2", r2), ("r3", r3)]),
        ];
        let out = reconcile_model(&runs, None);
        assert!(out[1].carried);
        assert_eq!(out[0].residual.input, 500, "A keeps its background tokens");
        assert_eq!(out[1].residual.total(), 0, "B's new part is exactly R2 + R3");
        // D = R1: 1100 of B's 3300 tokens, so $1.00 of B's $3.00.
        assert!((out[1].new_cost_usd - 2.0).abs() < 1e-12, "{}", out[1].new_cost_usd);
        assert!((total_usd(&runs, &out, None, &[r1, r2, r3]) - 3.5).abs() < 1e-12, "not 3.00");
    }

    #[test]
    fn same_start_counter_below_its_predecessor_is_not_a_continuation() {
        // Same start time, but B's input is below A's: B's counter cannot
        // hold A's, so only the shared R1 is subtracted.
        let r1 = tok(1000, 100);
        let r2 = tok(200, 200);
        let runs = vec![
            run("s-a", 1, 10, (1500, 100), 1.5, &[("r1", r1)]),
            run("s-b", 1, 20, (1200, 300), 1.5, &[("r1", r1), ("r2", r2)]),
        ];
        let out = reconcile_model(&runs, None);
        assert_eq!(out[0].residual.input, 500);
        assert_eq!(out[1].residual.total(), 0);
        // D = R1 = 1100 of 1500 tokens.
        assert!((out[1].new_cost_usd - 1.5 * 400.0 / 1500.0).abs() < 1e-12);
    }

    #[test]
    fn continuation_is_found_through_a_run_that_owns_no_response() {
        // Q owns R1. P continues Q's counter (same start), owns nothing, and
        // adds 500 background tokens. F continues P and adds R3. F's new
        // part is R3 only; P's background is already in P's counter.
        let r1 = tok(1000, 100);
        let r3 = tok(1000, 100);
        let runs = vec![
            run("s-q", 1, 10, (1000, 100), 1.0, &[("r1", r1)]),
            run("s-p", 1, 20, (1500, 100), 1.5, &[("r1", r1)]),
            run("s-f", 1, 30, (2500, 200), 2.5, &[("r1", r1), ("r3", r3)]),
        ];
        let out = reconcile_model(&runs, None);
        assert_eq!(out[1].residual.input, 500);
        assert_eq!(out[2].residual.total(), 0);
        assert!((out[2].new_cost_usd - 1.0).abs() < 1e-12, "{}", out[2].new_cost_usd);
        assert!((total_usd(&runs, &out, None, &[r1, r3]) - 2.5).abs() < 1e-12);
    }

    #[test]
    fn independent_runs_are_unchanged() {
        let r1 = tok(1000, 100);
        let runs = vec![run("s", 1, 10, (1500, 100), 0.8, &[("r1", r1)])];
        let out = reconcile_model(&runs, None);
        assert!(!out[0].carried);
        assert_eq!(out[0].residual.input, 500);
        assert!((out[0].adjustment - 0.8).abs() < 1e-12);
    }
}
