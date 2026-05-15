//! Tier 2 shared-fact read/write helpers. See ADR-002.
//!
//! The Tier 2 directory location is read from `NUCLEUS_TIER2_DIR` env var.
//! No hardcoded fallback — callers must have a `.env` set up.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    User,
    Feedback,
    Project,
    Reference,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub name: String,
    pub description: String,
    pub kind: Kind,
    pub body: String,
}

impl Memory {
    pub fn render(&self) -> String {
        format!(
            "---\nname: {}\ndescription: {}\nmetadata:\n  type: {}\n---\n\n{}\n",
            self.name,
            self.description,
            self.kind.as_str(),
            self.body.trim_end()
        )
    }
}

pub fn tier2_dir() -> Result<PathBuf> {
    let p = std::env::var("NUCLEUS_TIER2_DIR")
        .context("NUCLEUS_TIER2_DIR env var is not set (see .env.example)")?;
    Ok(PathBuf::from(p))
}

/// Write or overwrite a memory file. Caller is responsible for keeping
/// `MEMORY.md` (the index) in sync.
pub fn promote(mem: &Memory) -> Result<PathBuf> {
    let dir = tier2_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", mem.name.replace(' ', "_")));
    std::fs::write(&path, mem.render())?;
    Ok(path)
}

pub fn read(name: &str) -> Result<String> {
    let path = tier2_dir()?.join(format!("{}.md", name.replace(' ', "_")));
    Ok(std::fs::read_to_string(path)?)
}

pub fn list() -> Result<Vec<String>> {
    let dir = tier2_dir()?;
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut names = vec![];
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".md") && name != "MEMORY.md" {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}
