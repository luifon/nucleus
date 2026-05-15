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

    pub fn render(&self) -> String {
        let mut out = format!(
            "## {} — {}\n{}\n",
            self.at.format("%H:%M"),
            self.context,
            self.summary.trim_end()
        );
        for (tag, body) in &self.tagged {
            out.push_str(&format!("- {}: {}\n", tag.as_str(), body.trim()));
        }
        out
    }
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
