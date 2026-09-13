//! Mechanical item identity (ADR-031).
//!
//! Two functions decide whether two feed entries are "the same thing", and
//! neither of them asks a model:
//!
//! - [`canonicalize`] reduces a destination URL to a comparable form, so the
//!   same article submitted to Hacker News and to lobste.rs collapses to one
//!   row instead of two.
//! - [`title_tokens`] + [`jaccard`] catch the case canonicalization can't:
//!   the same event written up by three outlets under three URLs.

use std::collections::BTreeSet;
use url::Url;

/// Query parameters that identify a referral, never a resource. Dropped
/// before comparison; `utm_*` is handled by prefix.
const TRACKING_PARAMS: &[&str] = &[
    "fbclid", "gclid", "dclid", "msclkid", "mc_cid", "mc_eid", "igshid", "ref_src", "ref_url",
    "_hsenc", "_hsmi", "vero_id", "yclid",
];

/// Words carrying no topical signal. Dropped before the token-set overlap so
/// "OpenAI ships a new model" and "A new model from OpenAI" score on the
/// nouns rather than on the scaffolding.
const STOPWORDS: &[&str] = &[
    "a", "about", "after", "all", "an", "and", "any", "are", "as", "at", "be", "been", "but", "by",
    "can", "do", "does", "for", "from", "get", "has", "have", "how", "if", "in", "into", "is",
    "it", "its", "just", "like", "may", "more", "most", "new", "no", "not", "now", "of", "off",
    "on", "one", "only", "or", "our", "out", "over", "own", "she", "should", "so", "some", "than",
    "that", "the", "their", "them", "then", "there", "these", "they", "this", "to", "too", "two",
    "up", "use", "used", "using", "via", "was", "we", "were", "what", "when", "which", "who",
    "why", "will", "with", "would", "you", "your",
];

fn is_tracking(key: &str) -> bool {
    key.starts_with("utm_") || TRACKING_PARAMS.contains(&key)
}

/// Reduce a URL to the form two feeds would have to agree on to be pointing
/// at the same page: lowercase host without `www.`, no fragment, no tracking
/// parameters, no trailing slash. Everything else is left alone — paths are
/// case-sensitive and a query string like `?id=45228` IS the resource for an
/// aggregator's own discussion pages.
///
/// A URL that doesn't parse is returned trimmed and lowercased, so an
/// unparseable string still compares equal to itself.
pub fn canonicalize(raw: &str) -> String {
    let trimmed = raw.trim();
    let Ok(mut u) = Url::parse(trimmed) else {
        return trimmed.trim_end_matches('/').to_lowercase();
    };

    u.set_fragment(None);

    if let Some(host) = u.host_str() {
        let lower = host.to_lowercase();
        let bare = lower.strip_prefix("www.").unwrap_or(&lower).to_string();
        let _ = u.set_host(Some(&bare));
    }

    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !is_tracking(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        u.set_query(None);
    } else {
        let mut pairs = u.query_pairs_mut();
        pairs.clear();
        for (k, v) in &kept {
            pairs.append_pair(k, v);
        }
        drop(pairs);
    }

    let path = u.path().to_string();
    if path.len() > 1 && path.ends_with('/') {
        u.set_path(path.trim_end_matches('/'));
    }

    u.as_str().to_string()
}

/// Lowercase, split on anything non-alphanumeric, drop stopwords and
/// single characters. The result is a set, so word order and repetition
/// don't affect the comparison.
pub fn title_tokens(title: &str) -> BTreeSet<String> {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|t| t.chars().count() > 1 && !STOPWORDS.contains(t))
        .map(|t| t.to_string())
        .collect()
}

/// Token-set overlap in [0,1]. Empty on either side scores 0 — a title that
/// reduces to nothing suppresses nothing.
pub fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    intersection / union
}

/// Two titles describe the same event when their token sets overlap at least
/// this much. Tuned by hand against a week of aggregator traffic: 0.5 merged
/// distinct stories about one product, 0.7 let obvious rewrites through.
pub const TITLE_DUP_THRESHOLD: f64 = 0.6;

/// Convenience: do these two titles describe the same event?
pub fn same_event(a: &BTreeSet<String>, b: &BTreeSet<String>) -> bool {
    jaccard(a, b) >= TITLE_DUP_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tracking_params_and_fragment() {
        assert_eq!(
            canonicalize("https://example.com/post?utm_source=hn&utm_medium=feed&fbclid=abc#intro"),
            "https://example.com/post"
        );
    }

    #[test]
    fn keeps_meaningful_query_params() {
        // An aggregator's discussion page IS its query string.
        assert_eq!(
            canonicalize("https://news.ycombinator.com/item?id=45228&utm_source=rss"),
            "https://news.ycombinator.com/item?id=45228"
        );
    }

    #[test]
    fn lowercases_host_drops_www_and_trailing_slash() {
        assert_eq!(
            canonicalize("HTTPS://WWW.Example.COM/Blog/Post/"),
            "https://example.com/Blog/Post"
        );
    }

    #[test]
    fn path_case_is_preserved() {
        // Many static-site hosts serve case-sensitive paths; folding them
        // would merge two real pages.
        assert_ne!(canonicalize("https://a.dev/Foo"), canonicalize("https://a.dev/foo"));
    }

    #[test]
    fn root_urls_agree_with_and_without_slash() {
        assert_eq!(canonicalize("https://example.com"), canonicalize("https://example.com/"));
    }

    #[test]
    fn same_article_from_two_aggregators_collapses() {
        let via_hn = canonicalize("https://simonwillison.net/2026/Sep/12/agents/?utm_source=hn");
        let via_lobsters = canonicalize("https://www.simonwillison.net/2026/Sep/12/agents/");
        assert_eq!(via_hn, via_lobsters);
    }

    #[test]
    fn unparseable_input_is_stable() {
        assert_eq!(canonicalize("  Not A URL/ "), "not a url");
    }

    #[test]
    fn tokens_drop_stopwords_and_punctuation() {
        assert_eq!(
            title_tokens("The State of *Agentic* Coding, 2026!"),
            ["2026", "agentic", "coding", "state"]
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn reworded_headline_is_the_same_event() {
        let a = title_tokens("Anthropic releases Claude Opus 5 with a 1M context window");
        let b = title_tokens("Claude Opus 5 released by Anthropic, 1M context window");
        assert!(same_event(&a, &b), "jaccard was {}", jaccard(&a, &b));
    }

    #[test]
    fn different_stories_about_one_product_are_not_suppressed() {
        // Explicitly required by the no-topic-caps rule: a release and a
        // limits change on the same product must both survive.
        let release = title_tokens("Anthropic releases Claude Opus 5");
        let limits = title_tokens("Anthropic tightens weekly rate limits for Max subscribers");
        assert!(!same_event(&release, &limits), "jaccard was {}", jaccard(&release, &limits));
    }

    #[test]
    fn empty_token_sets_never_match() {
        assert!(!same_event(&title_tokens("The and of"), &title_tokens("A to the")));
    }

    #[test]
    fn jaccard_is_symmetric_and_bounded() {
        let a = title_tokens("rust async runtime benchmark");
        let b = title_tokens("benchmark of rust async runtimes");
        let j = jaccard(&a, &b);
        assert!((0.0..=1.0).contains(&j));
        assert!((j - jaccard(&b, &a)).abs() < f64::EPSILON);
    }
}
