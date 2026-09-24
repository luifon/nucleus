//! Model price table for the dollar estimate (ADR-034 §6).
//!
//! Claude Code, Codex and Nucleus run on subscriptions, so no invoice
//! corresponds to the dollar figures; they state what the same tokens would
//! cost through the vendors' metered APIs. Every built-in price records its
//! basis, source URL and retrieval date:
//!
//! - **API list price** — read from the vendor's own pricing page.
//! - **Third-party estimate** — the vendor publishes no price for the model;
//!   the figure comes from a third-party price listing. The surface marks
//!   every dollar amount that includes such a price.
//!
//! A rate the source does not list (a cache-read or cache-write rate) is
//! marked **inferred** and states the rule it was derived from.
//! `[usage.prices]` in `nucleus.toml` overrides or extends the table per
//! model (basis "nucleus.toml").
//!
//! A model is matched by exact id first, then by the longest table key that
//! is a prefix of the id at a `-` boundary, so a dated id such as
//! `claude-haiku-4-5-20251001` resolves to `claude-haiku-4-5`. A model with
//! no match is "unpriced": its tokens are counted and its cost is 0, and
//! the surface reports the unpriced token count separately.
//!
//! OpenAI long-context pricing: when one request's input (uncached + cached
//! + cache writes) is more than 272,000 tokens, the whole request is billed
//! at the long-context rates (2 x input, cached input and cache writes,
//! 1.5 x output on the models that have them). The price is applied per
//! response row, before any aggregation.

use crate::config::{LongContextPrice, ModelPrice};
use std::collections::BTreeMap;

/// The date the built-in prices were read from their sources.
pub const PRICES_AS_OF: &str = "2026-09-24";

const ANTHROPIC_URL: &str = "https://platform.claude.com/docs/en/about-claude/pricing";
const OPENAI_URL: &str = "https://developers.openai.com/api/docs/pricing";
/// Third-party listing for `codex-auto-review` (Bifrost LLM cost
/// calculator). The page states "Pricing data last updated: September 23,
/// 2026"; read on [`PRICES_AS_OF`].
const CODEX_AUTO_REVIEW_URL: &str =
    "https://www.getmaxim.ai/bifrost/llm-cost-calculator/provider/openai/model/codex-auto-review";

/// OpenAI's long-context threshold (input tokens per request).
pub const OPENAI_LONG_CONTEXT_ABOVE: i64 = 272_000;

/// How a price was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// The vendor's published API list price.
    ListPrice,
    /// Not published by the vendor; taken from a third-party listing.
    ThirdPartyEstimate,
    /// `[usage.prices]` in nucleus.toml.
    Config,
}

impl Basis {
    pub fn as_str(self) -> &'static str {
        match self {
            Basis::ListPrice => "list-price",
            Basis::ThirdPartyEstimate => "third-party-estimate",
            Basis::Config => "nucleus.toml",
        }
    }
}

/// Where one table entry comes from.
#[derive(Debug, Clone, PartialEq)]
pub struct Provenance {
    pub basis: Basis,
    pub source_url: Option<String>,
    /// Date the source was read (`YYYY-MM-DD`).
    pub retrieved: Option<String>,
    /// The cache-write rates are not listed by the source; they are derived.
    pub cache_write_inferred: bool,
    /// The cache-read rate is not listed by the source; it is derived.
    pub cache_read_inferred: bool,
}

struct BuiltIn {
    key: &'static str,
    price: ModelPrice,
    basis: Basis,
    url: &'static str,
    cache_write_inferred: bool,
    cache_read_inferred: bool,
}

const fn p(input: f64, output: f64, cache_read: f64, cache_write_5m: f64, cache_write_1h: f64) -> ModelPrice {
    ModelPrice { input, output, cache_read, cache_write_5m, cache_write_1h, long_context: None }
}

/// OpenAI model with long-context rates: 2 x input, cached input and cache
/// writes, 1.5 x output above [`OPENAI_LONG_CONTEXT_ABOVE`] (as listed on
/// the pricing page).
const fn openai_lc(input: f64, output: f64, cache_read: f64, cache_write: f64) -> ModelPrice {
    ModelPrice {
        input,
        output,
        cache_read,
        cache_write_5m: cache_write,
        cache_write_1h: cache_write,
        long_context: Some(LongContextPrice {
            above_input_tokens: OPENAI_LONG_CONTEXT_ABOVE,
            input: input * 2.0,
            output: output * 1.5,
            cache_read: cache_read * 2.0,
            cache_write: cache_write * 2.0,
        }),
    }
}

const fn listed(key: &'static str, price: ModelPrice, url: &'static str) -> BuiltIn {
    BuiltIn { key, price, basis: Basis::ListPrice, url, cache_write_inferred: false, cache_read_inferred: false }
}

/// USD per million tokens, read on [`PRICES_AS_OF`]. Every model that
/// appears in the Claude Code and Codex logs is listed; a new one shows up
/// on the usage surface as unpriced until it is added here or in
/// `[usage.prices]`.
const BUILT_IN: &[BuiltIn] = &[
    // Anthropic: list prices with both cache-write durations
    // (5-minute = 1.25 x input, 1-hour = 2 x input). Claude 4.6 and later
    // bill the full 1M context at the standard rates.
    listed("claude-fable-5-1", p(10.0, 50.0, 0.25, 12.5, 20.0), ANTHROPIC_URL),
    listed("claude-fable-5", p(10.0, 50.0, 1.0, 12.5, 20.0), ANTHROPIC_URL),
    listed("claude-opus-5-5", p(4.0, 20.0, 0.20, 5.0, 8.0), ANTHROPIC_URL),
    listed("claude-opus-5", p(5.0, 25.0, 0.50, 6.25, 10.0), ANTHROPIC_URL),
    listed("claude-opus-4-8", p(5.0, 25.0, 0.50, 6.25, 10.0), ANTHROPIC_URL),
    listed("claude-opus-4-7", p(5.0, 25.0, 0.50, 6.25, 10.0), ANTHROPIC_URL),
    listed("claude-opus-4-6", p(5.0, 25.0, 0.50, 6.25, 10.0), ANTHROPIC_URL),
    listed("claude-sonnet-5", p(2.0, 10.0, 0.20, 2.5, 4.0), ANTHROPIC_URL),
    listed("claude-sonnet-4-6", p(3.0, 15.0, 0.30, 3.75, 6.0), ANTHROPIC_URL),
    listed("claude-haiku-4-5", p(1.0, 5.0, 0.10, 1.25, 2.0), ANTHROPIC_URL),
    // OpenAI: list prices, one cache-write rate (both columns carry it),
    // long-context rates above 272K input tokens per request.
    listed("gpt-6-astra", openai_lc(10.0, 50.0, 1.0, 12.5), OPENAI_URL),
    listed("gpt-5.6-sol", openai_lc(4.0, 20.0, 0.40, 5.0), OPENAI_URL),
    listed("gpt-5.6-terra", openai_lc(2.0, 12.0, 0.20, 2.5), OPENAI_URL),
    listed("gpt-5.6-luna", openai_lc(0.20, 1.20, 0.02, 0.25), OPENAI_URL),
    // No cache-write price listed: inferred as billed like uncached input.
    BuiltIn {
        key: "gpt-5.3-codex",
        price: p(1.75, 14.0, 0.175, 1.75, 1.75),
        basis: Basis::ListPrice,
        url: OPENAI_URL,
        cache_write_inferred: true,
        cache_read_inferred: false,
    },
    // Codex's automatic approval-review model. OpenAI publishes no price;
    // input and output are a third-party estimate. Cached input (0.1 x
    // input) and cache writes (1.25 x input) are inferred from OpenAI's
    // ratios for its listed models.
    BuiltIn {
        key: "codex-auto-review",
        price: p(2.50, 15.0, 0.25, 3.125, 3.125),
        basis: Basis::ThirdPartyEstimate,
        url: CODEX_AUTO_REVIEW_URL,
        cache_write_inferred: true,
        cache_read_inferred: true,
    },
];

/// One resolved price.
#[derive(Debug, Clone, Copy)]
pub struct Resolved<'a> {
    pub key: &'a str,
    pub price: ModelPrice,
    pub provenance: &'a Provenance,
}

/// The effective table: built-in entries with config overrides merged over.
#[derive(Debug, Clone)]
pub struct PriceTable {
    entries: BTreeMap<String, (ModelPrice, Provenance)>,
}

impl PriceTable {
    pub fn new(overrides: &BTreeMap<String, ModelPrice>) -> Self {
        let mut entries: BTreeMap<String, (ModelPrice, Provenance)> = BUILT_IN
            .iter()
            .map(|b| {
                (
                    b.key.to_string(),
                    (
                        b.price,
                        Provenance {
                            basis: b.basis,
                            source_url: Some(b.url.to_string()),
                            retrieved: Some(PRICES_AS_OF.to_string()),
                            cache_write_inferred: b.cache_write_inferred,
                            cache_read_inferred: b.cache_read_inferred,
                        },
                    ),
                )
            })
            .collect();
        for (k, v) in overrides {
            entries.insert(
                normalize_model(k),
                (
                    *v,
                    Provenance {
                        basis: Basis::Config,
                        source_url: None,
                        retrieved: None,
                        cache_write_inferred: false,
                        cache_read_inferred: false,
                    },
                ),
            );
        }
        Self { entries }
    }

    /// Resolve a model id. `None` = unpriced.
    pub fn lookup(&self, model: &str) -> Option<Resolved<'_>> {
        let model = normalize_model(model);
        let hit = self.entries.get_key_value(&model).or_else(|| {
            self.entries
                .iter()
                .filter(|(k, _)| {
                    model.len() > k.len() && model.starts_with(k.as_str()) && model.as_bytes()[k.len()] == b'-'
                })
                .max_by_key(|(k, _)| k.len())
        });
        hit.map(|(k, (price, provenance))| Resolved { key: k.as_str(), price: *price, provenance })
    }
}

/// Model ids as recorded by the tools carry decorations the price does not
/// depend on: Claude Code's cost-state writes `claude-opus-5[1m]` for the
/// 1M-context variant of the same model. Strip a trailing `[...]`.
pub fn normalize_model(model: &str) -> String {
    let m = model.trim();
    match m.find('[') {
        Some(i) if m.ends_with(']') => m[..i].to_string(),
        _ => m.to_string(),
    }
}

/// Token counts of one priced unit. `input` excludes cached tokens on both
/// vendors (the Codex parser subtracts them), so the categories never
/// overlap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub cache_read: i64,
    pub output: i64,
}

impl Tokens {
    pub fn total(&self) -> i64 {
        self.input + self.cache_write_5m + self.cache_write_1h + self.cache_read + self.output
    }

    /// Input side of one request: uncached, cached and cache-written tokens.
    pub fn prompt(&self) -> i64 {
        self.input + self.cache_write_5m + self.cache_write_1h + self.cache_read
    }
}

/// Cost at the standard (short-context) rates. Used for aggregates of many
/// requests (cost-state totals, residuals), which have no per-request size.
pub fn cost(price: &ModelPrice, t: &Tokens) -> f64 {
    (t.input as f64 * price.input
        + t.cache_write_5m as f64 * price.cache_write_5m
        + t.cache_write_1h as f64 * price.cache_write_1h
        + t.cache_read as f64 * price.cache_read
        + t.output as f64 * price.output)
        / 1_000_000.0
}

/// Cost of ONE request: the long-context rates apply to the whole request
/// when its prompt is above the model's threshold.
pub fn request_cost(price: &ModelPrice, t: &Tokens) -> f64 {
    match price.long_context {
        Some(lc) if t.prompt() > lc.above_input_tokens => {
            (t.input as f64 * lc.input
                + (t.cache_write_5m + t.cache_write_1h) as f64 * lc.cache_write
                + t.cache_read as f64 * lc.cache_read
                + t.output as f64 * lc.output)
                / 1_000_000.0
        }
        _ => cost(price, t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_prefix_at_dash_boundary() {
        let t = PriceTable::new(&BTreeMap::new());
        assert_eq!(t.lookup("claude-opus-5").unwrap().key, "claude-opus-5");
        assert_eq!(t.lookup("claude-opus-5-5").unwrap().key, "claude-opus-5-5");
        assert_eq!(t.lookup("claude-haiku-4-5-20251001").unwrap().key, "claude-haiku-4-5");
        assert_eq!(t.lookup("claude-opus-5[1m]").unwrap().key, "claude-opus-5");
        assert!(t.lookup("claude-opus-50").is_none(), "prefix must end at a dash");
        assert!(t.lookup("some-unknown-model").is_none());
    }

    #[test]
    fn config_overrides_and_extends() {
        let mut o = BTreeMap::new();
        o.insert("claude-opus-5".to_string(), p(1.0, 2.0, 0.1, 1.0, 1.0));
        o.insert("review-model".to_string(), p(3.0, 3.0, 3.0, 3.0, 3.0));
        let t = PriceTable::new(&o);
        let r = t.lookup("claude-opus-5").unwrap();
        assert_eq!((r.price.input, r.provenance.basis), (1.0, Basis::Config));
        assert_eq!(t.lookup("review-model").unwrap().price.output, 3.0);
    }

    #[test]
    fn cost_of_a_synthetic_window() {
        // 1,000 uncached input, 100,000 1-hour cache writes, 1,000,000 cache
        // reads, 10,000 output on Opus 5:
        // 1000*5 + 100000*10 + 1e6*0.5 + 10000*25 = 1,755,000 → $1.755.
        let t = PriceTable::new(&BTreeMap::new());
        let price = t.lookup("claude-opus-5").unwrap().price;
        let tokens = Tokens { input: 1_000, cache_write_5m: 0, cache_write_1h: 100_000, cache_read: 1_000_000, output: 10_000 };
        assert!((cost(&price, &tokens) - 1.755).abs() < 1e-12);
    }

    #[test]
    fn openai_long_context_prices_the_whole_request() {
        let t = PriceTable::new(&BTreeMap::new());
        let sol = t.lookup("gpt-5.6-sol").unwrap().price;
        // At the threshold: standard rates.
        let at = Tokens { input: 72_000, cache_write_5m: 0, cache_write_1h: 0, cache_read: 200_000, output: 1_000 };
        assert_eq!(at.prompt(), 272_000);
        let std = (72_000.0 * 4.0 + 200_000.0 * 0.40 + 1_000.0 * 20.0) / 1e6;
        assert!((request_cost(&sol, &at) - std).abs() < 1e-12);
        // One token above: every category at the long-context rate.
        let above = Tokens { input: 72_001, ..at };
        let long = (72_001.0 * 8.0 + 200_000.0 * 0.80 + 1_000.0 * 30.0) / 1e6;
        assert!((request_cost(&sol, &above) - long).abs() < 1e-12);
        // Anthropic has no long-context tier in the table.
        let opus = t.lookup("claude-opus-5").unwrap().price;
        let big = Tokens { input: 900_000, ..Tokens::default() };
        assert_eq!(request_cost(&opus, &big), cost(&opus, &big));
    }

    #[test]
    fn provenance_separates_list_prices_from_estimates() {
        let t = PriceTable::new(&BTreeMap::new());
        let review = t.lookup("codex-auto-review").unwrap();
        assert_eq!(review.provenance.basis, Basis::ThirdPartyEstimate);
        assert!(review.provenance.source_url.as_deref().unwrap().starts_with("https://"));
        assert_eq!(review.provenance.retrieved.as_deref(), Some(PRICES_AS_OF));
        assert!(review.provenance.cache_write_inferred && review.provenance.cache_read_inferred);
        let codex = t.lookup("gpt-5.3-codex").unwrap();
        assert_eq!(codex.provenance.basis, Basis::ListPrice);
        assert!(codex.provenance.cache_write_inferred);
        let opus = t.lookup("claude-opus-5").unwrap();
        assert_eq!(opus.provenance.basis, Basis::ListPrice);
        assert!(!opus.provenance.cache_write_inferred);
    }
}
