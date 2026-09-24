//! Model price table for the dollar estimate (ADR-034).
//!
//! Every dollar figure on the usage surface is "an estimate at API list
//! price". Claude Code, Codex and Nucleus run on subscriptions, so no invoice
//! corresponds to these numbers; they state what the same tokens would cost
//! through the vendors' metered APIs.
//!
//! The built-in table below carries the list prices published on
//! [`PRICES_AS_OF`]. `[usage.prices]` in `nucleus.toml` overrides or extends
//! it per model. A model is matched by exact id first, then by the longest
//! table key that is a prefix of the id at a `-` boundary, so a dated id such
//! as `claude-haiku-4-5-20251001` resolves to `claude-haiku-4-5`. A model
//! with no match is "unpriced": its tokens are counted and its cost is 0,
//! and the surface reports the unpriced token count separately.

use crate::config::ModelPrice;
use std::collections::BTreeMap;

/// The date the built-in prices were read from the vendors' pricing pages.
pub const PRICES_AS_OF: &str = "2026-09-24";

const fn p(input: f64, output: f64, cache_read: f64, cache_write_5m: f64, cache_write_1h: f64) -> ModelPrice {
    ModelPrice { input, output, cache_read, cache_write_5m, cache_write_1h }
}

/// USD per million tokens, read on [`PRICES_AS_OF`]. Anthropic cache writes:
/// 5-minute = 1.25 x input, 1-hour = 2 x input. OpenAI bills one cache-write
/// rate (1.25 x input), so both columns carry it. Every model that appears in
/// the Claude Code and Codex logs is listed; a new one shows up on the usage
/// surface as unpriced until it is added here or in `[usage.prices]`.
const BUILT_IN: &[(&str, ModelPrice)] = &[
    // Anthropic list prices: Anthropic API pricing documentation, 2026-09-24.
    // Cross-checked against Claude Code's own cost-state estimates: over
    // 1,738 recorded runs the table differs from them by $0.37 in $3,654.
    ("claude-fable-5-1", p(10.0, 50.0, 0.25, 12.5, 20.0)),
    ("claude-fable-5", p(10.0, 50.0, 1.0, 12.5, 20.0)),
    ("claude-opus-5-5", p(4.0, 20.0, 0.20, 5.0, 8.0)),
    ("claude-opus-5", p(5.0, 25.0, 0.50, 6.25, 10.0)),
    ("claude-opus-4-8", p(5.0, 25.0, 0.50, 6.25, 10.0)),
    ("claude-opus-4-7", p(5.0, 25.0, 0.50, 6.25, 10.0)),
    ("claude-opus-4-6", p(5.0, 25.0, 0.50, 6.25, 10.0)),
    ("claude-sonnet-5", p(2.0, 10.0, 0.20, 2.5, 4.0)),
    ("claude-sonnet-4-6", p(3.0, 15.0, 0.30, 3.75, 6.0)),
    ("claude-haiku-4-5", p(1.0, 5.0, 0.10, 1.25, 2.0)),
    // OpenAI list prices: OpenAI API pricing page (developers.openai.com/api/docs/pricing), 2026-09-24.
    ("gpt-6-astra", p(10.0, 50.0, 1.0, 12.5, 12.5)),
    ("gpt-5.6-sol", p(4.0, 20.0, 0.40, 5.0, 5.0)),
    ("gpt-5.6-terra", p(2.0, 12.0, 0.20, 2.5, 2.5)),
    ("gpt-5.6-luna", p(0.20, 1.20, 0.02, 0.25, 0.25)),
    ("gpt-5.3-codex", p(1.75, 14.0, 0.175, 1.75, 1.75)),
    // Codex's automatic approval-review model. Not on OpenAI's pricing page;
    // input and output from a third-party API price aggregator (updated
    // 2026-09-23). Cached input and cache writes follow OpenAI's standard
    // ratios (0.1 x and 1.25 x input). Replace when OpenAI publishes a price.
    ("codex-auto-review", p(2.50, 15.0, 0.25, 3.125, 3.125)),
];

/// Where a resolved price came from, for the surface's price listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    BuiltIn,
    Config,
}

impl PriceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PriceSource::BuiltIn => "built-in",
            PriceSource::Config => "nucleus.toml",
        }
    }
}

/// The effective table: built-in entries with config overrides merged over.
#[derive(Debug, Clone)]
pub struct PriceTable {
    entries: BTreeMap<String, (ModelPrice, PriceSource)>,
}

impl PriceTable {
    pub fn new(overrides: &BTreeMap<String, ModelPrice>) -> Self {
        let mut entries: BTreeMap<String, (ModelPrice, PriceSource)> = BUILT_IN
            .iter()
            .map(|(k, v)| (k.to_string(), (*v, PriceSource::BuiltIn)))
            .collect();
        for (k, v) in overrides {
            entries.insert(normalize_model(k), (*v, PriceSource::Config));
        }
        Self { entries }
    }

    /// Resolve a model id to (table key, price, source). `None` = unpriced.
    pub fn lookup(&self, model: &str) -> Option<(&str, ModelPrice, PriceSource)> {
        let model = normalize_model(model);
        if let Some((k, (price, src))) = self.entries.get_key_value(&model) {
            return Some((k.as_str(), *price, *src));
        }
        self.entries
            .iter()
            .filter(|(k, _)| {
                model.len() > k.len()
                    && model.starts_with(k.as_str())
                    && model.as_bytes()[k.len()] == b'-'
            })
            .max_by_key(|(k, _)| k.len())
            .map(|(k, (price, src))| (k.as_str(), *price, *src))
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
}

pub fn cost(price: &ModelPrice, t: &Tokens) -> f64 {
    (t.input as f64 * price.input
        + t.cache_write_5m as f64 * price.cache_write_5m
        + t.cache_write_1h as f64 * price.cache_write_1h
        + t.cache_read as f64 * price.cache_read
        + t.output as f64 * price.output)
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_prefix_at_dash_boundary() {
        let t = PriceTable::new(&BTreeMap::new());
        assert_eq!(t.lookup("claude-opus-5").unwrap().0, "claude-opus-5");
        assert_eq!(t.lookup("claude-opus-5-5").unwrap().0, "claude-opus-5-5");
        assert_eq!(t.lookup("claude-haiku-4-5-20251001").unwrap().0, "claude-haiku-4-5");
        assert_eq!(t.lookup("claude-opus-5[1m]").unwrap().0, "claude-opus-5");
        assert!(t.lookup("claude-opus-50").is_none(), "prefix must end at a dash");
        assert!(t.lookup("some-unknown-model").is_none());
    }

    #[test]
    fn config_overrides_and_extends() {
        let mut o = BTreeMap::new();
        o.insert("claude-opus-5".to_string(), p(1.0, 2.0, 0.1, 1.0, 1.0));
        o.insert("review-model".to_string(), p(3.0, 3.0, 3.0, 3.0, 3.0));
        let t = PriceTable::new(&o);
        let (_, price, src) = t.lookup("claude-opus-5").unwrap();
        assert_eq!((price.input, src), (1.0, PriceSource::Config));
        assert_eq!(t.lookup("review-model").unwrap().1.output, 3.0);
    }

    #[test]
    fn cost_matches_claude_code_for_a_known_window() {
        // Opus 5 window whose Claude Code costUSD is 10.512306: all cache
        // writes 1-hour. The table reproduces it to the cent.
        let t = PriceTable::new(&BTreeMap::new());
        let (_, price, _) = t.lookup("claude-opus-5").unwrap();
        let tokens = Tokens {
            input: 656,
            cache_write_5m: 0,
            cache_write_1h: 234_462,
            cache_read: 12_025_562,
            output: 86_065,
        };
        assert!((cost(&price, &tokens) - 10.512306).abs() < 0.005);
    }
}
