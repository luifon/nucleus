//! Run-log index for tmux+claude agents (ADR-016 §unified log capture).
//!
//! Each Claude session a tmux-driving agent spawns leaves a structured
//! transcript at `~/.claude/projects/<cwd>/<session-id>.jsonl` that survives
//! the tmux window being killed. The transcript is the raw record — we do NOT
//! copy it (OpenClaw lesson, ADR-004: full-transcript copies blow up disk and
//! are rarely re-read). Instead each run appends a small pointer row to
//! `memory/logs/<agent>/runs.jsonl` so a surface like `/agents` can list runs
//! and open the right transcript in place.
//!
//! A row is written on spawn (`record_start`) with `ended_at`/`ok` null, then
//! finalized on close (`record_end`). `record_end` also caps the file to the
//! most recent [`MAX_ROWS_PER_AGENT`] runs and drops rows whose transcript no
//! longer exists — the gc is cheap and amortized across closes, so there's no
//! separate cleanup chore.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Most recent runs kept per agent. Older rows are dropped on the next
/// `record_end`. 50 covers "what ran today/this week" at single-operator
/// scale; transcripts themselves live under `~/.claude/projects`.
pub const MAX_ROWS_PER_AGENT: usize = 50;

/// One agent execution. Serialized one-per-line in `runs.jsonl`.
#[derive(Debug, Clone, Deserialize, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct RunRow {
    /// Unique per spawn (a fresh UUID, distinct from `session_id` so a
    /// resumed/rotated session that reuses an id still gets its own row).
    pub run_id: String,
    /// Registry agent name (`agent_label`).
    pub agent: String,
    /// Claude session id — names the transcript file.
    pub session_id: String,
    /// Absolute path to the transcript JSONL (read in place; never copied).
    pub transcript_path: String,
    /// tmux `session:window` target while the window was alive.
    pub tmux_target: String,
    pub started_at: String,
    /// RFC3339 when the session closed; null while in-flight.
    #[serde(default)]
    pub ended_at: Option<String>,
    /// Outcome at close; null while in-flight. (Best-effort — a crashed
    /// process leaves this null, which reads as "ran, outcome unknown".)
    #[serde(default)]
    pub ok: Option<bool>,
    /// `claude --version` output captured at spawn (ADR-020: forensics,
    /// NOT pinning — the latest binary always runs; this records which one
    /// did). None on legacy rows and when capture failed. Cached once per
    /// process, so a `claude update` mid-process shows the pre-update
    /// version until the next restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_version: Option<String>,
}

/// `memory/logs/<agent>/runs.jsonl` under the workspace root.
pub fn index_path(workspace_root: &Path, agent: &str) -> PathBuf {
    workspace_root
        .join("memory")
        .join("logs")
        .join(agent)
        .join("runs.jsonl")
}

/// Append an in-flight row at spawn time. Best-effort: a failure here must
/// never block spawning a session, so callers ignore the error.
pub fn record_start(workspace_root: &Path, row: &RunRow) -> Result<()> {
    let path = index_path(workspace_root, &row.agent);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating run-log dir {}", parent.display()))?;
    }
    let line = serde_json::to_string(row)?;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening run-log {}", path.display()))?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Finalize the row for `run_id`: set `ended_at`/`ok`, then cap to the most
/// recent [`MAX_ROWS_PER_AGENT`] and drop rows whose transcript is gone.
/// Best-effort; callers ignore the error.
pub fn record_end(workspace_root: &Path, agent: &str, run_id: &str, ok: bool) -> Result<()> {
    let path = index_path(workspace_root, agent);
    let mut rows = read(workspace_root, agent);
    if let Some(r) = rows.iter_mut().find(|r| r.run_id == run_id) {
        r.ended_at = Some(now_rfc3339());
        r.ok = Some(ok);
    }
    gc(&mut rows);
    write_all(&path, &rows)
}

/// Read all rows for an agent, oldest first. Missing/unreadable file = empty.
pub fn read(workspace_root: &Path, agent: &str) -> Vec<RunRow> {
    let path = index_path(workspace_root, agent);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<RunRow>(l).ok())
        .collect()
}

/// Drop rows whose transcript no longer exists, then keep only the most
/// recent [`MAX_ROWS_PER_AGENT`]. In-flight rows (ended_at == None) are kept
/// regardless of transcript presence — the file appears slightly after spawn.
fn gc(rows: &mut Vec<RunRow>) {
    rows.retain(|r| r.ended_at.is_none() || Path::new(&r.transcript_path).exists());
    if rows.len() > MAX_ROWS_PER_AGENT {
        let drop = rows.len() - MAX_ROWS_PER_AGENT;
        rows.drain(..drop);
    }
}

fn write_all(path: &Path, rows: &[RunRow]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut buf = String::new();
    for r in rows {
        buf.push_str(&serde_json::to_string(r)?);
        buf.push('\n');
    }
    // Write via temp + rename so a mid-write crash can't truncate the index.
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, buf.as_bytes())
        .with_context(|| format!("writing run-log {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming run-log into place {}", path.display()))?;
    Ok(())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_workspace() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "nucleus-runlog-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn row(run_id: &str, transcript: &str) -> RunRow {
        RunRow {
            run_id: run_id.into(),
            agent: "test-agent".into(),
            session_id: run_id.into(),
            transcript_path: transcript.into(),
            tmux_target: "nucleus-test:w".into(),
            started_at: now_rfc3339(),
            ended_at: None,
            ok: None,
            claude_version: None,
        }
    }

    #[test]
    fn legacy_rows_without_claude_version_deserialize() {
        let legacy = r#"{"run_id":"r","agent":"a","session_id":"s","transcript_path":"/t","tmux_target":"@1","started_at":"2026-01-01T00:00:00Z"}"#;
        let row: RunRow = serde_json::from_str(legacy).unwrap();
        assert!(row.claude_version.is_none());
        // And the field is omitted on write when None, so new code writing
        // next to legacy rows keeps the same shape.
        assert!(!serde_json::to_string(&row).unwrap().contains("claude_version"));
    }

    #[test]
    fn start_then_end_roundtrips() {
        let ws = tmp_workspace();
        // a real transcript file so gc keeps the finalized row
        let transcript = ws.join("t.jsonl");
        std::fs::write(&transcript, "{}").unwrap();

        record_start(&ws, &row("r1", transcript.to_str().unwrap())).unwrap();
        let rows = read(&ws, "test-agent");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ended_at.is_none());

        record_end(&ws, "test-agent", "r1", true).unwrap();
        let rows = read(&ws, "test-agent");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ok, Some(true));
        assert!(rows[0].ended_at.is_some());

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn gc_drops_finalized_rows_with_missing_transcripts_and_caps() {
        let ws = tmp_workspace();
        // 60 finalized rows, all pointing at a non-existent transcript →
        // gc drops them; then an in-flight row survives regardless.
        for i in 0..60 {
            let id = format!("r{i}");
            record_start(&ws, &row(&id, "/nonexistent/x.jsonl")).unwrap();
        }
        record_start(&ws, &row("live", "/nonexistent/y.jsonl")).unwrap();
        // finalize one of the missing-transcript rows → triggers gc
        record_end(&ws, "test-agent", "r0", true).unwrap();

        let rows = read(&ws, "test-agent");
        // r0 was finalized with a missing transcript → dropped; the rest are
        // still in-flight (ended_at None) so kept, but capped to MAX.
        assert!(rows.len() <= MAX_ROWS_PER_AGENT);
        assert!(rows.iter().any(|r| r.run_id == "live"));
        assert!(!rows.iter().any(|r| r.run_id == "r0"));

        let _ = std::fs::remove_dir_all(&ws);
    }
}
