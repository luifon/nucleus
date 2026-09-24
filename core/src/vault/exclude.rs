//! Exclusion rules shared by vault search and vault check (ADR-035).
//!
//! Two layers:
//!
//! 1. **Path globs.** A fixed floor ([`FLOOR`]) that configuration cannot
//!    remove, plus `[vault_search] exclude` from nucleus.toml. The floor
//!    covers dot folders (`.obsidian/`, `.trash/`, `.git/`), file and
//!    folder names that look like credential stores, and the homelab area,
//!    where operators keep service logins.
//! 2. **Content.** [`looks_like_credentials`], a built-in detector that
//!    cannot be turned off, plus an optional extra regex from
//!    `[vault_search] credential_content_regex`. A note that holds a secret
//!    is excluded wherever it lives, so a password or API key pasted into an
//!    ordinary note still stays out of search results.
//!
//! Glob syntax: `*` matches within one path component, `**` matches any
//! number of components, `?` matches one character, `[...]` is a character
//! class. Matching is case-insensitive. A pattern without `/` is matched
//! against every single component of the path (gitignore semantics), so
//! `*secret*` excludes both `a/secret-notes.md` and `secrets/a.md`.

use crate::config::VaultSearchConfig;
use anyhow::{Context, Result};
use regex::Regex;

/// Always excluded, whatever nucleus.toml says.
pub const FLOOR: &[&str] = &[
    ".*",
    "*credential*",
    "*credencia*",
    "*password*",
    "*passwd*",
    "*senha*",
    "*secret*",
    "*api-key*",
    "*apikey*",
    "*.pem",
    "*.key",
    "homelab",
];

#[derive(Debug, Clone)]
pub struct Exclusions {
    globs: Vec<Glob>,
    content: Option<Regex>,
    /// Stable description of the rules, stored with the index so a config
    /// change forces a full rebuild.
    fingerprint: String,
}

#[derive(Debug, Clone)]
struct Glob {
    re: Regex,
    /// Pattern had no `/`: match any single component.
    component: bool,
}

impl Exclusions {
    pub fn from_config(cfg: &VaultSearchConfig) -> Result<Self> {
        Self::new(&cfg.exclude, &cfg.credential_content_regex)
    }

    pub fn new(extra: &[String], content_regex: &str) -> Result<Self> {
        let mut globs = Vec::new();
        let mut patterns: Vec<String> = FLOOR.iter().map(|s| s.to_string()).collect();
        patterns.extend(extra.iter().cloned());
        for p in &patterns {
            globs.push(Glob::compile(p).with_context(|| format!("vault exclude glob {p:?}"))?);
        }
        let content = if content_regex.trim().is_empty() {
            None
        } else {
            Some(
                Regex::new(&format!("(?im){content_regex}"))
                    .context("[vault_search] credential_content_regex")?,
            )
        };
        let fingerprint =
            format!("{DETECTOR_VERSION}\n{}\n{}", patterns.join("\n"), content_regex);
        Ok(Self { globs, content, fingerprint })
    }

    /// True when a vault-relative path (forward slashes) is excluded by a
    /// glob. Directories are tested with the same function, so a walk can
    /// skip a whole excluded folder.
    pub fn path_excluded(&self, rel: &str) -> bool {
        let rel = rel.trim_start_matches('/');
        self.globs.iter().any(|g| g.matches(rel))
    }

    /// True when a note's text looks like it holds credentials.
    pub fn content_excluded(&self, text: &str) -> bool {
        looks_like_credentials(text) || self.content.as_ref().is_some_and(|re| re.is_match(text))
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

/// Bumped whenever [`looks_like_credentials`] changes, so existing indexes
/// are rebuilt under the new rule.
const DETECTOR_VERSION: &str = "credential-detector-v1";

/// Words that name a secret when they label a value.
const SECRET_WORDS: &[&str] = &[
    "password", "passwords", "passwd", "passphrase", "senha", "senhas", "secret",
    "token", "apikey", "pin", "credential", "credentials", "credencial", "credenciais",
];

/// Two-word labels that name a secret (`chave de API` is folded to
/// `chave api` by [`normalize_label`]).
const SECRET_PAIRS: &[(&str, &str)] = &[
    ("api", "key"),
    ("api", "keys"),
    ("chave", "api"),
    ("access", "key"),
    ("private", "key"),
    ("client", "secret"),
    ("access", "token"),
];

/// Well-known API key formats and PEM private keys, matched anywhere.
fn known_key_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
            r"|\bsk-[A-Za-z0-9_-]{20,}",
            r"|\bgh[pousr]_[A-Za-z0-9]{30,}",
            r"|\bgithub_pat_[A-Za-z0-9_]{30,}",
            r"|\bglpat-[A-Za-z0-9_-]{20,}",
            r"|\bxox[abprs]-[A-Za-z0-9-]{10,}",
            r"|\bAKIA[0-9A-Z]{16}\b",
            r"|\bAIza[0-9A-Za-z_-]{35}",
        ))
        .unwrap()
    })
}

/// Built-in credential detector. A note is a credential note when:
///
/// 1. it contains a well-known key format or a PEM private key; or
/// 2. a line is `label: value` (or `label = value`) where the label, with
///    markdown removed, is exactly a secret word or pair (`password: x`,
///    `- **Senha:** x`, `API_KEY=x`) and the value is one token of 3 or
///    more characters; or
/// 3. the label has at most five words and contains a secret word or pair
///    (`API key on file:`, `senha do roteador:`), and the value — on the same
///    line, or on the next non-empty line when the same line has none — is a
///    single token of 6 or more characters with both letters and digits.
///
/// Rule 3 keeps prose such as `Token budget: 5000 per call` or
/// `Max tokens: 4096` searchable, and still catches a key written on the
/// line below its label.
pub fn looks_like_credentials(text: &str) -> bool {
    if known_key_re().is_match(text) {
        return true;
    }
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        // Every `:` / `=` is a candidate separator. Its label is the text
        // since the previous separator or sentence end, so
        // `Status: registered. API key on file:` yields the label
        // `API key on file`.
        let seps: Vec<usize> = line.match_indices([':', '=']).map(|(k, _)| k).collect();
        for &sep in &seps {
            let start = line[..sep].rfind([':', '=', '.', ';', ',']).map(|k| k + 1).unwrap_or(0);
            if label_value_is_secret(&line[start..sep], &line[sep + 1..], &lines[i + 1..]) {
                return true;
            }
        }
    }
    false
}

/// Rules 2 and 3 for one `label: value` candidate. `following` is the text
/// after the line, for a value written on the next non-empty line.
fn label_value_is_secret(raw_label: &str, raw_value: &str, following: &[&str]) -> bool {
    let label = normalize_label(raw_label);
    if label.is_empty() {
        return false;
    }
    let words: Vec<&str> = label.split_whitespace().collect();
    let value = clean_value(raw_value);
    let exact = match words.as_slice() {
        [w] => SECRET_WORDS.contains(w),
        [a, b] => SECRET_PAIRS.contains(&(*a, *b)),
        _ => false,
    };
    // One token: `password: hunter2`. Prose after the label (`Token: create
    // one under Settings`) is instructions, not a secret.
    if exact && value.chars().count() >= 3 && !value.chars().any(char::is_whitespace) {
        return true;
    }
    if words.len() > 5 || !mentions_secret(&words) {
        return false;
    }
    let value = if value.is_empty() {
        following.iter().map(|l| clean_value(l)).find(|v| !v.is_empty()).unwrap_or_default()
    } else {
        value
    };
    is_secret_token(&value)
}

/// Lowercase label words with markdown and punctuation removed; filler
/// words (`de`, `do`, `da`, `of`, `the`, `my`, `meu`, `minha`) dropped so
/// `chave de API` reads as `chave api`.
fn normalize_label(raw: &str) -> String {
    let lower = raw.to_lowercase();
    let mut words: Vec<&str> = Vec::new();
    for w in lower.split(|c: char| !c.is_alphanumeric()) {
        if w.is_empty() || matches!(w, "de" | "do" | "da" | "of" | "the" | "my" | "meu" | "minha") {
            continue;
        }
        words.push(w);
    }
    words.join(" ")
}

fn mentions_secret(words: &[&str]) -> bool {
    words.iter().any(|w| SECRET_WORDS.contains(w))
        || words.windows(2).any(|p| SECRET_PAIRS.contains(&(p[0], p[1])))
}

/// The value with list markers, emphasis, quotes and backticks removed.
fn clean_value(raw: &str) -> String {
    raw.trim()
        .trim_start_matches(['-', '*', '>', '+', ' ', '\t'])
        .trim_matches(['*', '`', '"', '\'', ' ', '\t'])
        .to_string()
}

fn is_secret_token(v: &str) -> bool {
    v.chars().count() >= 6
        && !v.chars().any(char::is_whitespace)
        && v.chars().any(|c| c.is_ascii_alphabetic())
        && v.chars().any(|c| c.is_ascii_digit())
}

impl Glob {
    fn compile(pattern: &str) -> Result<Self> {
        let pattern = pattern.trim().trim_start_matches('/');
        let component = !pattern.contains('/');
        let re = Regex::new(&format!("(?i)^{}$", glob_to_regex(pattern)))?;
        Ok(Self { re, component })
    }

    fn matches(&self, rel: &str) -> bool {
        if self.component {
            rel.split('/').any(|c| self.re.is_match(c))
        } else {
            self.re.is_match(rel)
        }
    }
}

/// Translate a glob into a regex body (unanchored).
pub fn glob_to_regex(glob: &str) -> String {
    let chars: Vec<char> = glob.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    // `**/` = zero or more whole components; bare `**` = anything.
                    if chars.get(i + 2) == Some(&'/') {
                        out.push_str("(?:.*/)?");
                        i += 3;
                    } else {
                        out.push_str(".*");
                        i += 2;
                    }
                    continue;
                }
                out.push_str("[^/]*");
            }
            '?' => out.push_str("[^/]"),
            '[' => {
                // Copy a character class through verbatim when it closes.
                if let Some(end) = chars[i + 1..].iter().position(|&x| x == ']') {
                    let class: String = chars[i + 1..i + 1 + end].iter().collect();
                    let class = class.strip_prefix('!').map(|r| format!("^{r}")).unwrap_or(class);
                    out.push('[');
                    out.push_str(&class.replace('\\', "\\\\"));
                    out.push(']');
                    i += end + 2;
                    continue;
                }
                out.push_str("\\[");
            }
            _ => out.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    out
}

/// A list of globs used for exemptions (orphan / frontmatter exemptions in
/// `[vault_check]`). Same syntax as the exclusions.
#[derive(Debug, Clone)]
pub struct GlobSet {
    globs: Vec<Glob>,
}

impl GlobSet {
    pub fn new(patterns: &[String]) -> Result<Self> {
        let globs = patterns
            .iter()
            .map(|p| Glob::compile(p).with_context(|| format!("glob {p:?}")))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { globs })
    }

    pub fn matches(&self, rel: &str) -> bool {
        self.globs.iter().any(|g| g.matches(rel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(extra: &[&str]) -> Exclusions {
        let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        Exclusions::new(&extra, "").unwrap()
    }

    #[test]
    fn floor_excludes_dot_folders_and_credential_names() {
        let e = ex(&[]);
        assert!(e.path_excluded(".obsidian/app.json"));
        assert!(e.path_excluded(".trash/old.md"));
        assert!(e.path_excluded("4-Areas/Servers/.hidden/x.md"));
        assert!(e.path_excluded("4-Areas/Homelab/media-server.md"));
        assert!(e.path_excluded("4-Areas/homelab/README.md"));
        assert!(e.path_excluded("5-Resources/Credentials/bank.md"));
        assert!(e.path_excluded("0-Inbox/wifi-passwords.md"));
        assert!(e.path_excluded("0-Inbox/Senhas.md"));
        assert!(e.path_excluded("3-Projects/X/deploy.pem"));
        assert!(!e.path_excluded("3-Projects/Alpha/index.md"));
        // The name floor is deliberately broad: a false positive costs one
        // note missing from search, a false negative leaks a credential.
        assert!(e.path_excluded("6-Slipbox/secretary-problem.md"));
    }

    #[test]
    fn configured_globs_add_to_the_floor() {
        let e = ex(&["**/attachments/**", "7-Archives/Old/**"]);
        assert!(e.path_excluded("3-Projects/Alpha/attachments/diagram.md"));
        assert!(e.path_excluded("7-Archives/Old/a/b.md"));
        assert!(!e.path_excluded("7-Archives/New/a.md"));
        // The floor is still there.
        assert!(e.path_excluded(".obsidian/x"));
    }

    #[test]
    fn glob_translation() {
        let g = GlobSet::new(&["2-Daily-Notes/**".into(), "README.md".into(), "*/index.md".into()]).unwrap();
        assert!(g.matches("2-Daily-Notes/2026-01-01.md"));
        assert!(g.matches("2-Daily-Notes/2026/01.md"));
        assert!(g.matches("0-Inbox/README.md"));
        assert!(g.matches("readme.md"));
        assert!(g.matches("3-Projects/index.md"));
        assert!(!g.matches("3-Projects/Alpha/index.md"));
        assert!(!g.matches("3-Projects/Alpha/notes.md"));
    }

    #[test]
    fn detector_catches_credentials() {
        let e = ex(&[]);
        for text in [
            "# Router\n\npassword: hunter2\n",
            "- **Password:** hunter2",
            "- **Senha**: abc",
            "api_key = sk-123",
            "API-KEY: xyz",
            "> token: abc",
            "Senha do roteador: abc12345",
            "- **API key on file:**\n  - `0123456789abcdef0123456789abcdef`",
            "- **Status:** registered. API key on file:\n  - `0123456789abcdef0123456789abcdef`",
            "Chave de API:\n\n`k3y-with-digits-42`",
            "export KEY=sk-abcdefghijklmnopqrstuvwxyz012345",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
        ] {
            assert!(e.content_excluded(text), "should exclude {text:?}");
        }
        for text in [
            "We discussed password managers today.",
            "Token budget: 5000 per call",
            "Max tokens: 4096",
            "## Passwords\n\nUse a manager.",
            "The secret of the method is repetition.",
            "password:\n",
            "Token type: bearer",
            "2. Token: GitHub → Settings → create a token",
            "Password: use the manager",
            "O modelo processa tokens: unidade real de processamento.",
            "Reset de senha: fluxo com e-mail e link temporário.",
        ] {
            assert!(!e.content_excluded(text), "should keep {text:?}");
        }
    }

    #[test]
    fn configured_regex_adds_to_the_detector() {
        let e = Exclusions::new(&[], r"^wifi:").unwrap();
        assert!(e.content_excluded("wifi: guest network"));
        assert!(e.content_excluded("password: hunter2"));
        assert!(!e.content_excluded("nothing here"));
    }
}
