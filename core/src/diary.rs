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
    /// Lifecycle bookkeeping — a boot, a reconnect, a delivery, a sweep that
    /// found nothing, a pass with no candidates. Kept for triage within the
    /// retention window, invisible to the distiller, and a day made only of
    /// these is deleted at the next prune: "nothing happened" days do not
    /// earn a diary file.
    Routine,
}

impl Tag {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "FACT",
            Self::Feedback => "FEEDBACK",
            Self::Observation => "OBSERVATION",
            Self::Notable => "NOTABLE",
            Self::Routine => "ROUTINE",
        }
    }
}

/// Shorthand for a [`Tag::Routine`] entry.
pub fn record_routine(workspace_root: &Path, agent: &str, context: &str, summary: &str) -> Result<()> {
    record_observation(workspace_root, agent, context, summary, Tag::Routine)
}

const ROUTINE_BULLET: &str = "- ROUTINE: ";

/// The diary text with every routine entry removed. An entry is the block
/// from one `## HH:MM — context` heading to the next; it is routine when
/// every tagged bullet in it is `ROUTINE`. The frontmatter is dropped too,
/// so the result is only what a reader should reason about; empty when the
/// day had nothing.
pub fn without_routine(text: &str) -> String {
    let mut out = String::new();
    for block in entry_blocks(text) {
        if !block_is_routine(&block) {
            out.push_str(block.trim_end());
            out.push_str("\n\n");
        }
    }
    out
}

/// True when the file holds at least one non-routine entry.
pub fn has_substance(text: &str) -> bool {
    entry_blocks(text).iter().any(|b| !block_is_routine(b))
}

fn block_is_routine(block: &str) -> bool {
    let bullets: Vec<&str> = block.lines().filter(|l| l.starts_with("- ") && l.contains(": ")).collect();
    !bullets.is_empty() && bullets.iter().all(|l| l.starts_with(ROUTINE_BULLET))
}

/// Split a diary file into its `## HH:MM — …` entry blocks, skipping the
/// frontmatter and anything before the first heading.
fn entry_blocks(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut cur: Option<String> = None;
    for line in text.lines() {
        if line.starts_with("## ") {
            if let Some(b) = cur.take() {
                blocks.push(b);
            }
            cur = Some(String::new());
        }
        if let Some(b) = cur.as_mut() {
            b.push_str(line);
            b.push('\n');
        }
    }
    if let Some(b) = cur {
        blocks.push(b);
    }
    blocks
}

/// Delete `agent_dir` day files older than today whose entries are all
/// routine. Returns the dates removed.
pub fn prune_routine_only_days(agent_dir: &Path) -> Result<Vec<chrono::NaiveDate>> {
    let today = Local::now().date_naive();
    let mut removed = Vec::new();
    let Ok(entries) = std::fs::read_dir(agent_dir) else { return Ok(removed) };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('_') || !name.ends_with(".md") {
            continue;
        }
        let Ok(date) = name.trim_end_matches(".md").parse::<chrono::NaiveDate>() else { continue };
        if date >= today {
            continue;
        }
        let text = std::fs::read_to_string(e.path()).unwrap_or_default();
        if !has_substance(&text) {
            std::fs::remove_file(e.path())?;
            removed.push(date);
        }
    }
    removed.sort();
    Ok(removed)
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
            // A trailing period counts as a delimiter only when it ends the
            // sentence (followed by whitespace or end of text), so a dotted
            // version or an IP keeps its digits.
            (regex::Regex::new(r"(^|[^\w.-])\+?\d{10,13}([^\w.-]|\.\s|\.$|$)").unwrap(), "${1}<phone>${2}"),
            (regex::Regex::new(r#"/(?:Users|home)/[^/\s'")\]]+"#).unwrap(), "~"),
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
    use super::{has_substance, redact, without_routine};

    const DAY: &str = "---\nagent: x\ndate: 2026-09-07\n---\n\n## 04:00 — boot\nConnected as <jid>\n- ROUTINE: Connected as <jid>\n\n## 12:55 — dm\nreplied to text in 3.1s\n- OBSERVATION: replied to text in 3.1s\n\n## 13:00 — reconnect\nConnected as <jid>\n- ROUTINE: Connected as <jid>\n";

    #[test]
    fn routine_entries_are_filtered_and_days_classified() {
        let kept = without_routine(DAY);
        assert!(kept.contains("## 12:55 — dm"), "{kept}");
        assert!(!kept.contains("boot") && !kept.contains("reconnect"), "{kept}");
        assert!(has_substance(DAY));
        let quiet = "---\nagent: x\ndate: 2026-09-07\n---\n\n## 04:00 — boot\nup\n- ROUTINE: up\n";
        assert!(!has_substance(quiet));
        assert_eq!(without_routine(quiet), "");
        // an untagged legacy entry counts as substance
        let legacy = "---\nagent: x\ndate: 2026-09-07\n---\n\n## 04:00 — note\nsomething happened\n";
        assert!(has_substance(legacy));
    }

    #[test]
    fn redacts_jids_emails_phones_and_home_dirs() {
        assert_eq!(redact("Connected as 5511999999999:2@s.whatsapp.net"), "Connected as <jid>");
        assert_eq!(redact("group 5511999999999-1234567890@g.us and 5511999999999@lid"), "group <jid> and <jid>");
        assert_eq!(redact("mail someone@example.com now"), "mail <email> now");
        assert_eq!(redact("call +5511999999999 or 5511999999999"), "call <phone> or <phone>");
        assert_eq!(redact("/Users/someone/path/to/x"), "~/path/to/x");
        assert_eq!(redact("Call +5511999999999."), "Call <phone>.");
        assert_eq!(redact("Home (/Users/someone)."), "Home (~).");
        assert_eq!(redact("v1.2.3456789012 stays"), "v1.2.3456789012 stays");
    }

    #[test]
    fn leaves_dates_ids_and_counts_alone() {
        assert_eq!(redact("2026-09-07 12:00 reminder #50, msg 1546638944463097886"), "2026-09-07 12:00 reminder #50, msg 1546638944463097886");
        assert_eq!(redact("queue#513 sent in 1.2s (937c)"), "queue#513 sent in 1.2s (937c)");
    }
}
