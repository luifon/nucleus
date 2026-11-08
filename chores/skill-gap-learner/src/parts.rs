//! Turn the unreviewed part of a session into reviewable parts (ADR-038):
//! render each event as text a reviewer reads, measure its pre-filter
//! signals, and split at line (or turn) boundaries so no part's rendered
//! text exceeds the window.
//!
//! Rendering is lossy on purpose: operator and assistant text is cut per
//! turn, a tool call becomes one line naming the tool and its main
//! argument, a failed tool result keeps its first lines, and a successful
//! result is left out (the next call shows what happened).

use crate::prefilter::{Signals, is_correction};
use nucleus_core::claude_session::{TranscriptEvent, TranscriptEventKind, TurnRole};
use nucleus_core::session_index::IndexedTurn;
use std::collections::HashMap;

const USER_CLIP: usize = 2500;
const ASSISTANT_CLIP: usize = 1500;
const TOOL_CLIP: usize = 200;
const ERROR_CLIP: usize = 300;
/// Length of the session's opening request shown with a later part.
const OPENING_CLIP: usize = 800;
/// Most rendered text of one transcript record (one line can hold many
/// tool calls or results); the rest is left out with a note, its signals
/// still counted. Keeps every part within the window plus this much.
pub const GROUP_MAX: usize = 4 * 1024;

/// One reviewable part of a session.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    /// Position the part starts at (transcript line, or index turn `seq`).
    pub from: i64,
    /// Position after the part; the ledger records it once the part is done.
    pub to: i64,
    /// User and assistant text turns of the session before `to`: the same
    /// position in the session index (whose `seq` counts those turns).
    pub text_turns: i64,
    /// The rendered conversation.
    pub text: String,
    pub signals: Signals,
}

/// `s` trimmed and cut to `max` characters.
pub fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// How a user-role text reads.
#[derive(Debug, PartialEq)]
enum UserText {
    Operator,
    /// Claude Code's injection of a loaded skill's body.
    SkillBody(String),
    /// A compaction summary.
    Compaction,
    /// Some other text Claude Code wrote itself.
    Other,
}

const SKILL_BODY_PREFIX: &str = "Base directory for this skill:";
const COMPACTION_PREFIX: &str = "This session is being continued from a previous conversation";

fn classify_user_text(text: &str, meta: bool) -> UserText {
    if let Some(rest) = text.strip_prefix(SKILL_BODY_PREFIX) {
        let path = rest.lines().next().unwrap_or("").trim();
        let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
        return UserText::SkillBody(name);
    }
    if text.starts_with(COMPACTION_PREFIX) {
        return UserText::Compaction;
    }
    if meta { UserText::Other } else { UserText::Operator }
}

/// The main argument of a tool call, for its one-line rendering.
fn tool_summary(input: &serde_json::Value) -> String {
    for key in ["command", "file_path", "path", "pattern", "skill", "url", "query", "description", "prompt"] {
        if let Some(v) = input.get(key).and_then(|v| v.as_str()) {
            return clip(&v.replace('\n', " "), TOOL_CLIP);
        }
    }
    clip(&input.to_string(), TOOL_CLIP)
}

const REFUSED_MARKER: &str = "The user doesn't want to proceed";

/// What an event contributes: its rendered text and its signal.
struct Rendered {
    text: Option<String>,
    user_turn: bool,
    correction: bool,
    tool_call: bool,
    tool_error: Option<String>,
    tool_ok: Option<String>,
    skill_load: Option<String>,
}

impl Rendered {
    fn empty() -> Self {
        Rendered {
            text: None,
            user_turn: false,
            correction: false,
            tool_call: false,
            tool_error: None,
            tool_ok: None,
            skill_load: None,
        }
    }
}

fn render_event(e: &TranscriptEventKind, names: &HashMap<String, String>) -> Rendered {
    let mut r = Rendered::empty();
    match e {
        TranscriptEventKind::Text { role: TurnRole::User, text, meta } => match classify_user_text(text, *meta) {
            UserText::Operator => {
                r.user_turn = !text.starts_with("[Request interrupted by user");
                r.correction = is_correction(text);
                r.text = Some(format!("USER: {}", clip(text, USER_CLIP)));
            }
            UserText::SkillBody(name) => {
                r.text = Some(format!("[skill loaded: {name}]"));
                r.skill_load = Some(name);
            }
            UserText::Compaction => r.text = Some("[the session was compacted here; the summary is left out]".into()),
            UserText::Other => {}
        },
        TranscriptEventKind::Text { role: TurnRole::Assistant, text, .. } => {
            r.text = Some(format!("ASSISTANT: {}", clip(text, ASSISTANT_CLIP)));
        }
        TranscriptEventKind::ToolUse { name, input, .. } => {
            r.tool_call = true;
            if name == "Skill" {
                if let Some(s) = input.get("skill").and_then(|v| v.as_str()) {
                    r.skill_load = Some(s.to_string());
                }
            }
            r.text = Some(format!("TOOL {name}: {}", tool_summary(input)));
        }
        TranscriptEventKind::ToolResult { tool_use_id, is_error, text } => {
            let name = names.get(tool_use_id).cloned().unwrap_or_else(|| "?".into());
            if text.contains(REFUSED_MARKER) {
                r.correction = true;
                r.text = Some(format!("THE OPERATOR REFUSED THE {name} CALL"));
            } else if *is_error {
                r.text = Some(format!("TOOL ERROR ({name}): {}", clip(text, ERROR_CLIP)));
                r.tool_error = Some(name);
            } else {
                r.tool_ok = Some(name);
            }
        }
    }
    r
}

/// Accumulates one part.
#[derive(Default)]
struct Builder {
    text: String,
    signals: Signals,
    failed: HashMap<String, u32>,
}

impl Builder {
    fn add(&mut self, r: Rendered, skill_seen: &mut bool) {
        if let Some(name) = &r.skill_load {
            *skill_seen = true;
            if !self.signals.skills_loaded.contains(name) {
                self.signals.skills_loaded.push(name.clone());
            }
        }
        if r.user_turn {
            self.signals.user_turns += 1;
        }
        if r.correction {
            self.signals.corrections += 1;
            if *skill_seen {
                self.signals.skill_corrections += 1;
            }
        }
        if r.tool_call {
            self.signals.tool_calls += 1;
        }
        if let Some(name) = r.tool_error {
            self.signals.tool_errors += 1;
            *self.failed.entry(name).or_default() += 1;
        }
        if let Some(name) = r.tool_ok {
            if self.failed.remove(&name).is_some() {
                self.signals.error_recoveries += 1;
            }
        }
        if let Some(t) = r.text {
            self.text.push_str(&t);
            self.text.push_str("\n\n");
        }
    }

    fn finish(mut self, from: i64, to: i64) -> Part {
        self.signals.bytes = self.text.len();
        Part { from, to, text_turns: to, text: self.text, signals: self.signals }
    }
}

/// Split rendered groups (one per position) into parts of at most `window`
/// bytes each. A group larger than the window is a part of its own. A part
/// also ends at every position in `breaks` (sorted; the start of a new
/// local day), so no part holds two days.
fn split(
    groups: Vec<(i64, Vec<Rendered>)>,
    from: i64,
    end: i64,
    window: usize,
    breaks: &[i64],
    mut skill_seen: bool,
) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut cur = Builder::default();
    let mut cur_from = from;
    let mut breaks = breaks.iter().copied().filter(|b| *b > from && *b < end).peekable();
    for (pos, mut items) in groups {
        while let Some(b) = breaks.next_if(|b| *b <= pos) {
            if !cur.text.is_empty() {
                parts.push(std::mem::take(&mut cur).finish(cur_from, b));
                cur_from = b;
            }
        }
        let mut size = 0usize;
        let mut left_out = 0usize;
        for r in items.iter_mut() {
            let Some(t) = &r.text else { continue };
            if size + t.len() + 2 > GROUP_MAX {
                r.text = None;
                left_out += 1;
            } else {
                size += t.len() + 2;
            }
        }
        if left_out > 0 {
            let note = format!("[{left_out} more events of this record left out for length]");
            size += note.len() + 2;
            items.push(Rendered { text: Some(note), ..Rendered::empty() });
        }
        if size > 0 && !cur.text.is_empty() && cur.text.len() + size > window {
            parts.push(std::mem::take(&mut cur).finish(cur_from, pos));
            cur_from = pos;
        }
        for r in items {
            cur.add(r, &mut skill_seen);
        }
    }
    if !cur.text.is_empty() || parts.is_empty() {
        parts.push(cur.finish(cur_from, end));
    } else if let Some(last) = parts.last_mut() {
        last.to = end;
    }
    parts
}

/// The parts of a transcript in lines `[from, end)`. `events` are all of
/// the session's events from line 0 (tool names and skill loads before
/// `from` count); `end` is where reading stops: the line after the last
/// completed turn (see `sweep::reading_end`). Events at or after `end` wait
/// for a later run. `breaks` are lines where a part must end (the first
/// line of a new local day). Returns no part when nothing new was written.
pub fn transcript_parts(events: &[TranscriptEvent], from: i64, end: i64, window: usize, breaks: &[i64]) -> Vec<Part> {
    if end <= from {
        return Vec::new();
    }
    let names: HashMap<String, String> = events
        .iter()
        .filter_map(|e| match &e.kind {
            TranscriptEventKind::ToolUse { id, name, .. } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect();
    let mut skill_seen = false;
    let mut groups: Vec<(i64, Vec<Rendered>)> = Vec::new();
    for e in events {
        let r = render_event(&e.kind, &names);
        let pos = e.line as i64;
        if pos < from {
            skill_seen |= r.skill_load.is_some();
            continue;
        }
        if pos >= end {
            break;
        }
        match groups.last_mut() {
            Some((p, items)) if *p == pos => items.push(r),
            _ => groups.push((pos, vec![r])),
        }
    }
    let mut parts = split(groups, from, end, window, breaks, skill_seen);
    for p in &mut parts {
        p.text_turns = events
            .iter()
            .filter(|e| (e.line as i64) < p.to && matches!(e.kind, TranscriptEventKind::Text { .. }))
            .count() as i64;
    }
    parts
}

/// The parts of an index-only session from turn `from` on. `turns` are all
/// of the session's turns from `seq` 0; `end` is one past the last `seq`.
pub fn index_parts(turns: &[IndexedTurn], from: i64, end: i64, window: usize) -> Vec<Part> {
    if end <= from {
        return Vec::new();
    }
    let names = HashMap::new();
    let mut skill_seen = false;
    let mut groups = Vec::new();
    for t in turns {
        // The index stored Claude Code's own user-role texts (skill bodies,
        // compaction summaries) as user turns; classify them the same way.
        let kind = TranscriptEventKind::Text { role: t.role, text: t.text.clone(), meta: false };
        let r = render_event(&kind, &names);
        if t.seq < from {
            skill_seen |= r.skill_load.is_some();
            continue;
        }
        if t.seq >= end {
            continue;
        }
        groups.push((t.seq, vec![r]));
    }
    split(groups, from, end, window, &[], skill_seen)
}

/// The session's first operator request, shown with a part that does not
/// start at the beginning so the reviewer knows what the session was about.
pub fn opening_request<'a>(texts: impl Iterator<Item = (&'a str, bool)>) -> Option<String> {
    texts
        .filter(|(t, meta)| classify_user_text(t, *meta) == UserText::Operator)
        .map(|(t, _)| clip(t, OPENING_CLIP))
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(line: usize, kind: TranscriptEventKind) -> TranscriptEvent {
        TranscriptEvent { line, kind }
    }
    fn user(line: usize, t: &str) -> TranscriptEvent {
        ev(line, TranscriptEventKind::Text { role: TurnRole::User, text: t.into(), meta: false })
    }
    fn asst(line: usize, t: &str) -> TranscriptEvent {
        ev(line, TranscriptEventKind::Text { role: TurnRole::Assistant, text: t.into(), meta: false })
    }
    fn call(line: usize, id: &str, name: &str, input: serde_json::Value) -> TranscriptEvent {
        ev(line, TranscriptEventKind::ToolUse { id: id.into(), name: name.into(), input })
    }
    fn result(line: usize, id: &str, is_error: bool, text: &str) -> TranscriptEvent {
        ev(line, TranscriptEventKind::ToolResult { tool_use_id: id.into(), is_error, text: text.into() })
    }

    fn session() -> Vec<TranscriptEvent> {
        vec![
            user(0, "deploy the service"),
            call(1, "t1", "Skill", json!({"skill": "service-deploy"})),
            ev(2, TranscriptEventKind::Text { role: TurnRole::User, text: "Base directory for this skill: /ws/skills/service-deploy\n\nbody".into(), meta: true }),
            call(3, "t2", "Bash", json!({"command": "make push"})),
            result(4, "t2", true, "error: not built"),
            call(5, "t3", "Bash", json!({"command": "make build && make push"})),
            result(6, "t3", false, "ok"),
            asst(7, "Pushed."),
            user(8, "no, you forgot to run the tests first"),
            call(9, "t4", "Edit", json!({"file_path": "/ws/x.rs"})),
            result(10, "t4", false, "The user doesn't want to proceed with this tool use."),
        ]
    }

    #[test]
    fn signals_and_rendering_of_one_part() {
        let parts = transcript_parts(&session(), 0, 11, 100_000, &[]);
        assert_eq!(parts.len(), 1);
        let p = &parts[0];
        assert_eq!((p.from, p.to, p.text_turns), (0, 11, 4));
        let s = &p.signals;
        assert_eq!(s.user_turns, 2);
        assert_eq!(s.tool_calls, 4);
        assert_eq!(s.tool_errors, 1);
        assert_eq!(s.error_recoveries, 1);
        assert_eq!(s.corrections, 2, "the correcting turn and the refused call");
        assert_eq!(s.skill_corrections, 2);
        assert_eq!(s.skills_loaded, vec!["service-deploy".to_string()]);
        assert!(p.text.contains("TOOL Bash: make push"));
        assert!(p.text.contains("TOOL ERROR (Bash): error: not built"));
        assert!(p.text.contains("[skill loaded: service-deploy]"));
        assert!(p.text.contains("THE OPERATOR REFUSED THE Edit CALL"));
        assert!(!p.text.contains("\nok\n"), "successful results are left out");
        assert_eq!(s.bytes, p.text.len());
    }

    #[test]
    fn later_parts_start_at_the_ledger_position_and_split_at_lines() {
        let events = session();
        // Only the new part: a skill loaded before `from` still counts.
        let parts = transcript_parts(&events, 8, 11, 100_000, &[]);
        assert_eq!(parts.len(), 1);
        assert_eq!((parts[0].from, parts[0].to), (8, 11));
        assert_eq!(parts[0].signals.skill_corrections, 2);
        assert!(!parts[0].text.contains("deploy the service"));
        // Nothing new.
        assert!(transcript_parts(&events, 11, 11, 100_000, &[]).is_empty());
        // A new day starts at line 8: two parts, split there.
        let parts = transcript_parts(&events, 0, 11, 100_000, &[8]);
        assert_eq!(parts.iter().map(|p| (p.from, p.to)).collect::<Vec<_>>(), vec![(0, 8), (8, 11)]);
        assert!(parts[1].text.contains("forgot to run the tests") && !parts[1].text.contains("Pushed."));
        // Events at or after the end (a turn still running) wait.
        let parts = transcript_parts(&events, 0, 8, 100_000, &[]);
        assert_eq!((parts.len(), parts[0].to), (1, 8));
        assert!(parts[0].text.contains("Pushed.") && !parts[0].text.contains("forgot to run the tests"));
        assert_eq!(parts[0].signals.corrections, 0);
        // A small window splits at line boundaries; the parts are contiguous
        // and the last one ends at the end of the file.
        let parts = transcript_parts(&events, 0, 12, 60, &[]);
        assert!(parts.len() > 2, "{parts:?}");
        assert_eq!(parts[0].from, 0);
        assert_eq!(parts.last().unwrap().to, 12);
        for w in parts.windows(2) {
            assert_eq!(w[0].to, w[1].from);
        }
    }

    #[test]
    fn one_huge_record_is_cut_but_its_signals_count() {
        let mut events = vec![user(0, "run all the checks"), user(1, "no, run them again")];
        for i in 0..200 {
            events.push(call(2, &format!("c{i}"), "Bash", json!({"command": format!("check {i} {}", "x".repeat(150))})));
        }
        let parts = transcript_parts(&events, 0, 3, 100_000, &[]);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].text.len() < 2 * GROUP_MAX, "{}", parts[0].text.len());
        assert!(parts[0].text.contains("more events of this record left out"));
        assert_eq!(parts[0].signals.tool_calls, 200);
    }

    #[test]
    fn a_tail_without_rendered_text_extends_the_last_part() {
        let events = vec![user(0, "hi"), asst(1, "hello"), result(2, "x", false, "ok")];
        let parts = transcript_parts(&events, 0, 3, 100_000, &[]);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].to, 3);
        // Only noise after the position: one empty part that moves the ledger.
        let parts = transcript_parts(&events, 2, 3, 100_000, &[]);
        assert_eq!(parts.len(), 1);
        assert_eq!((parts[0].from, parts[0].to, parts[0].signals.user_turns), (2, 3, 0));
    }

    #[test]
    fn index_parts_use_text_signals_only() {
        let turns = vec![
            IndexedTurn { seq: 0, role: TurnRole::User, text: "set up the backup job".into() },
            IndexedTurn { seq: 1, role: TurnRole::User, text: "Base directory for this skill: /x/backup-ops".into() },
            IndexedTurn { seq: 2, role: TurnRole::Assistant, text: "done".into() },
            IndexedTurn { seq: 3, role: TurnRole::User, text: "não é isso, usa o outro disco".into() },
        ];
        let parts = index_parts(&turns, 0, 4, 100_000);
        assert_eq!(parts.len(), 1);
        let s = &parts[0].signals;
        assert_eq!((s.user_turns, s.corrections, s.skill_corrections, s.tool_calls), (2, 1, 1, 0));
        assert_eq!(s.skills_loaded, vec!["backup-ops".to_string()]);
        assert_eq!(opening_request(turns.iter().map(|t| (t.text.as_str(), false))).as_deref(), Some("set up the backup job"));
    }
}
