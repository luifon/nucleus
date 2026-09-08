//! Tier 1.5 — per-agent daily diaries. See ADR-004.
//!
//! Every spawned agent appends decisions/observations to today's file. The
//! distiller (`chores/distiller`) processes these on hourly + weekly cadences.
//!
//! Files live at `<workspace_root>/memory/diaries/<agent>/YYYY-MM-DD.md`.

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Tag {
    Fact,
    Feedback,
    Observation,
    Notable,
}

impl Tag {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "FACT",
            Self::Feedback => "FEEDBACK",
            Self::Observation => "OBSERVATION",
            Self::Notable => "NOTABLE",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub at: DateTime<Local>,
    pub context: String,
    pub summary: String,
    pub tagged: Vec<(Tag, String)>,
}

impl Entry {
    pub fn now(context: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            at: Local::now(),
            context: context.into(),
            summary: summary.into(),
            tagged: vec![],
        }
    }

    pub fn tag(mut self, tag: Tag, body: impl Into<String>) -> Self {
        self.tagged.push((tag, body.into()));
        self
    }

    /// The markdown for this entry, identifiers redacted (see [`redact`]).
    pub fn render(&self) -> String {
        let mut out = format!(
            "## {} — {}\n{}\n",
            self.at.format("%H:%M"),
            redact(&self.context),
            redact(self.summary.trim_end())
        );
        for (tag, body) in &self.tagged {
            out.push_str(&format!("- {}: {}\n", tag.as_str(), redact(body.trim())));
        }
        out
    }
}

/// Replace personal identifiers with placeholders before a line is written.
///
/// Diaries are the input of every autonomous writer (distiller, skill-gap
/// learner, curator), and those write to T2 memory, the vault and the skill
/// tree. A WhatsApp JID or a phone number in a diary line is one hop from a
/// file, and 2026-08-28 showed a reviewer receiving one. Redaction happens
/// here, at the source, so no downstream reader has to remember to.
///
/// Covered: WhatsApp JIDs (`<digits>[:n]@s.whatsapp.net`, `@lid`, group
/// `@g.us`), email addresses, bare 10–13 digit phone numbers, home
/// directories. Discord snowflakes (17–19 digits) are deliberately outside
/// the phone range.
pub fn redact(text: &str) -> String {
    use std::sync::OnceLock;
    static RULES: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        vec![
            (
                regex::Regex::new(r"\b\d{5,20}(?:-\d{5,20})?(?::\d{1,3})?@(?:s\.whatsapp\.net|lid|g\.us)\b").unwrap(),
                "<jid>",
            ),
            (regex::Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").unwrap(), "<email>"),
            // No look-around in the regex crate: capture the delimiters and
            // put them back. Applied until stable so two numbers separated by
            // a single delimiter both get caught.
            (regex::Regex::new(r"(^|[^\w.-])\+?\d{10,13}([^\w.-]|$)").unwrap(), "${1}<phone>${2}"),
            (regex::Regex::new(r#"/(?:Users|home)/[^/\s'"]+"#).unwrap(), "~"),
        ]
    });
    let mut out = text.to_string();
    for (re, rep) in rules {
        loop {
            let next = re.replace_all(&out, *rep).into_owned();
            if next == out {
                break;
            }
            out = next;
        }
    }
    out
}

pub fn diary_dir(workspace_root: &Path, agent: &str) -> PathBuf {
    workspace_root.join("memory/diaries").join(agent)
}

pub fn today_path(workspace_root: &Path, agent: &str) -> PathBuf {
    let date = Local::now().date_naive();
    diary_dir(workspace_root, agent).join(format!("{}.md", date))
}

/// Append an entry to today's diary. Creates the file (with frontmatter) on first write of the day.
pub fn append(workspace_root: &Path, agent: &str, entry: &Entry) -> Result<()> {
    let path = today_path(workspace_root, agent);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let needs_frontmatter = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if needs_frontmatter {
        writeln!(
            file,
            "---\nagent: {}\ndate: {}\n---",
            agent,
            entry.at.date_naive(),
        )?;
    }
    write!(file, "\n{}\n", entry.render())?;
    Ok(())
}

/// Convenience: record a single tagged observation in one call.
pub fn record_observation(
    workspace_root: &Path,
    agent: &str,
    context: &str,
    summary: &str,
    tag: Tag,
) -> Result<()> {
    let entry = Entry::now(context, summary).tag(tag, summary);
    append(workspace_root, agent, &entry)
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn redacts_jids_emails_phones_and_home_dirs() {
        assert_eq!(redact("Connected as 5511999999999:2@s.whatsapp.net"), "Connected as <jid>");
        assert_eq!(redact("group 5511999999999-1234567890@g.us and 5511999999999@lid"), "group <jid> and <jid>");
        assert_eq!(redact("mail someone@example.com now"), "mail <email> now");
        assert_eq!(redact("call +5511999999999 or 5511999999999"), "call <phone> or <phone>");
        assert_eq!(redact("/Users/someone/path/to/x"), "~/path/to/x");
    }

    #[test]
    fn leaves_dates_ids_and_counts_alone() {
        assert_eq!(redact("2026-09-07 12:00 reminder #50, msg 1546638944463097886"), "2026-09-07 12:00 reminder #50, msg 1546638944463097886");
        assert_eq!(redact("queue#513 sent in 1.2s (937c)"), "queue#513 sent in 1.2s (937c)");
    }
}
