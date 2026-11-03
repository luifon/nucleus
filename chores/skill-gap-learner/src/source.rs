//! Which sessions the sweep and the backfill read (ADR-038).
//!
//! Two sources. The transcript JSONL files in this workspace's Claude Code
//! project directory have text, tool calls and tool results. The session
//! index (ADR-023) keeps the text of sessions whose transcript Claude Code
//! has deleted; it is read only for those sessions, and only read.
//!
//! A transcript modified at or after the live start (an ADR-029 watermark,
//! set on the first run) belongs to the live sweep; older transcripts and
//! every index-only session belong to the backfill. The ledger decides
//! where reading starts, so the split never reviews a part twice.

use nucleus_core::config::SkillLearnerConfig;
use nucleus_core::session_index::{self, IndexedSession};
use nucleus_core::skill_learner::ledger::{LedgerRow, Source};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Watermark key: unix seconds; transcripts modified at or after it are the
/// live sweep's, older ones the backfill's.
pub const LIVE_START_KEY: &str = "skill-gap-learner.sweep.live-start";
/// On the first run, the live sweep covers sessions of the last day.
pub const FIRST_LIVE_WINDOW_SECS: i64 = 24 * 60 * 60;

/// A session with something left to read.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub session_id: String,
    pub source: Source,
    pub agent: Option<String>,
    /// The transcript path (for an index session, where it used to be).
    pub path: PathBuf,
    /// Unix seconds of the transcript's last change.
    pub mtime: i64,
    /// Transcript size in bytes (transcripts only).
    pub size: Option<i64>,
    /// Where reading starts: the ledger position, or 0.
    pub from: i64,
    /// Index sessions: the number of indexed turns.
    pub turn_count: Option<i64>,
    /// Transcripts: SHA-256 of the first line.
    pub head_hash: Option<String>,
}

/// SHA-256 of a transcript's first line. Claude Code only appends to a
/// transcript, so the first line changes only when the file is rewritten.
pub fn head_hash(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    let mut line = Vec::new();
    std::io::BufReader::new(f).take(1 << 20).read_until(b'\n', &mut line).ok()?;
    if line.is_empty() {
        return None;
    }
    Some(nucleus_core::skill_learner::content_hash(&line))
}

/// The sessions of one sweep.
#[derive(Debug, Default)]
pub struct Inventory {
    pub live: Vec<Candidate>,
    pub backfill: Vec<Candidate>,
    /// Transcripts modified within `quiet_minutes`: left for a later run.
    pub busy: Vec<String>,
    /// Sessions whose agent label is on the skip list and that the ledger
    /// does not record yet.
    pub skipped: Vec<Candidate>,
}

/// session id → agent label, from every attribution source Nucleus keeps,
/// all read-only: `usage.db` (sticky labels, ADR-034), then the venue DBs,
/// the session index, the run-logs and the reminder fires
/// (`usage::attribution::collect`, later sources win). Unknown sessions are
/// terminal sessions.
pub async fn labels(workspace_root: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let usage = workspace_root.join("memory/usage.db");
    if usage.exists() {
        if let Ok(pool) = nucleus_core::db::open_read_only(&usage).await {
            let rows: Vec<(String, Option<String>)> =
                sqlx_rows(&pool, "SELECT session_id, agent FROM sessions WHERE agent IS NOT NULL AND agent != ''").await;
            for (sid, agent) in rows {
                if let Some(a) = agent {
                    out.insert(sid, a);
                }
            }
            pool.close().await;
        }
    }
    for (sid, label) in nucleus_core::usage::attribution::collect(workspace_root).await {
        out.insert(sid, label.agent);
    }
    out
}

async fn sqlx_rows(pool: &sqlx::SqlitePool, sql: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as(sql).fetch_all(pool).await.unwrap_or_default()
}

/// True when sessions of `agent` are never reviewed: an ADR-023 synthetic
/// label, or a prefix in `skip_agents`.
pub fn is_skipped_agent(agent: Option<&str>, cfg: &SkillLearnerConfig) -> bool {
    agent.is_some_and(|a| {
        session_index::is_synthetic_agent(a)
            || cfg.skip_agents.iter().any(|p| !p.is_empty() && a.starts_with(p.as_str()))
    })
}

fn unix_mtime(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether the ledger says a transcript has nothing new: the part up to the
/// end of a file of exactly this size has been handled.
fn unchanged(row: &LedgerRow, size: i64) -> bool {
    row.file_size == Some(size) && matches!(row.outcome.as_str(), "reviewed" | "filtered" | "gave-up")
}

/// Sort the sessions of one sweep. `index` is the session index's eligible
/// sessions (empty when there is no index); `now` and `live_start` are unix
/// seconds.
pub fn inventory(
    transcripts_dir: &Path,
    cfg: &SkillLearnerConfig,
    ledger: &HashMap<String, LedgerRow>,
    labels: &HashMap<String, String>,
    index: &[IndexedSession],
    live_start: i64,
    now: i64,
) -> Inventory {
    let mut inv = Inventory::default();
    let quiet_since = now - i64::from(cfg.quiet_minutes) * 60;
    let mut on_disk: HashSet<String> = HashSet::new();

    if let Ok(entries) = std::fs::read_dir(transcripts_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(sid) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else { continue };
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            on_disk.insert(sid.clone());
            let mtime = unix_mtime(&meta);
            let size = meta.len() as i64;
            let agent = labels.get(&sid).cloned();
            let row = ledger.get(&sid);
            if row.is_some_and(|r| r.outcome == "own") {
                continue;
            }
            let skip = is_skipped_agent(agent.as_deref(), cfg);
            if skip && row.is_some_and(|r| r.outcome == "skipped-agent") {
                continue;
            }
            let head = if skip { None } else { head_hash(&path) };
            let from = match row {
                Some(r) if r.source == Source::Transcript && !skip => {
                    // A different first line, or a file smaller than when it
                    // was read: the file was rewritten; read it from the start.
                    let rewritten = (r.head_hash.is_some() && r.head_hash != head)
                        || r.file_size.is_some_and(|s| size < s);
                    if !rewritten && unchanged(r, size) {
                        continue;
                    }
                    if rewritten { 0 } else { r.position }
                }
                _ => 0,
            };
            let c = Candidate {
                session_id: sid.clone(),
                source: Source::Transcript,
                agent,
                path: path.clone(),
                mtime,
                size: Some(size),
                from,
                turn_count: None,
                head_hash: head,
            };
            if skip {
                inv.skipped.push(c);
            } else if mtime > quiet_since {
                inv.busy.push(sid);
            } else if mtime >= live_start {
                inv.live.push(c);
            } else {
                inv.backfill.push(c);
            }
        }
    }

    for s in index {
        if on_disk.contains(&s.session_id) || Path::new(&s.path).exists() {
            continue;
        }
        let row = ledger.get(&s.session_id);
        let from = match row {
            Some(r) if matches!(r.outcome.as_str(), "own" | "skipped-agent") => continue,
            // Read from its transcript before Claude Code deleted the file:
            // continue in the index after the text turns already read.
            Some(r) if r.source == Source::Transcript => match r.text_turns {
                Some(t) => t,
                None => continue,
            },
            Some(r) => r.position,
            None => 0,
        };
        if from >= s.turn_count {
            continue;
        }
        let agent = s.agent.clone().or_else(|| labels.get(&s.session_id).cloned());
        let c = Candidate {
            session_id: s.session_id.clone(),
            source: Source::Index,
            agent: agent.clone(),
            path: PathBuf::from(&s.path),
            mtime: s.mtime,
            size: None,
            from,
            turn_count: Some(s.turn_count),
            head_hash: None,
        };
        if is_skipped_agent(agent.as_deref(), cfg) {
            inv.skipped.push(c);
        } else {
            inv.backfill.push(c);
        }
    }

    let key = |c: &Candidate| (c.mtime, c.session_id.clone());
    inv.live.sort_by_key(key);
    inv.backfill.sort_by_key(key);
    inv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sid: &str, source: Source, position: i64, size: Option<i64>, outcome: &str) -> LedgerRow {
        LedgerRow {
            session_id: sid.into(),
            source,
            agent: None,
            position,
            file_size: size,
            outcome: outcome.into(),
            reviews: 0,
            failures: 0,
            head_hash: None,
            text_turns: None,
        }
    }

    fn touch(dir: &Path, name: &str, body: &str, secs_ago: i64, now: i64) {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs((now - secs_ago) as u64);
        std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn sessions_are_sorted_into_live_backfill_busy_and_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = 2_000_000_000;
        let live_start = now - 86_400;
        touch(dir, "old.jsonl", "{}\n", 10 * 86_400, now);
        touch(dir, "older.jsonl", "{}\n", 20 * 86_400, now);
        touch(dir, "recent.jsonl", "{}\n", 3600, now);
        touch(dir, "busy.jsonl", "{}\n", 60, now);
        touch(dir, "fire.jsonl", "{}\n", 7200, now);
        touch(dir, "done.jsonl", "{}\n", 7200, now);
        touch(dir, "grown.jsonl", "{}\n{}\n", 7200, now);
        touch(dir, "own.jsonl", "{}\n", 7200, now);
        touch(dir, "rewritten.jsonl", "{}\n", 7200, now);
        std::fs::create_dir_all(dir.join("subagents")).unwrap();

        let cfg = SkillLearnerConfig::default();
        let labels: HashMap<String, String> = [("fire".to_string(), "reminders-fire".to_string())].into();
        let ledger: HashMap<String, LedgerRow> = [
            ("done".to_string(), row("done", Source::Transcript, 1, Some(3), "reviewed")),
            ("grown".to_string(), row("grown", Source::Transcript, 1, Some(3), "filtered")),
            ("own".to_string(), row("own", Source::Transcript, 0, None, "own")),
            ("gone-read".to_string(), row("gone-read", Source::Transcript, 9, Some(99), "reviewed")),
            ("gone-half".to_string(), LedgerRow { text_turns: Some(4), ..row("gone-half", Source::Transcript, 9, None, "filtered") }),
            ("rewritten".to_string(), LedgerRow { head_hash: Some("other".into()), ..row("rewritten", Source::Transcript, 1, Some(3), "reviewed") }),
            ("gone-part".to_string(), row("gone-part", Source::Index, 2, None, "filtered")),
        ]
        .into();
        let idx = |sid: &str, turns: i64, mtime: i64| IndexedSession {
            session_id: sid.into(),
            path: dir.join(format!("{sid}.jsonl")).to_string_lossy().into_owned(),
            agent: None,
            mtime,
            turn_count: turns,
        };
        let index = vec![
            idx("gone", 6, 100),
            idx("gone-read", 6, 50),
            idx("gone-part", 6, 60),
            idx("gone-done", 0, 70),
            idx("gone-half", 6, 80),
            idx("old", 3, 0), // still on disk: read from the transcript
        ];
        let inv = inventory(dir, &cfg, &ledger, &labels, &index, live_start, now);

        let ids = |v: &[Candidate]| v.iter().map(|c| c.session_id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&inv.live), vec!["grown", "rewritten", "recent"]);
        assert_eq!(inv.live[0].from, 1, "a grown transcript continues at the ledger position");
        assert_eq!(inv.live[1].from, 0, "a rewritten transcript is read from the start");
        assert_eq!(ids(&inv.backfill), vec!["gone-part", "gone-half", "gone", "older", "old"]);
        assert_eq!(inv.backfill[0].from, 2);
        assert_eq!((inv.backfill[1].from, inv.backfill[1].source), (4, Source::Index), "continues after the text turns read");
        assert_eq!(inv.backfill[1].source, Source::Index);
        assert_eq!(inv.busy, vec!["busy".to_string()]);
        assert_eq!(ids(&inv.skipped), vec!["fire"]);
    }

    #[test]
    fn skip_list_matches_prefixes_and_synthetic_labels() {
        let cfg = SkillLearnerConfig::default();
        assert!(is_skipped_agent(Some("reminders-fire"), &cfg));
        assert!(is_skipped_agent(Some("skill-gap-learner"), &cfg));
        assert!(is_skipped_agent(Some("sstest-x"), &cfg));
        assert!(!is_skipped_agent(Some("whatsapp"), &cfg));
        assert!(!is_skipped_agent(Some("chat"), &cfg));
        assert!(!is_skipped_agent(None, &cfg));
    }
}
