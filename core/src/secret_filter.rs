//! Credential redaction for text Nucleus sends from Rust (ADR-033).
//!
//! The WhatsApp bot filters every outbound row itself
//! (`messaging/whatsapp/src/secret_filter.ts`). Text that Rust sends
//! directly — a task result posted to Discord — passes through this module
//! first. It applies the credential part of the same rules: `.env` values
//! whose key names a credential, and credential-shaped tokens. Both
//! implementations run the vectors in `core/testdata/credential_vectors.json`.

use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

const REDACTED: &str = "[redacted]";

/// How a shape's match is redacted.
#[derive(Clone, Copy)]
enum Part {
    /// The whole match.
    Whole,
    /// Group 2; group 1 (a label such as `password=`) stays.
    Value,
    /// Group 2 when it looks random ([`looks_random`]); group 1 stays.
    RandomValue,
}

struct Shape {
    re: Regex,
    kind: &'static str,
    part: Part,
}

/// Credential-shaped tokens, most specific first. Mirrors
/// `CREDENTIAL_SHAPES` in secret_filter.ts.
fn shapes() -> &'static [Shape] {
    static SHAPES: OnceLock<Vec<Shape>> = OnceLock::new();
    SHAPES.get_or_init(|| {
        let s = |re: &str, kind, part| Shape { re: Regex::new(re).expect("credential shape"), kind, part };
        vec![
            s(
                r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
                "credential-private-key",
                Part::Whole,
            ),
            s(r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}", "credential-jwt", Part::Whole),
            s(r"\bsk-(?:ant-)?[A-Za-z0-9_-]{20,}", "credential-api-key", Part::Whole),
            s(r"\b[sr]k_(?:live|test)_[A-Za-z0-9]{16,}", "credential-stripe", Part::Whole),
            s(r"\bAIza[0-9A-Za-z_-]{35}", "credential-google", Part::Whole),
            s(r"\bglpat-[A-Za-z0-9_-]{20,}", "credential-gitlab", Part::Whole),
            s(r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}", "credential-github", Part::Whole),
            s(r"\bgithub_pat_[A-Za-z0-9_]{30,}", "credential-github", Part::Whole),
            s(r"\bxox[abeoprs]-[A-Za-z0-9-]{10,}", "credential-slack", Part::Whole),
            s(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", "credential-aws", Part::Whole),
            s(
                r#"(?i)(aws[A-Za-z0-9_ -]{0,20}secret[A-Za-z0-9_ -]{0,20}["']?\s*[:=]\s*["']?)([A-Za-z0-9/+=]{40})"#,
                "credential-aws",
                Part::Value,
            ),
            s(r"\bBearer\s+[A-Za-z0-9._~+/=-]{20,}", "credential-bearer", Part::Whole),
            s(
                r#"(?i)((?:\b|_)(?:password|passwd|pwd|passphrase|secret|client[_-]?secret|token|access[_-]?token|refresh[_-]?token|auth[_-]?token|api[_-]?key|apikey|access[_-]?key|secret[_-]?key|private[_-]?key)\b["']?\s*[:=]\s*["']?)([^\s"'<>,;]{8,})"#,
                "credential-labeled",
                Part::RandomValue,
            ),
        ]
    })
}

/// A labeled value counts as a secret when it is at least 8 characters,
/// mixes at least two character classes (lowercase, uppercase, digit,
/// other), and has a Shannon entropy of at least 3 bits per character.
/// Mirrors `looksRandom` in secret_filter.ts.
pub fn looks_random(v: &str) -> bool {
    let chars: Vec<char> = v.chars().collect();
    if chars.len() < 8 {
        return false;
    }
    let classes = [
        chars.iter().any(|c| c.is_ascii_lowercase()),
        chars.iter().any(|c| c.is_ascii_uppercase()),
        chars.iter().any(|c| c.is_ascii_digit()),
        chars.iter().any(|c| !c.is_ascii_alphanumeric()),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    classes >= 2 && entropy(&chars) >= 3.0
}

fn entropy(chars: &[char]) -> f64 {
    let mut counts = std::collections::HashMap::new();
    for c in chars {
        *counts.entry(*c).or_insert(0usize) += 1;
    }
    let n = chars.len() as f64;
    counts.values().map(|&k| {
        let p = k as f64 / n;
        -p * p.log2()
    }).sum()
}

/// Result of [`CredentialRules::redact`].
pub struct Redacted {
    pub text: String,
    /// One entry per redaction: its kind, never the value.
    pub hits: Vec<&'static str>,
}

/// `.env` credential values plus the credential shapes.
#[derive(Default)]
pub struct CredentialRules {
    values: Vec<String>,
}

const BENIGN: &[&str] = &["info", "debug", "warn", "error", "trace", "true", "false", "claude"];

fn credential_key(key: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|PRIVATE|COOKIE|(^|_)KEY($|_)|API_?KEY|AUTH)").unwrap()
    })
    .is_match(key)
}

impl CredentialRules {
    /// Credential values of `.env` text (keys that name a credential; values
    /// of at least 6 characters). Mirrors `buildRules` in secret_filter.ts.
    pub fn from_env_text(env: &str) -> Self {
        let mut values = Vec::new();
        for line in env.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let key = k.trim().trim_start_matches("export ").trim();
            if !credential_key(key) {
                continue;
            }
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|x| x.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
                .unwrap_or(v);
            for part in v.split(',').map(str::trim) {
                if part.chars().count() >= 6 && !BENIGN.contains(&part.to_lowercase().as_str()) {
                    values.push(part.to_string());
                }
            }
        }
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        CredentialRules { values }
    }

    /// Rules from `<workspace>/.env` (none when it is missing).
    pub fn from_workspace(workspace_root: &Path) -> Self {
        Self::from_env_text(&std::fs::read_to_string(workspace_root.join(".env")).unwrap_or_default())
    }

    /// Redact every credential in `text`.
    pub fn redact(&self, text: &str) -> Redacted {
        let mut hits = Vec::new();
        let mut out = text.to_string();
        for v in &self.values {
            let n = out.matches(v.as_str()).count();
            if n > 0 {
                out = out.replace(v.as_str(), REDACTED);
                hits.extend(std::iter::repeat("env-credential").take(n));
            }
        }
        for shape in shapes() {
            out = shape
                .re
                .replace_all(&out, |c: &regex::Captures| match shape.part {
                    Part::Whole => {
                        hits.push(shape.kind);
                        REDACTED.to_string()
                    }
                    Part::Value => {
                        hits.push(shape.kind);
                        format!("{}{REDACTED}", &c[1])
                    }
                    Part::RandomValue if looks_random(&c[2]) => {
                        hits.push(shape.kind);
                        format!("{}{REDACTED}", &c[1])
                    }
                    Part::RandomValue => c[0].to_string(),
                })
                .into_owned();
        }
        Redacted { text: out, hits }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vector text is stored in pieces (`{"repeat": s, "times": n}` repeats
    /// `s`), so no committed file contains a contiguous credential-shaped
    /// string that secret scanners would flag.
    pub(crate) fn assemble(parts: &serde_json::Value) -> String {
        parts
            .as_array()
            .unwrap()
            .iter()
            .map(|p| match p {
                serde_json::Value::String(s) => s.clone(),
                o => o["repeat"].as_str().unwrap().repeat(o["times"].as_u64().unwrap() as usize),
            })
            .collect()
    }

    #[test]
    fn shared_vectors() {
        let raw = include_str!("../testdata/credential_vectors.json");
        let vectors: Vec<serde_json::Value> = serde_json::from_str(raw).unwrap();
        let rules = CredentialRules::default();
        assert!(vectors.len() >= 12);
        for v in vectors {
            let name = v["name"].as_str().unwrap();
            let text = assemble(&v["text"]);
            let r = rules.redact(&text);
            match v["kind"].as_str() {
                Some(kind) => {
                    assert!(r.hits.contains(&kind), "{name}: hits {:?} in {:?}", r.hits, r.text);
                    let secret = assemble(&v["secret"]);
                    assert!(!r.text.contains(&secret), "{name}: {:?}", r.text);
                }
                None => assert!(r.hits.is_empty(), "{name}: unexpected {:?} in {:?}", r.hits, r.text),
            }
            if let Some(keep) = v["keep"].as_str() {
                assert!(r.text.contains(keep), "{name}: {:?} lost {keep:?}", r.text);
            }
        }
    }

    #[test]
    fn env_credentials_are_redacted() {
        let rules = CredentialRules::from_env_text(
            "# c\nDISCORD_BOT_TOKEN=\"tok-abcdefghijklmnop\"\nNUCLEUS_TZ=Europe/Oslo\nAPI_KEY=short\n",
        );
        let r = rules.redact("a tok-abcdefghijklmnop b Europe/Oslo");
        assert_eq!(r.text, "a [redacted] b Europe/Oslo");
        assert_eq!(r.hits, vec!["env-credential"]);
    }
}
