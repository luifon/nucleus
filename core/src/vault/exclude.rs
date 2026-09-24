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
use std::borrow::Cow;
use std::path::Path;
use unicode_normalization::UnicodeNormalization;

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

    /// The rules as `<workspace_root>/nucleus.toml` states them now. Every
    /// consumer that decides what may be shown or indexed calls this at the
    /// moment of the decision, so an exclusion the operator adds takes
    /// effect at the next request of a process that is already running.
    /// A missing file means the defaults; an unreadable or invalid file is
    /// an error (fail closed).
    pub fn load(workspace_root: &Path) -> Result<Self> {
        Self::from_config(&crate::config::load_vault_search(workspace_root)?)
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
    ///
    /// The path is compared in its [`fold`]ed form, so a name written with
    /// full-width letters or with an invisible character inside a floor
    /// word (`pass\u{200B}word.md`) is excluded like the plain spelling.
    pub fn path_excluded(&self, rel: &str) -> bool {
        let folded = fold(rel);
        let rel = folded.trim_start_matches('/');
        self.globs.iter().any(|g| g.matches(rel))
    }

    /// True when a note's text looks like it holds credentials. The
    /// configured regex is tried on the text as written and on its folded
    /// form.
    pub fn content_excluded(&self, text: &str) -> bool {
        let folded = fold(text);
        looks_like_credentials(&folded)
            || self
                .content
                .as_ref()
                .is_some_and(|re| re.is_match(text) || re.is_match(&folded))
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

/// Bumped whenever [`looks_like_credentials`] changes, so existing indexes
/// are rebuilt under the new rule.
const DETECTOR_VERSION: &str = "credential-detector-v3";

/// Unicode compatibility normalization (NFKC) with every
/// Default_Ignorable_Code_Point removed. NFKC turns full-width and other
/// compatibility letters into their plain forms (`ｐａｓｓｗｏｒｄ` →
/// `password`); removing the ignorable characters (zero-width space and
/// joiners, soft hyphen, bidi controls, variation selectors, tag
/// characters) joins a word that an invisible character split. Both
/// changes make text that looks the same to a reader compare the same
/// here. Returns the input unchanged when it is ASCII.
pub fn fold(s: &str) -> Cow<'_, str> {
    if s.is_ascii() {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.nfkc().filter(|c| !is_default_ignorable(*c)).collect())
}

/// Unicode `Default_Ignorable_Code_Point` (DerivedCoreProperties.txt).
fn is_default_ignorable(c: char) -> bool {
    matches!(
        c as u32,
        0x00AD
            | 0x034F
            | 0x061C
            | 0x115F..=0x1160
            | 0x17B4..=0x17B5
            | 0x180B..=0x180F
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x3164
            | 0xFE00..=0xFE0F
            | 0xFEFF
            | 0xFFA0
            | 0xFFF0..=0xFFF8
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0000..=0xE0FFF
    )
}

/// Words that name a secret when they label a value.
const SECRET_WORDS: &[&str] = &[
    "password", "passwords", "passwd", "passphrase", "passphrases", "passcode", "senha", "senhas",
    "secret", "token", "apikey", "pin", "credential", "credentials", "credencial", "credenciais",
];

/// Two-word labels that name a secret (`chave de API` is read as `chave
/// api`, see [`LabelWords`]).
const SECRET_PAIRS: &[(&str, &str)] = &[
    ("api", "key"),
    ("api", "keys"),
    ("chave", "api"),
    ("access", "key"),
    ("private", "key"),
    ("client", "secret"),
    ("access", "token"),
];

/// Words dropped from a label before it is compared with the secret words.
const FILLER_WORDS: &[&str] = &["de", "do", "da", "of", "the", "my", "meu", "minha"];

/// Prepositions that attach a secret word to the thing it belongs to:
/// `senha do roteador`, `password for the printer`.
const OWNER_LINKS: &[&str] = &["de", "do", "da", "dos", "das", "for", "of", "para"];

/// Prepositions that make the secret word a modifier of the word before it:
/// `reset de senha` is about passwords, it does not hold one.
const MODIFIER_LINKS: &[&str] = &["de", "do", "da", "dos", "das", "of", "about", "sobre"];

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
///    markdown removed, is a **secret label** and the value is anything
///    other than a placeholder ([`is_placeholder`]). The value may contain
///    spaces (`password: correct horse battery staple`). When the same line
///    has no value, the next non-empty line that is not a heading is the
///    value. A secret label is one of:
///    - exactly a secret word or pair (`password`, `- **Senha:**`,
///      `API_KEY`, `chave de API`);
///    - a secret word or pair followed by an owner preposition, in a label
///      of up to six words (`senha do roteador`, `password for the printer`);
///    - two to four words ending in a secret word or pair that is not
///      preceded by a modifier preposition (`wifi password`, `github
///      token`; not `reset de senha`); or
/// 3. the label, of any length, contains a secret word or pair (`API key on
///    file:`, `the shared password for the office printer:`), and the value
///    — on the same line, or on the next non-empty line when the same line
///    has none — is a single token of 6 or more characters with both letters
///    and digits.
///
/// A value loses a trailing comment before it is judged: a YAML comment
/// (` # prod`), an HTML comment (`<!-- -->`) or an Obsidian comment
/// (`%% %%`). Rule 3 keeps prose such as `Token budget: 5000 per call` or
/// `Password policy: 12 characters minimum` searchable.
///
/// Rule 2 has false positives by design (`Token: see Settings → create one`
/// excludes its note). A false positive costs one note missing from search;
/// a false negative puts a credential in a search result.
///
/// The caller folds the text first ([`fold`]); [`Exclusions::content_excluded`]
/// does.
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

/// How a label relates to secrets.
#[derive(Debug, PartialEq, Eq)]
enum LabelKind {
    /// The label names the secret that the value is (rule 2).
    Secret,
    /// The label mentions a secret word somewhere (rule 3).
    Mentions,
    None,
}

/// A label split into lowercase alphanumeric words. `all` keeps every word;
/// `kept` holds the indexes (into `all`) of the words that are not
/// [`FILLER_WORDS`].
struct LabelWords {
    all: Vec<String>,
    kept: Vec<usize>,
}

impl LabelWords {
    fn new(raw: &str) -> Self {
        let all: Vec<String> = raw
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect();
        let kept = (0..all.len()).filter(|&i| !FILLER_WORDS.contains(&all[i].as_str())).collect();
        Self { all, kept }
    }

    fn kept(&self, k: usize) -> &str {
        &self.all[self.kept[k]]
    }

    /// Length (1 or 2 kept words) of a secret word or pair starting at
    /// kept word `k`.
    fn secret_term_at(&self, k: usize) -> Option<usize> {
        let w = self.kept(k);
        if k + 1 < self.kept.len() && SECRET_PAIRS.contains(&(w, self.kept(k + 1))) {
            return Some(2);
        }
        SECRET_WORDS.contains(&w).then_some(1)
    }

    fn word_after(&self, kept_k: usize) -> Option<&str> {
        self.all.get(self.kept[kept_k] + 1).map(String::as_str)
    }

    fn word_before(&self, kept_k: usize) -> Option<&str> {
        self.kept[kept_k].checked_sub(1).map(|i| self.all[i].as_str())
    }

    fn classify(&self) -> LabelKind {
        let n = self.kept.len();
        if n == 0 {
            return LabelKind::None;
        }
        // Exactly a secret word or pair.
        if self.secret_term_at(0) == Some(n) {
            return LabelKind::Secret;
        }
        // Head first: `senha do roteador`, `password for the printer`.
        if let Some(len) = self.secret_term_at(0) {
            if n <= 6 && self.word_after(len - 1).is_some_and(|w| OWNER_LINKS.contains(&w)) {
                return LabelKind::Secret;
            }
        }
        // Head last: `wifi password`, `github token`, not `reset de senha`.
        if (2..=4).contains(&n) {
            for len in [2, 1] {
                if n > len && self.secret_term_at(n - len) == Some(len) {
                    let modifier = self.word_before(n - len).is_some_and(|w| MODIFIER_LINKS.contains(&w));
                    if !modifier {
                        return LabelKind::Secret;
                    }
                }
            }
        }
        if (0..n).any(|k| self.secret_term_at(k).is_some()) {
            LabelKind::Mentions
        } else {
            LabelKind::None
        }
    }
}

/// Rules 2 and 3 for one `label: value` candidate. `following` is the text
/// after the line, for a value written on the next non-empty line.
fn label_value_is_secret(raw_label: &str, raw_value: &str, following: &[&str]) -> bool {
    let kind = LabelWords::new(raw_label).classify();
    if kind == LabelKind::None {
        return false;
    }
    let mut value = clean_value(raw_value);
    if value.is_empty() {
        value = following
            .iter()
            .find(|l| !l.trim().is_empty())
            .filter(|l| !is_heading(l))
            .map(|l| clean_value(l))
            .unwrap_or_default();
    }
    match kind {
        LabelKind::Secret => !is_placeholder(&value),
        LabelKind::Mentions => is_secret_token(&value),
        LabelKind::None => false,
    }
}

fn is_heading(line: &str) -> bool {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|&c| c == '#').count();
    (1..=6).contains(&hashes) && t[hashes..].starts_with([' ', '\t'])
}

/// Values that stand in for a secret instead of being one. A secret label
/// followed by one of these does not exclude the note:
///
/// - an empty value;
/// - a value wrapped in `<…>`, `[…]` (including `[[note links]]`), `{…}` or
///   `(…)`: a template slot or a pointer to another note;
/// - a value made only of mask characters: `x`, `*`, `.`, `-`, `_`, `•`,
///   `…`, `?`, `#` (`xxx`, `***`, `...`);
/// - one of [`PLACEHOLDER_WORDS`] (`none`, `n/a`, `tbd`, `redacted`,
///   `true`, `false`, …);
/// - a short reference to where the secret is kept: at most six words,
///   starting with a [`REFERENCE_LEADS`] word and naming a
///   [`SECRET_STORES`] word (`see vault`, `in 1password`, `use the
///   manager`, `no cofre`).
///
/// A reference is the one case where a value an operator could have typed
/// as a passphrase is accepted (`see the vault now`); the lead-plus-store
/// requirement keeps that set small.
pub fn is_placeholder(v: &str) -> bool {
    let t = v.trim().to_lowercase();
    if t.is_empty() {
        return true;
    }
    for (open, close) in [('<', '>'), ('[', ']'), ('{', '}'), ('(', ')')] {
        if t.len() >= 2 && t.starts_with(open) && t.ends_with(close) {
            return true;
        }
    }
    if t.chars().all(|c| matches!(c, 'x' | '*' | '.' | '-' | '_' | '•' | '…' | '?' | '#' | ' ')) {
        return true;
    }
    if PLACEHOLDER_WORDS.contains(&t.as_str()) {
        return true;
    }
    let words: Vec<&str> = t
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')'))
        .filter(|w| !w.is_empty())
        .collect();
    words.len() <= 6
        && words.first().is_some_and(|w| REFERENCE_LEADS.contains(w))
        && words.iter().any(|w| SECRET_STORES.contains(&w.trim_end_matches(['.', '!'])))
}

const PLACEHOLDER_WORDS: &[&str] = &[
    "none", "n/a", "na", "nil", "null", "tbd", "todo", "redacted", "removed", "hidden",
    "omitted", "placeholder", "empty", "true", "false", "yes", "no", "sim", "não", "nao",
    "nenhum", "nenhuma", "vazio",
];

const REFERENCE_LEADS: &[&str] = &[
    "see", "in", "on", "stored", "saved", "kept", "use", "ask", "check", "from", "ver", "veja",
    "no", "na", "em", "está", "esta", "guardada", "guardado", "salva", "salvo", "usar",
];

const SECRET_STORES: &[&str] = &[
    "vault", "manager", "keychain", "keyring", "1password", "bitwarden", "keepass", "lastpass",
    "gerenciador", "cofre", ".env", "env",
];

/// The value with a trailing comment, list markers, emphasis, quotes and
/// backticks removed.
fn clean_value(raw: &str) -> String {
    strip_trailing_comment(raw)
        .trim()
        .trim_start_matches(['-', '*', '>', '+', ' ', '\t'])
        .trim_matches(['*', '`', '"', '\'', ' ', '\t'])
        .to_string()
}

/// Cut a value at the first comment that follows it: `<!--` (HTML), `%%`
/// (Obsidian), or `#` preceded by whitespace (YAML). A `#` inside a token
/// (`abc#1`) or at the start of the value (`#abc123`) is part of the value.
fn strip_trailing_comment(raw: &str) -> &str {
    let mut cut = raw.len();
    for marker in ["<!--", "%%"] {
        if let Some(i) = raw.find(marker) {
            cut = cut.min(i);
        }
    }
    let bytes = raw.as_bytes();
    for (i, b) in bytes.iter().enumerate().take(cut) {
        if *b == b'#'
            && i > 0
            && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t')
            && !raw[..i].trim().is_empty()
        {
            cut = i;
            break;
        }
    }
    &raw[..cut]
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
            "Password: use the manager",
            "O modelo processa tokens: unidade real de processamento.",
            "Reset de senha: fluxo com e-mail e link temporário.",
        ] {
            assert!(!e.content_excluded(text), "should keep {text:?}");
        }
    }

    /// Text that looks like a credential to a reader but is written to
    /// slip past a plain string comparison.
    #[test]
    fn detector_resists_evasion() {
        let e = ex(&[]);
        for text in [
            // Zero-width space, soft hyphen, word joiner inside the label.
            "pass\u{200B}word: hunter2",
            "pass\u{00AD}word: hunter2",
            "se\u{2060}nha: abc123",
            // Full-width letters (NFKC folds them).
            "\u{FF50}\u{FF41}\u{FF53}\u{FF53}\u{FF57}\u{FF4F}\u{FF52}\u{FF44}: hunter2",
            // Trailing YAML, HTML and Obsidian comments after the value.
            "password: hunter2 # prod",
            "password: hunter2   # rotated in May",
            "- **Senha:** abc123 <!-- office -->",
            "token: k3yv4lue %% old %%",
            // A long label naming a secret, followed by a strong token.
            "the shared password for the office printer on the second floor: Xk29abcQ",
            "we keep the api key for the staging billing service here: abcd1234efgh",
            // Zero-width characters inside a known key format.
            "ghp_abcdefghijklmnopqrstu\u{200B}vwxyz0123456789",
        ] {
            assert!(e.content_excluded(text), "should exclude {text:?}");
        }
        for text in [
            // Comments do not turn prose into a secret.
            "password: see the manager # prod",
            "Token budget: 5000 per call # rough",
            // A long label without a strong token stays searchable.
            "the discussion about password managers in the team meeting: useful",
        ] {
            assert!(!e.content_excluded(text), "should keep {text:?}");
        }
        // A `#` that starts the value, or sits inside it, is the value.
        assert!(e.content_excluded("password: #Xk29abc"));
        assert!(e.content_excluded("password: abc#123"));
    }

    /// Round-3 regression: a value with spaces under a secret label is a
    /// credential. Before detector v3 the one-token requirement let every
    /// multi-word passphrase through.
    #[test]
    fn multi_word_values_under_a_secret_label_are_credentials() {
        let e = ex(&[]);
        for text in [
            "password: correct horse battery staple",
            "Passphrase: correct horse battery staple",
            "passphrase = correct horse battery staple",
            "**Password:** correct horse battery staple",
            "- **Senha:** cavalo correto bateria grampo",
            "* senha: cavalo correto bateria grampo",
            "1. PIN: 12 34 56",
            "> secret: the blue door opens at dawn",
            "- **Senha do roteador:** cavalo correto bateria",
            "Senha do wifi: cavalo correto bateria",
            "password for the printer: correct horse battery",
            "Wifi password: correct horse battery staple",
            "GitHub token: abc def ghi",
            "Chave de API: abc def ghi",
            "credenciais: usuario admin senha forte",
            "password: `correct horse battery staple`",
            "password: correct horse battery staple # rotated",
            "- **Password:**\n  - correct horse battery staple",
            "Senha:\n\ncavalo correto bateria grampo",
            "2. Token: GitHub → Settings → create a token",
            "pass\u{200B}phrase: correct horse battery staple",
        ] {
            assert!(e.content_excluded(text), "should exclude {text:?}");
        }
        for text in [
            // Placeholders.
            "password:",
            "password: <your password>",
            "password: [[Router login]]",
            "password: {{password}}",
            "password: xxx",
            "password: ***",
            "password: ...",
            "Password: see vault",
            "senha: no cofre",
            "token: in 1password",
            "password: stored in the password manager",
            "pin: true",
            "password: n/a",
            "password: TBD # later",
            "Password:\n\n## Next section",
            // Labels that only mention a secret, with prose values.
            "Reset de senha: fluxo com e-mail e link temporário.",
            "Password policy: 12 characters minimum",
            "Senha forte: pelo menos 12 caracteres",
            "Access token lifetime: 3600",
            "Token type: bearer",
        ] {
            assert!(!e.content_excluded(text), "should keep {text:?}");
        }
    }

    /// Detector changes rebuild existing indexes: the version is part of
    /// the fingerprint.
    #[test]
    fn detector_version_is_in_the_fingerprint() {
        assert!(ex(&[]).fingerprint().starts_with("credential-detector-v3\n"));
    }

    #[test]
    fn path_floor_resists_evasion() {
        let e = ex(&[]);
        for rel in [
            "0-Inbox/pass\u{200B}word-list.md",
            "0-Inbox/pass\u{00AD}words.md",
            "0-Inbox/\u{FF53}\u{FF45}\u{FF4E}\u{FF48}\u{FF41}.md",
            "4-Areas/Home\u{200D}lab/router.md",
            "\u{200B}.obsidian/app.json",
        ] {
            assert!(e.path_excluded(rel), "should exclude {rel:?}");
        }
        assert!(!e.path_excluded("3-Projects/Caf\u{e9}/index.md"));
    }

    #[test]
    fn configured_regex_adds_to_the_detector() {
        let e = Exclusions::new(&[], r"^wifi:").unwrap();
        assert!(e.content_excluded("wifi: guest network"));
        assert!(e.content_excluded("password: hunter2"));
        assert!(!e.content_excluded("nothing here"));
    }
}
