//! Markdown note parsing for the vault index and check (ADR-035):
//! frontmatter, title, headings, tags, and `[[wiki-links]]`.
//!
//! Code is removed before links, headings and inline tags are read: fenced
//! blocks (``` and ~~~, closed by a fence of the same character and at
//! least the same length) and inline code spans. A `[[link]]` inside code
//! is an example, not a link. Code is replaced with spaces, not deleted,
//! so line numbers stay correct.

use regex::Regex;
use serde_yaml::{Mapping, Value};
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone)]
pub enum Frontmatter {
    /// The file does not open with a `---` block.
    Missing,
    /// A `---` block exists but is not a YAML mapping.
    Invalid,
    Valid(Mapping),
}

impl Frontmatter {
    pub fn is_valid(&self) -> bool {
        matches!(self, Frontmatter::Valid(_))
    }

    /// A scalar (or list joined with `, `) value for `key`, if present and
    /// non-empty.
    pub fn get_str(&self, key: &str) -> Option<String> {
        let Frontmatter::Valid(map) = self else { return None };
        let v = map.get(Value::String(key.to_string()))?;
        let s = value_to_string(v);
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    pub fn has_key(&self, key: &str) -> bool {
        self.get_str(key).is_some()
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Sequence(seq) => seq.iter().map(value_to_string).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", "),
        Value::Mapping(m) => m
            .iter()
            .map(|(k, v)| format!("{}: {}", value_to_string(k), value_to_string(v)))
            .collect::<Vec<_>>()
            .join(", "),
        Value::Tagged(t) => value_to_string(&t.value),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The note part of the link: `[[folder/Note#Heading|alias]]` → `folder/Note`.
    pub target: String,
    /// `![[...]]` embed.
    pub embed: bool,
    /// 1-based line number in the file.
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct ParsedNote {
    pub frontmatter: Frontmatter,
    /// Frontmatter `title`, else the first `# ` heading, else the file stem
    /// (for generic names such as `index.md`, the parent folder name).
    pub title: String,
    pub headings: Vec<String>,
    /// Frontmatter `tags`/`tag` plus inline `#tags`, without `#`, deduplicated.
    pub tags: Vec<String>,
    /// Text after the frontmatter block.
    pub body: String,
    /// Flattened `key: value` lines of the frontmatter, for full-text search.
    pub meta_text: String,
    pub links: Vec<Link>,
    /// Relative `[text](note.md)` targets, resolved later against the note's
    /// folder. Counted as inbound links for the orphan check only.
    pub md_links: Vec<String>,
}

impl ParsedNote {
    pub fn created(&self) -> Option<String> {
        self.frontmatter.get_str("created")
    }
    pub fn source(&self) -> Option<String> {
        self.frontmatter.get_str("source")
    }
}

/// Split a `---` frontmatter block off the start of `text`. Returns the raw
/// YAML, the body, and the 0-based line index where the body starts.
pub fn split_frontmatter(text: &str) -> (Option<&str>, &str, usize) {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let offset = text.len() - t.len();
    let first_end = match t.find('\n') {
        Some(i) => i,
        None => return (None, text, 0),
    };
    if t[..first_end].trim_end() != "---" {
        return (None, text, 0);
    }
    let mut pos = first_end + 1;
    let mut line_no = 1usize;
    while pos <= t.len() {
        let end = t[pos..].find('\n').map(|i| pos + i).unwrap_or(t.len());
        let line = t[pos..end].trim_end();
        line_no += 1;
        if line == "---" || line == "..." {
            let yaml = &t[first_end + 1..pos];
            let body_start = (end + 1).min(t.len());
            return (Some(yaml), &text[offset + body_start..], line_no);
        }
        if end >= t.len() {
            break;
        }
        pos = end + 1;
    }
    (None, text, 0)
}

/// Replace fenced code blocks and inline code spans with spaces.
pub fn strip_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let newline = &line[content.len()..];
        let indent = content.len() - content.trim_start_matches(' ').len();
        let trimmed = content.trim_start_matches(' ');
        let run_char = trimmed.chars().next();
        let run_len = |c: char| trimmed.chars().take_while(|&x| x == c).count();
        match fence {
            Some((c, n)) => {
                if indent <= 3 && run_char == Some(c) && run_len(c) >= n && trimmed.trim_start_matches(c).trim().is_empty() {
                    fence = None;
                }
                out.push_str(&blank(content));
            }
            None => {
                if indent <= 3 && matches!(run_char, Some('`') | Some('~')) {
                    let c = run_char.unwrap();
                    let n = run_len(c);
                    // A backtick fence's info string may not contain backticks.
                    let info = &trimmed[n * c.len_utf8()..];
                    if n >= 3 && !(c == '`' && info.contains('`')) {
                        fence = Some((c, n));
                        out.push_str(&blank(content));
                        out.push_str(newline);
                        continue;
                    }
                }
                out.push_str(&strip_inline_code(content));
            }
        }
        out.push_str(newline);
    }
    out
}

fn blank(s: &str) -> String {
    " ".repeat(s.chars().count())
}

/// Blank out `code` spans: a run of N backticks up to the next run of
/// exactly N backticks on the same line. An unclosed run is literal text.
fn strip_inline_code(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '`' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i] == '`' {
            i += 1;
        }
        let n = i - start;
        // Find the closing run of exactly n backticks.
        let mut j = i;
        let mut close = None;
        while j < chars.len() {
            if chars[j] == '`' {
                let s = j;
                while j < chars.len() && chars[j] == '`' {
                    j += 1;
                }
                if j - s == n {
                    close = Some(j);
                    break;
                }
            } else {
                j += 1;
            }
        }
        match close {
            Some(end) => {
                out.push_str(&" ".repeat(end - start));
                i = end;
            }
            None => {
                for _ in 0..n {
                    out.push('`');
                }
            }
        }
    }
    out
}

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(!?)\[\[([^\[\]\n]+?)\]\]").unwrap())
}

fn md_link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\]\(([^)\s]+\.md)(?:#[^)]*)?\)").unwrap())
}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^ {0,3}(#{1,6})[ \t]+(.+?)[ \t#]*$").unwrap())
}

fn inline_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|[\s(,])#([\p{L}\p{N}_][\p{L}\p{N}_/-]*)").unwrap())
}

/// Parse the inner text of a `[[...]]` into its note part. Handles
/// `|alias`, the table-escaped `\|alias`, `#heading` and `#^block`.
/// Returns `None` for a link to a heading of the same note (`[[#x]]`).
pub fn link_target(inner: &str) -> Option<String> {
    // The alias starts at the first `|`; in a table the pipe is written `\|`.
    let note_part = match inner.find('|') {
        Some(i) => inner[..i].strip_suffix('\\').unwrap_or(&inner[..i]),
        None => inner,
    };
    let note_part = match note_part.find('#') {
        Some(i) => &note_part[..i],
        None => note_part,
    };
    let t = note_part.trim();
    (!t.is_empty()).then(|| t.nfc().collect())
}

/// Extract wiki-links from text that has already had its code stripped.
pub fn extract_links(visible: &str) -> Vec<Link> {
    let mut out = Vec::new();
    for (idx, line) in visible.lines().enumerate() {
        if !line.contains("[[") {
            continue;
        }
        for cap in link_re().captures_iter(line) {
            if let Some(target) = link_target(&cap[2]) {
                out.push(Link { target, embed: &cap[1] == "!", line: idx + 1 });
            }
        }
    }
    out
}

pub fn parse(rel: &str, text: &str) -> ParsedNote {
    let (yaml, body, body_line) = split_frontmatter(text);
    let frontmatter = match yaml {
        None => Frontmatter::Missing,
        Some(y) if y.trim().is_empty() => Frontmatter::Valid(Mapping::new()),
        Some(y) => match serde_yaml::from_str::<Value>(y) {
            Ok(Value::Mapping(m)) => Frontmatter::Valid(m),
            Ok(Value::Null) => Frontmatter::Valid(Mapping::new()),
            _ => Frontmatter::Invalid,
        },
    };

    let visible = strip_code(text);
    let links = extract_links(&visible);
    let md_links = md_link_re()
        .captures_iter(&visible)
        .map(|c| c[1].to_string())
        .filter(|t| !t.contains("://"))
        .collect();

    // Headings and inline tags come from the body only. `strip_code` keeps
    // line structure, so skipping the frontmatter's lines is exact.
    let mut headings = Vec::new();
    let mut first_h1 = None;
    let mut tags: Vec<String> = Vec::new();
    for line in visible.lines().skip(body_line) {
        if let Some(c) = heading_re().captures(line) {
            let h = c[2].trim().to_string();
            if c[1].len() == 1 && first_h1.is_none() {
                first_h1 = Some(h.clone());
            }
            headings.push(h);
            continue;
        }
        for c in inline_tag_re().captures_iter(line) {
            let t = c[1].to_string();
            if t.chars().any(|ch| !ch.is_ascii_digit()) {
                tags.push(t);
            }
        }
    }
    for key in ["tags", "tag"] {
        if let Frontmatter::Valid(m) = &frontmatter {
            if let Some(v) = m.get(Value::String(key.to_string())) {
                let raw = value_to_string(v);
                for t in raw.split([',', ' ']) {
                    let t = t.trim().trim_start_matches('#').trim_matches(['[', ']', '"', '\'']);
                    if !t.is_empty() {
                        tags.push(t.to_string());
                    }
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    tags.retain(|t| seen.insert(t.to_lowercase()));

    let meta_text = match &frontmatter {
        Frontmatter::Valid(m) => m
            .iter()
            .map(|(k, v)| format!("{}: {}", value_to_string(k), value_to_string(v)))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };

    let title = frontmatter
        .get_str("title")
        .or(first_h1)
        .unwrap_or_else(|| stem_title(rel));

    ParsedNote {
        frontmatter,
        title,
        headings,
        tags,
        body: body.to_string(),
        meta_text,
        links,
        md_links,
    }
}

/// Title from the path: the file stem, or the parent folder for generic
/// names (`Alpha/index.md` → `Alpha`).
pub fn stem_title(rel: &str) -> String {
    let file = rel.rsplit('/').next().unwrap_or(rel);
    let stem = file.strip_suffix(".md").or_else(|| file.strip_suffix(".MD")).unwrap_or(file);
    if super::is_generic_name(file) {
        let mut parts = rel.rsplit('/');
        parts.next();
        if let Some(parent) = parts.next() {
            return parent.to_string();
        }
    }
    stem.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_split_and_parse() {
        let text = "---\ncreated: 2026-05-14\nsource: manual\ntags: [a, b]\n---\n# Title\n\nBody #inline\n";
        let n = parse("3-Projects/Alpha/notes.md", text);
        assert!(n.frontmatter.is_valid());
        assert_eq!(n.created().as_deref(), Some("2026-05-14"));
        assert_eq!(n.source().as_deref(), Some("manual"));
        assert_eq!(n.title, "Title");
        assert_eq!(n.tags, vec!["inline", "a", "b"]);
        assert!(n.body.starts_with("# Title"));
    }

    #[test]
    fn missing_and_invalid_frontmatter() {
        assert!(matches!(parse("a.md", "# x\n").frontmatter, Frontmatter::Missing));
        assert!(matches!(parse("a.md", "---\n- a\n- b\n---\nx").frontmatter, Frontmatter::Invalid));
        assert!(matches!(parse("a.md", "---\nkey: [unclosed\n---\nx").frontmatter, Frontmatter::Invalid));
        // An unclosed block is not frontmatter.
        assert!(matches!(parse("a.md", "---\ncreated: x\nno close").frontmatter, Frontmatter::Missing));
    }

    #[test]
    fn title_falls_back_to_parent_for_index() {
        assert_eq!(parse("3-Projects/Alpha/index.md", "no heading").title, "Alpha");
        assert_eq!(parse("6-Slipbox/some-idea.md", "no heading").title, "some-idea");
        assert_eq!(parse("x.md", "---\ntitle: Named\n---\n# H1").title, "Named");
    }

    #[test]
    fn links_aliases_headings_and_table_escapes() {
        let text = "See [[Note A]] and [[folder/Note B|alias]] and [[Note C#Section]].\n\
                    | col | [[Note D\\|shown]] |\n\
                    ![[image.png]] [[#local heading]] [[Note E#^block|x]]\n";
        let links = extract_links(&strip_code(text));
        let targets: Vec<&str> = links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, vec!["Note A", "folder/Note B", "Note C", "Note D", "image.png", "Note E"]);
        assert!(links[4].embed);
        assert_eq!(links[3].line, 2);
    }

    #[test]
    fn links_inside_code_are_ignored() {
        let text = "real [[Kept]]\n```\n[[In Fence]]\n```\n~~~~md\n[[In Tilde]]\n~~~\nstill fenced [[Also In Tilde]]\n~~~~\n\
                    inline `[[In Code]]` and ``[[Double `x` code]]`` then [[After Code]]\n\
                    unclosed ` backtick [[Visible]]\n";
        let links = extract_links(&strip_code(text));
        let targets: Vec<&str> = links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, vec!["Kept", "After Code", "Visible"]);
        // Line numbers survive the stripping.
        assert_eq!(links[1].line, 10);
    }

    #[test]
    fn headings_and_tags_skip_code() {
        let text = "# Top\n```\n# not a heading #nottag\n```\n## Second ##\nText #real-tag and #123 and url#frag\n";
        let n = parse("a.md", text);
        assert_eq!(n.headings, vec!["Top", "Second"]);
        assert_eq!(n.tags, vec!["real-tag"]);
    }

    #[test]
    fn markdown_links_collected() {
        let n = parse("a/b.md", "[x](other.md) [y](https://e.com/z.md) [z](../up/note.md#h)");
        assert_eq!(n.md_links, vec!["other.md", "../up/note.md"]);
    }
}
