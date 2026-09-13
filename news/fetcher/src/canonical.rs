//! Mechanical item identity (ADR-031).
//!
//! Two functions decide whether two feed entries are "the same thing", and
//! neither of them asks a model:
//!
//! - [`canonicalize`] reduces a destination URL to a comparable form, so the
//!   same article submitted to Hacker News and to lobste.rs collapses to one
//!   row instead of two.
//! - [`title_tokens`] + [`same_event`] catch the case canonicalization can't:
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
/// The adverbial scaffolding at the end of the list ("again", "back", "even",
/// "here", "much", "still", "very") was added after the RubyGems near-miss
/// below: "…attacked RubyGems back in May" kept `back` as a content token,
/// which was enough to push a real duplicate under the Jaccard threshold.
const STOPWORDS: &[&str] = &[
    "a", "about", "after", "all", "an", "and", "any", "are", "as", "at", "be", "been", "but", "by",
    "can", "do", "does", "for", "from", "get", "has", "have", "how", "if", "in", "into", "is",
    "it", "its", "just", "like", "may", "more", "most", "new", "no", "not", "now", "of", "off",
    "on", "one", "only", "or", "our", "out", "over", "own", "she", "should", "so", "some", "than",
    "that", "the", "their", "them", "then", "there", "these", "they", "this", "to", "too", "two",
    "up", "use", "used", "using", "via", "was", "we", "were", "what", "when", "which", "who",
    "why", "will", "with", "would", "you", "your",
    "again", "back", "even", "here", "much", "still", "very",
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

/// Fold the inflections that two outlets pick differently for one event:
/// "attacked" / "attacks" / "attack", "releases" / "released" / "release".
///
/// Deliberately not a real stemmer. Three suffix rules and a silent final
/// `e`, each guarded on a minimum length so short words survive whole. It
/// only has to make two headlines about one event agree more often than it
/// makes two headlines about different events agree — a full Porter stemmer
/// would fold harder in both directions.
fn stem(token: &str) -> &str {
    let len = token.chars().count();
    // `-ss` and `-us` endings are not plurals ("business", "corpus", "bonus"),
    // so they keep their final s.
    let plural_safe = !token.ends_with("ss") && !token.ends_with("us");
    let base = if len >= 6 && token.ends_with("ing") {
        token.strip_suffix("ing").unwrap_or(token)
    } else if len >= 5 && token.ends_with("ed") {
        token.strip_suffix("ed").unwrap_or(token)
    } else if len >= 4 && plural_safe && token.ends_with('s') {
        token.strip_suffix('s').unwrap_or(token)
    } else {
        token
    };
    // Applied after the suffix rules so "release", "releases" and "released"
    // all land on "releas".
    if base.chars().count() >= 4 {
        base.strip_suffix('e').unwrap_or(base)
    } else {
        base
    }
}

/// Lowercase, split on anything non-alphanumeric, drop stopwords and
/// single characters, then [`stem`] what's left. The result is a set, so word
/// order and repetition don't affect the comparison.
pub fn title_tokens(title: &str) -> BTreeSet<String> {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|t| t.chars().count() > 1 && !STOPWORDS.contains(t))
        .map(|t| stem(t).to_string())
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

/// Overlap measured against the *shorter* title rather than against both.
/// Jaccard punishes asymmetry: a terse headline and a long one describing the
/// same event share every word the short one has, and still score low because
/// the long one's extra words inflate the union.
pub fn containment(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count() as f64;
    intersection / (a.len().min(b.len()) as f64)
}

/// Two titles describe the same event when their token sets overlap at least
/// this much. Tuned by hand against a week of aggregator traffic: 0.5 merged
/// distinct stories about one product, 0.7 let obvious rewrites through.
pub const TITLE_DUP_THRESHOLD: f64 = 0.6;

/// The second rule's bar. 0.75 would merge "Anthropic releases Claude Opus 5"
/// with "Anthropic releases Claude Haiku 5" — three of four tokens shared,
/// two genuinely different releases — so the bar sits above it.
pub const TITLE_CONTAINMENT_THRESHOLD: f64 = 0.8;

/// …and the containment rule only applies at all once this many content
/// tokens actually match. Without it, a three-word headline contained in a
/// longer one scores 1.0 on a coincidence.
pub const CONTAINMENT_MIN_SHARED: usize = 3;

/// Do these two titles describe the same event?
///
/// Two rules, either sufficient. Jaccard covers the general reworded-headline
/// case. Containment covers the asymmetric one that slipped through on
/// 2026-09-13, when "OpenAI agents attacked RubyGems back in May" and "OpenAI
/// agents carried out an undisclosed attack on RubyGems" both surfaced: every
/// content token of the shorter headline appears in the longer one, and the
/// longer one's four extra words held Jaccard under the bar.
pub fn same_event(a: &BTreeSet<String>, b: &BTreeSet<String>) -> bool {
    if jaccard(a, b) >= TITLE_DUP_THRESHOLD {
        return true;
    }
    a.intersection(b).count() >= CONTAINMENT_MIN_SHARED
        && containment(a, b) >= TITLE_CONTAINMENT_THRESHOLD
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

    fn tokens(words: &[&str]) -> BTreeSet<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tokens_drop_stopwords_and_punctuation_then_stem() {
        assert_eq!(
            title_tokens("The State of *Agentic* Coding, 2026!"),
            tokens(&["2026", "agentic", "cod", "stat"])
        );
    }

    #[test]
    fn inflections_of_one_word_collapse() {
        for family in [
            ["attack", "attacks", "attacked"],
            ["release", "releases", "released"],
            ["agent", "agents", "agents"],
        ] {
            let stems: BTreeSet<String> = family.iter().map(|w| stem(w).to_string()).collect();
            assert_eq!(stems.len(), 1, "{family:?} stemmed to {stems:?}");
        }
    }

    #[test]
    fn stemming_leaves_short_words_and_non_plurals_alone() {
        // A final s that isn't a plural, and words too short to be safely cut.
        for word in ["business", "corpus", "bonus", "css", "gas", "ios"] {
            assert_eq!(stem(word), word, "{word} should survive whole");
        }
        // …but a real plural of an -ss word still folds onto it.
        assert_eq!(stem("businesses"), stem("business"));
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
    fn the_rubygems_pair_that_both_surfaced_is_now_one_event() {
        // Verbatim from the 2026-09-13 morning run: Simon Willison's write-up
        // and the lobste.rs submission of the same disclosure, scored 0.72 and
        // 0.50, shown one above the other, both downvoted.
        let willison = title_tokens("OpenAI agents attacked RubyGems back in May");
        let lobsters = title_tokens("OpenAI agents carried out an undisclosed attack on RubyGems");
        assert!(
            same_event(&willison, &lobsters),
            "jaccard {:.3}, containment {:.3}",
            jaccard(&willison, &lobsters),
            containment(&willison, &lobsters),
        );
    }

    #[test]
    fn containment_catches_a_headline_contained_in_a_longer_one() {
        let short = title_tokens("Postgres 18 ships async I/O");
        let long = title_tokens("Postgres 18 ships async I/O after a decade of work, maintainers say");
        assert!(jaccard(&short, &long) < TITLE_DUP_THRESHOLD, "otherwise this proves nothing");
        assert!(same_event(&short, &long));
    }

    #[test]
    fn two_tokens_in_common_is_not_an_event() {
        // Each pair shares exactly the subject and nothing about what
        // happened to it. Containment must not fire on any of them.
        for (a, b) in [
            ("Claude Opus 5 released", "Claude Code adds hooks"),
            ("Postgres 18 ships async I/O", "Postgres 18 performance regression report"),
            ("Rust 1.94 stabilises async closures", "Rust Foundation names a new director"),
        ] {
            let (x, y) = (title_tokens(a), title_tokens(b));
            assert!(
                !same_event(&x, &y),
                "{a:?} vs {b:?}: jaccard {:.3}, containment {:.3}, shared {}",
                jaccard(&x, &y),
                containment(&x, &y),
                x.intersection(&y).count(),
            );
        }
    }

    #[test]
    fn two_releases_of_two_products_are_not_one_event() {
        // Containment 0.75 — three of the short headline's four tokens — for
        // two genuinely different releases. This is the pair that fixes the
        // containment bar at 0.8 rather than at the lowest value that
        // separates the RubyGems pair.
        let opus = title_tokens("Anthropic releases Claude Opus 5");
        let haiku =
            title_tokens("Anthropic releases Claude Haiku 5, a cheaper small model for agents");
        assert!(
            (containment(&opus, &haiku) - 0.75).abs() < 1e-9,
            "containment moved to {:.3} — re-tune the threshold with it",
            containment(&opus, &haiku)
        );
        assert!(!same_event(&opus, &haiku));
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
