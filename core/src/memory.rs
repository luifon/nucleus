//! Tier 2 shared-fact read/write helpers. See ADR-002.
//!
//! The Tier 2 directory location is read from `NUCLEUS_TIER2_DIR` env var.
//! No hardcoded fallback — callers must have a `.env` set up.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The human-readable index loaded into every session. `promote`/`forget`
/// keep it in sync so a written memory is actually discoverable and a removed
/// one leaves no dangling link.
pub const INDEX_FILE: &str = "MEMORY.md";

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

/// Write or overwrite a memory file AND keep `MEMORY.md` in sync — appends an
/// index line if the file isn't already linked (idempotent; never clobbers a
/// hand-edited hook). Without this, promoted memories were silently orphaned
/// from the index and invisible to recall.
pub fn promote(mem: &Memory) -> Result<PathBuf> {
    let dir = tier2_dir()?;
    std::fs::create_dir_all(&dir)?;
    let filename = format!("{}.md", mem.name.replace(' ', "_"));
    let path = dir.join(&filename);
    std::fs::write(&path, mem.render())?;
    ensure_indexed(&dir, &filename, &humanize(&mem.name), &mem.description)?;
    Ok(path)
}

/// Remove a memory file and its `MEMORY.md` index line. Returns false if the
/// file didn't exist. Symmetric with `promote` — `/forget` must not leave a
/// dangling index link.
pub fn forget(name: &str) -> Result<bool> {
    let dir = tier2_dir()?;
    let filename = format!("{}.md", name.replace(' ', "_"));
    let path = dir.join(&filename);
    let existed = path.exists();
    if existed {
        std::fs::remove_file(&path)?;
    }
    // Drop the index line regardless, so a previously-dangling entry is cleaned.
    remove_index_line(&dir, &filename)?;
    Ok(existed)
}

/// Append `- [label](filename) — description` to MEMORY.md unless the file is
/// already linked. Creates MEMORY.md if absent.
fn ensure_indexed(dir: &Path, filename: &str, label: &str, description: &str) -> Result<()> {
    let index = dir.join(INDEX_FILE);
    let existing = std::fs::read_to_string(&index).unwrap_or_default();
    let link = format!("({filename})");
    if existing.lines().any(|l| l.contains(&link)) {
        return Ok(()); // already indexed — leave any hand-edited hook intact
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    let desc = description.trim().replace('\n', " ");
    out.push_str(&format!("- [{label}]({filename}) — {desc}\n"));
    std::fs::write(&index, out).with_context(|| format!("writing {}", index.display()))
}

/// Remove any MEMORY.md line linking `filename`.
fn remove_index_line(dir: &Path, filename: &str) -> Result<()> {
    let index = dir.join(INDEX_FILE);
    let Ok(existing) = std::fs::read_to_string(&index) else {
        return Ok(());
    };
    let link = format!("({filename})");
    let kept: Vec<&str> = existing.lines().filter(|l| !l.contains(&link)).collect();
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    std::fs::write(&index, out).with_context(|| format!("writing {}", index.display()))
}

/// Turn a kebab/snake memory slug into a readable index label:
/// `reminders-calendar-fires-late` → `Reminders calendar fires late`.
fn humanize(name: &str) -> String {
    let spaced = name.replace(['-', '_'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "nucleus-mem-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn index(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(INDEX_FILE)).unwrap_or_default()
    }

    #[test]
    fn ensure_indexed_appends_once_and_is_idempotent() {
        let dir = tmpdir();
        ensure_indexed(&dir, "foo.md", "Foo", "does a foo").unwrap();
        assert_eq!(index(&dir), "- [Foo](foo.md) — does a foo\n");
        // second call with same file: no duplicate, hook preserved
        ensure_indexed(&dir, "foo.md", "Foo", "DIFFERENT desc").unwrap();
        assert_eq!(index(&dir).matches("(foo.md)").count(), 1);
        assert!(index(&dir).contains("does a foo"), "kept original hook");
        // a different file appends a second line
        ensure_indexed(&dir, "bar.md", "Bar", "does a bar").unwrap();
        assert_eq!(index(&dir).lines().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_indexed_preserves_a_hand_written_index() {
        let dir = tmpdir();
        std::fs::write(dir.join(INDEX_FILE), "- [Existing](existing.md) — hand-written\n").unwrap();
        ensure_indexed(&dir, "new.md", "New", "auto").unwrap();
        let idx = index(&dir);
        assert!(idx.contains("hand-written"));
        assert!(idx.contains("(new.md)"));
        assert_eq!(idx.lines().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_index_line_drops_only_the_match() {
        let dir = tmpdir();
        std::fs::write(
            dir.join(INDEX_FILE),
            "- [A](a.md) — one\n- [B](b.md) — two\n- [C](c.md) — three\n",
        )
        .unwrap();
        remove_index_line(&dir, "b.md").unwrap();
        let idx = index(&dir);
        assert!(!idx.contains("(b.md)"));
        assert!(idx.contains("(a.md)") && idx.contains("(c.md)"));
        assert_eq!(idx.lines().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn humanize_slug() {
        assert_eq!(humanize("reminders-calendar-fires-late"), "Reminders calendar fires late");
        assert_eq!(humanize("foo_bar"), "Foo bar");
    }
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
