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
//! `queued` → `eval` → (`refinement` →) `implementation` → `pr` →
//! `review` → `closed`, or `failed` / `cancelled`. Every agent step is a
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
//! tables (`outbound_queue`, `intake_group_requests`) and reads
//! `intake_groups` and `intake_inbound`.

pub mod briefs;
pub mod event;
pub mod git;
pub mod github;
pub mod pipeline;
pub mod publish;
pub mod stage;
pub mod store;
pub mod tools;

pub use event::{Event, NewEvent};
pub use stage::Stage;
pub use store::{Item, ItemMessage, ItemTransition};

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
