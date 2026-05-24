//! Shared skill-library access (ADR-008 / ADR-017).
//!
//! Discovery, SKILL.md frontmatter parsing, and the format validator — used
//! by the dashboard `/skills` handler (read) and the skill-gap learner
//! (read + write + validate) so both judge skills identically. Mirrors the
//! lib+bin sharing the reminders crate does.
//!
//! Two storage trees per ADR-008:
//!   - `~/.claude/skills/<name>/SKILL.md`   — operator-personal (gitignored)
//!   - `<repo>/.claude/skills/<name>/SKILL.md` — committed
//!
//! The learner only ever *writes* to the operator-personal tree (Rule 1).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const SKILL_FILE: &str = "SKILL.md";

/// A parsed skill, ready for the dashboard API or the learner's library view.
#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    /// Frontmatter `name`, falling back to the directory name (CC convention).
    pub name: String,
    pub description: String,
    /// "personal" (`~/.claude/skills`) or "repo" (`.claude/skills`).
    pub tier: String,
    /// Absolute path to the SKILL.md file.
    pub path: String,
    pub flavor: Option<String>,
    /// ADR-017: "agent" when the learner authored it; None for hand-written.
    pub created_by: Option<String>,
    /// ADR-017: protected from the curator's auto-archive when true.
    pub pinned: bool,
    pub mcp_needed: Option<Vec<String>>,
    pub last_used: Option<String>,
    pub last_failure: Option<String>,
    pub failure_count_30d: Option<i64>,
    pub notify_on_failure: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub trigger: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub flavor: Option<String>,
    pub created_by: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, deserialize_with = "string_or_vec")]
    pub mcp_needed: Option<Vec<String>>,
    pub last_used: Option<String>,
    pub last_failure: Option<String>,
    pub failure_count_30d: Option<i64>,
    #[serde(default, deserialize_with = "string_or_vec")]
    pub notify_on_failure: Option<Vec<String>>,
    #[serde(default, deserialize_with = "string_or_vec")]
    pub tags: Option<Vec<String>>,
    pub trigger: Option<String>,
}

/// Frontmatter authors flip between `key: value` and `key: [a, b]` even within
/// one field — accept either. Empty / null reads as None.
fn string_or_vec<'de, D>(de: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = serde_yaml::Value::deserialize(de)?;
    match value {
        serde_yaml::Value::Null => Ok(None),
        serde_yaml::Value::String(s) if s.is_empty() => Ok(None),
        serde_yaml::Value::String(s) => Ok(Some(vec![s])),
        serde_yaml::Value::Sequence(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    serde_yaml::Value::String(s) => out.push(s),
                    other => out.push(
                        serde_yaml::to_string(&other).unwrap_or_default().trim().to_string(),
                    ),
                }
            }
            Ok(Some(out))
        }
        other => Err(D::Error::custom(format!(
            "expected string or sequence, got {other:?}"
        ))),
    }
}

/// Extract the leading `---`-delimited YAML block. None if absent.
pub fn split_frontmatter(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    Some(&rest[..end])
}

/// Parse SKILL.md frontmatter, strict YAML first then a lenient line parser.
/// Strict gets the full field set; lenient recovers `name`/`description` etc.
/// when a natural-language `: ` in a value trips strict YAML. None = no block.
pub fn parse_frontmatter(content: &str, path: &Path) -> Option<Frontmatter> {
    let yaml = split_frontmatter(content)?;
    match serde_yaml::from_str::<Frontmatter>(yaml) {
        Ok(fm) => Some(fm),
        Err(e) => {
            tracing::warn!(
                "skills: strict YAML failed for {} ({e}); lenient line parse",
                path.display()
            );
            Some(parse_frontmatter_lenient(yaml))
        }
    }
}

fn parse_frontmatter_lenient(yaml: &str) -> Frontmatter {
    let mut fm = Frontmatter::default();
    for line in yaml.lines() {
        if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-') {
            continue;
        }
        let Some(idx) = line.find(": ") else { continue };
        let key = line[..idx].trim();
        let value = strip_quotes(line[idx + 2..].trim()).to_string();
        match key {
            "name" => fm.name = Some(value),
            "description" => fm.description = Some(value),
            "flavor" => fm.flavor = Some(value),
            "created_by" => fm.created_by = Some(value),
            "pinned" => fm.pinned = value == "true",
            "trigger" => fm.trigger = Some(value),
            "last_used" => fm.last_used = Some(value),
            "last_failure" => fm.last_failure = Some(value),
            _ => {}
        }
    }
    fm
}

fn strip_quotes(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Synchronously read every `<dir>/SKILL.md` under `root`, tagged `tier`.
/// Missing root → empty (operator hasn't created the tree). Skips the
/// learner's `.archive/` and `.rejected/` housekeeping dirs.
pub fn read_skills(root: &Path, tier: &str) -> Vec<Skill> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for dirent in entries.flatten() {
        let path = dirent.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        // Housekeeping dirs aren't skills.
        if dir_name.starts_with('.') {
            continue;
        }
        let skill_md = path.join(SKILL_FILE);
        let content = match std::fs::read_to_string(&skill_md) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let fm = parse_frontmatter(&content, &skill_md).unwrap_or_default();
        out.push(Skill {
            name: fm.name.unwrap_or(dir_name),
            description: fm.description.unwrap_or_default(),
            tier: tier.to_string(),
            path: skill_md.to_string_lossy().into_owned(),
            flavor: fm.flavor,
            created_by: fm.created_by,
            pinned: fm.pinned,
            mcp_needed: fm.mcp_needed,
            last_used: fm.last_used,
            last_failure: fm.last_failure,
            failure_count_30d: fm.failure_count_30d,
            notify_on_failure: fm.notify_on_failure,
            tags: fm.tags,
            trigger: fm.trigger,
        });
    }
    out
}

/// The required headings every SKILL.md must carry (Rule 11 / ADR-008). The
/// `# Failure modes` one is the load-bearing check — an empty/absent one
/// signals the skill wasn't thought through.
const REQUIRED_SECTIONS: &[&str] = &["when to invoke", "steps", "failure modes"];

/// Validate a SKILL.md against the contract the learner's autonomous writes
/// must meet — the format gate (ADR-017). Returns the list of problems;
/// empty = valid. This is what makes "direct writes" as reliable as
/// skill-creator: a non-conforming write is caught mechanically.
pub fn validate(content: &str) -> Vec<String> {
    let mut issues = Vec::new();

    let Some(yaml) = split_frontmatter(content) else {
        issues.push("missing `---` YAML frontmatter block".into());
        return issues; // nothing else parseable without it
    };
    let fm = match serde_yaml::from_str::<Frontmatter>(yaml) {
        Ok(fm) => fm,
        Err(_) => parse_frontmatter_lenient(yaml),
    };
    if fm.description.as_deref().unwrap_or("").trim().is_empty() {
        issues.push("frontmatter `description` is required and must be non-empty".into());
    }
    if fm.flavor.as_deref().unwrap_or("").trim().is_empty() {
        issues.push("frontmatter `flavor` is required (recipe | learned)".into());
    }

    // Required body sections — match `#`/`##` headings case-insensitively.
    let body_lower = content.to_lowercase();
    for section in REQUIRED_SECTIONS {
        let h1 = format!("# {section}");
        let h2 = format!("## {section}");
        let present = body_lower
            .lines()
            .any(|l| l.trim_start().starts_with(&h1) || l.trim_start().starts_with(&h2));
        if !present {
            issues.push(format!("missing required section heading `# {section}`"));
        }
    }
    issues
}

/// Default skills roots: operator-personal first (where the learner writes),
/// then the repo-committed tree. `home` and `workspace_root` resolved by caller.
pub fn default_roots(home: &Path, workspace_root: &Path) -> Vec<(PathBuf, &'static str)> {
    vec![
        (home.join(".claude/skills"), "personal"),
        (workspace_root.join(".claude/skills"), "repo"),
    ]
}

/// Fire a detached on-the-fly skill review (ADR-017) for a conversation that
/// just crossed the nudge interval. Best-effort and fully decoupled: it shells
/// out to the built `skill-gap-learner` binary and returns immediately, so it
/// never blocks the caller's reply or fails it. A no-op if the binary isn't
/// built yet. The conversational agents call this when `AskResult.review_due`.
pub fn fire_skill_review(workspace_root: &Path, venue: &str, chat_key: &str, transcript_path: &str) {
    use std::process::{Command, Stdio};
    let release = workspace_root.join("target/release/skill-gap-learner");
    let bin = if release.exists() {
        release
    } else {
        workspace_root.join("target/debug/skill-gap-learner")
    };
    if !bin.exists() {
        return;
    }
    let _ = Command::new(bin)
        .current_dir(workspace_root)
        .args([
            "review",
            "--transcript",
            transcript_path,
            "--venue",
            venue,
            "--chat-key",
            chat_key,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "---\nname: x\ndescription: does a thing\nflavor: learned\ncreated_by: agent\n---\n\n# When to invoke\nwhen y\n\n# Steps\n1. a\n\n# Failure modes\n- boom\n";

    #[test]
    fn validate_accepts_a_well_formed_skill() {
        assert!(validate(GOOD).is_empty(), "{:?}", validate(GOOD));
    }

    #[test]
    fn validate_flags_missing_failure_modes() {
        let no_fail = "---\ndescription: d\nflavor: learned\n---\n\n# When to invoke\nx\n\n# Steps\n1\n";
        let issues = validate(no_fail);
        assert!(issues.iter().any(|i| i.contains("failure modes")), "{issues:?}");
    }

    #[test]
    fn validate_flags_missing_frontmatter_and_description() {
        assert!(validate("# Steps\nno frontmatter").iter().any(|i| i.contains("frontmatter")));
        let no_desc = "---\nflavor: learned\n---\n\n# When to invoke\nx\n# Steps\n1\n# Failure modes\n-z\n";
        assert!(validate(no_desc).iter().any(|i| i.contains("description")));
    }

    #[test]
    fn parses_created_by_and_pinned() {
        let fm = parse_frontmatter(
            "---\ndescription: d\nflavor: learned\ncreated_by: agent\npinned: true\n---\nbody\n",
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(fm.created_by.as_deref(), Some("agent"));
        assert!(fm.pinned);
    }

    #[test]
    fn lenient_parse_recovers_description_with_colon() {
        // strict YAML trips on the bare `: ` in the value; lenient recovers it.
        let fm = parse_frontmatter(
            "---\nname: x\ndescription: Workspace arg: A or B\nflavor: learned\n---\nbody\n",
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(fm.flavor.as_deref(), Some("learned"));
    }
}
