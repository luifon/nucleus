//! Exclusion rules shared by vault search and vault check (ADR-035).
//!
//! Two layers:
//!
//! 1. **Path globs.** A fixed floor ([`FLOOR`]) that configuration cannot
//!    remove, plus `[vault_search] exclude` from nucleus.toml. The floor
//!    covers dot folders (`.obsidian/`, `.trash/`, `.git/`), file and
//!    folder names that look like credential stores, and the homelab area,
//!    where operators keep service logins.
//! 2. **Content.** `[vault_search] credential_content_regex`: a note whose
//!    text assigns a value to a credential-like key is excluded wherever it
//!    lives, so a password pasted into an ordinary note still stays out of
//!    search results.
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
        let fingerprint = format!("{}\n{}", patterns.join("\n"), content_regex);
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
        self.content.as_ref().is_some_and(|re| re.is_match(text))
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
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
    use crate::config::default_credential_content_regex;

    fn ex(extra: &[&str]) -> Exclusions {
        let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        Exclusions::new(&extra, &default_credential_content_regex()).unwrap()
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
    fn content_regex_catches_credential_lines() {
        let e = ex(&[]);
        for text in [
            "# Router\n\npassword: hunter2\n",
            "- **Password:** hunter2",
            "- **Senha**: abc",
            "api_key = sk-123",
            "API-KEY: x",
            "> token: abc",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
        ] {
            assert!(e.content_excluded(text), "should exclude {text:?}");
        }
        for text in [
            "We discussed password managers today.",
            "Token budget: 5000 per call",
            "## Passwords\n\nUse a manager.",
            "The secret of the method is repetition.",
            "password:\n",
        ] {
            assert!(!e.content_excluded(text), "should keep {text:?}");
        }
    }
}
