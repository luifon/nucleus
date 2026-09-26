//! Event intake and the issue pipeline (ADR-036).
//!
//! **Intake.** Every source (GitHub issues today; email, WhatsApp, homelab
//! alerts later) is an adapter ([`event::SourceAdapter`]) that turns its
//! items into one common record, [`event::NewEvent`]. The core stores each
//! record once per `(source, external_id)` in `memory/intake.db` and updates
//! it when the source reports a change. Scripts add events with
//! `nucleus events emit`. An adapter also decides whether an event passes
//! its gate (GitHub: the issue carries the configured label).
//!
//! **Pipeline.** An accepted event on a configured repo becomes an *item*
//! (`#<n>`). The item moves through stages ([`stage::Stage`]):
//! `queued` → `eval` → (`refinement` →) `implementation` → `pr` → `closed`,
//! or `failed` / `cancelled` (and `stale`, `blocked`, `held`). The operator's
//! WhatsApp decisions are read in [`decide`]. Every agent step is a
//! task in the ADR-033 ledger (`origin = pipeline`, parent links between the
//! steps of one item): the eval agent and each refinement turn run
//! read-only in a checkout of the repo, the implementation agent runs in a
//! git worktree with the `code` profile. Nucleus code, not an agent, does
//! every network step: clone and fetch, push, the draft pull request and
//! the issue comment.
//!
//! **Driver.** [`pipeline::tick`] (`nucleus intake tick`, every minute from
//! launchd, and on demand after an operator reply) polls the sources when
//! their interval has passed, reads operator replies, and advances every
//! open item by at most one step per stage. Ticks serialize on an advisory
//! lock; every item change is one `BEGIN IMMEDIATE` transaction that
//! re-checks the stage it moves from.
//!
//! **Write ownership (ADR-020).** intake.db is written only through this
//! module, inside the `nucleus` binary (the CLI, the tick, the dashboard's
//! write routes). whatsapp.db is the bot's: Rust inserts into its queue
//! table `outbound_queue`, writes the one-row `intake_chat_block`, and reads
//! `intake_inbound`.

pub mod briefs;
pub mod decide;
pub mod event;
pub mod git;
pub mod github;
pub mod hidden;
pub mod pipeline;
pub mod publish;
pub mod snapshot;
pub mod stage;
pub mod store;
pub mod tools;

pub use event::{Event, NewEvent};
pub use stage::Stage;
pub use store::{Item, ItemMessage, ItemTransition, PlanVersion};

/// Relative to the workspace root.
pub const INTAKE_DB_PATH: &str = "memory/intake.db";

/// Fill `{key}` placeholders of an operator-facing text.
pub fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// `template` with each `{key}` of `vars` replaced in one pass: text a value
/// brings in is not filled again, so an issue title that contains `{link}`
/// stays as written. An unknown `{key}` is kept.
pub fn fill_once(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let known = after.find('}').and_then(|close| vars.iter().find(|(k, _)| *k == &after[..close]).map(|(_, v)| (close, *v)));
        match known {
            Some((close, v)) => {
                out.push_str(v);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `s` on one line: every run of whitespace (line breaks included) becomes
/// one space, and the ends are trimmed. A notice whose `{link}` is empty
/// keeps no trailing space.
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut `s` to at most `max` characters, marking the cut.
pub(crate) fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}
