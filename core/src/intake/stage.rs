//! Pipeline stages, their transitions, and the pure decisions the pipeline
//! makes from agent output and operator messages (ADR-036). Everything here
//! is pure and unit-tested; `pipeline.rs` applies it.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Where an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Accepted; the eval agent has not started.
    Queued,
    /// The eval agent runs.
    Eval,
    /// Discussion with the operator until a plan is approved.
    Refinement,
    /// The implementation agent runs, then Nucleus runs the tests.
    Implementation,
    /// Nucleus pushes the branch and opens the draft pull request.
    Pr,
    /// The draft PR is open; the issue comment waits for the operator.
    Review,
    Closed,
    /// A step failed; `nucleus intake retry` or the dashboard resumes it.
    Failed,
    Cancelled,
    /// The source changed after the gate was satisfied (the issue text, a
    /// comment the item used, the label event): the item stops for good.
    /// Re-adding the label starts a new item from the current text.
    Stale,
    /// A check before a privileged step stopped it: the secret guard found
    /// something in what was about to be published, or a pinned executable
    /// changed. Nothing was published; `retry` checks again, `cancel` stops
    /// the item.
    Blocked,
    /// The issue text or a comment the item uses has content that GitHub's
    /// page does not show (an HTML comment, invisible characters, …). No
    /// agent runs until the operator releases the item or cancels it.
    Held,
}

impl Stage {
    pub const ALL: [Stage; 12] = [
        Stage::Queued,
        Stage::Eval,
        Stage::Refinement,
        Stage::Implementation,
        Stage::Pr,
        Stage::Review,
        Stage::Closed,
        Stage::Failed,
        Stage::Cancelled,
        Stage::Stale,
        Stage::Blocked,
        Stage::Held,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Queued => "queued",
            Stage::Eval => "eval",
            Stage::Refinement => "refinement",
            Stage::Implementation => "implementation",
            Stage::Pr => "pr",
            Stage::Review => "review",
            Stage::Closed => "closed",
            Stage::Failed => "failed",
            Stage::Cancelled => "cancelled",
            Stage::Stale => "stale",
            Stage::Blocked => "blocked",
            Stage::Held => "held",
        }
    }

    pub fn parse(s: &str) -> Option<Stage> {
        Stage::ALL.into_iter().find(|st| st.as_str() == s)
    }

    /// No further work and no retry.
    pub fn is_terminal(self) -> bool {
        matches!(self, Stage::Closed | Stage::Cancelled | Stage::Stale)
    }
}

/// What happened to an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageEvent {
    EvalStarted,
    /// The eval decided the item goes straight to implementation.
    EvalSimple,
    /// The eval decided the item needs a plan first.
    EvalNeedsPlan,
    PlanApproved,
    ImplementationDone,
    PrOpened,
    /// The review stage ended: the comment was posted or skipped.
    Finished,
    Failed,
    Cancel,
    /// The event was closed at its source (the issue was closed).
    SourceClosed,
    /// The source no longer matches what the item is bound to.
    Stale,
    /// The secret guard stopped a publishing step.
    Blocked,
    /// Resume a failed item at the stage it failed in.
    Retry { failed_in: Stage },
    /// Hidden content was found before an agent step: the item waits for
    /// the operator.
    Hold,
    /// The operator released a held item; it continues at the stage it was
    /// held in.
    Release { held_in: Stage },
}

/// The stage `from` moves to on `ev`, or an error when the event does not
/// apply to that stage. The only place stage changes are decided.
pub fn transition(from: Stage, ev: &StageEvent) -> Result<Stage> {
    use Stage::*;
    use StageEvent as E;
    let to = match (from, ev) {
        (s, _) if s.is_terminal() => bail!("item is {}; it does not change any more", s.as_str()),
        (Queued, E::EvalStarted) => Eval,
        (Eval, E::EvalSimple) => Implementation,
        (Eval, E::EvalNeedsPlan) => Refinement,
        (Refinement, E::PlanApproved) => Implementation,
        (Implementation, E::ImplementationDone) => Pr,
        (Pr, E::PrOpened) => Review,
        (Review, E::Finished) => Closed,
        (Failed, E::Failed) => bail!("item already failed"),
        (Queued | Eval | Refinement | Implementation | Pr | Review, E::Blocked) => Blocked,
        (Blocked, E::Retry { failed_in: Queued | Eval }) => Queued,
        (Blocked, E::Retry { failed_in: failed_in @ (Refinement | Implementation | Pr | Review) }) => *failed_in,
        (Queued | Eval | Refinement | Implementation, E::Hold) => Held,
        (Held, E::Release { held_in: held_in @ (Queued | Eval | Refinement | Implementation) }) => *held_in,
        (_, E::Failed) => Failed,
        (_, E::Cancel) => Cancelled,
        (_, E::SourceClosed) => Closed,
        (_, E::Stale) => Stale,
        (Failed, E::Retry { failed_in }) => match failed_in {
            // An eval is run again from the start.
            Queued | Eval => Queued,
            Refinement | Implementation | Pr | Review | Held => *failed_in,
            Closed | Failed | Cancelled | Stale | Blocked => bail!("nothing to retry in stage {}", failed_in.as_str()),
        },
        (s, e) => bail!("{e:?} does not apply to an item in the {} stage", s.as_str()),
    };
    Ok(to)
}

// ── eval ─────────────────────────────────────────────────────────────────

/// The eval agent's classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Classification {
    Simple,
    Complex,
    Feature,
}

impl Classification {
    pub fn as_str(self) -> &'static str {
        match self {
            Classification::Simple => "simple",
            Classification::Complex => "complex",
            Classification::Feature => "feature",
        }
    }
}

/// The criteria the eval agent reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export, rename = "IntakeEvalCriteria")]
pub struct Criteria {
    /// `small` | `medium` | `large`.
    pub change_size: String,
    pub schema_impact: bool,
    pub security_impact: bool,
    pub public_api_impact: bool,
    /// 0.0 – 1.0.
    pub confidence: f64,
}

/// The eval result as stored (`items.eval_json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export, rename = "IntakeEval")]
pub struct EvalResult {
    /// What the agent said: `simple` | `complex` | `feature`.
    pub classification: String,
    /// What the pipeline does: `simple` (straight to implementation) or the
    /// agent's class, raised to `complex` by [`decide`].
    pub effective: String,
    pub summary: String,
    pub reasons: Vec<String>,
    pub criteria: Criteria,
    /// Why the pipeline raised a `simple` to `complex`; empty otherwise.
    pub escalations: Vec<String>,
}

#[derive(Deserialize)]
struct RawEval {
    classification: Classification,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    reasons: Vec<String>,
    criteria: Criteria,
}

pub const EVAL_OPEN: &str = "===EVAL===";
pub const EVAL_CLOSE: &str = "===END EVAL===";

/// The text between the last `open` line and the following `close` line.
fn last_block<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.rfind(open)? + open.len();
    let rest = &text[start..];
    let end = rest.find(close)?;
    Some(rest[..end].trim())
}

/// Parse the eval agent's final message and apply the escalation policy.
pub fn parse_eval(text: &str, min_confidence: f64) -> Result<EvalResult> {
    let block = last_block(text, EVAL_OPEN, EVAL_CLOSE)
        .with_context(|| format!("the eval output has no {EVAL_OPEN} … {EVAL_CLOSE} block"))?;
    let json = block.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    let raw: RawEval = serde_json::from_str(json).context("the eval block is not the expected JSON")?;
    if !(0.0..=1.0).contains(&raw.criteria.confidence) {
        bail!("the eval confidence {} is outside 0–1", raw.criteria.confidence);
    }
    if !matches!(raw.criteria.change_size.as_str(), "small" | "medium" | "large") {
        bail!("the eval change_size {:?} is not small, medium or large", raw.criteria.change_size);
    }
    if raw.reasons.is_empty() {
        bail!("the eval gives no reasons");
    }
    let (effective, escalations) = decide(raw.classification, &raw.criteria, min_confidence);
    Ok(EvalResult {
        classification: raw.classification.as_str().into(),
        effective: effective.as_str().into(),
        summary: raw.summary,
        reasons: raw.reasons,
        criteria: raw.criteria,
        escalations,
    })
}

/// The pipeline's own rule on top of the agent's class: an item goes
/// straight to implementation only when the agent says `simple` AND none of
/// the risk criteria holds. Otherwise it goes to refinement, where the
/// operator approves a plan. Returns the effective class and the reasons
/// for raising it.
pub fn decide(agent: Classification, c: &Criteria, min_confidence: f64) -> (Classification, Vec<String>) {
    if agent != Classification::Simple {
        return (agent, vec![]);
    }
    let mut why = Vec::new();
    if c.schema_impact {
        why.push("the change touches a schema".to_string());
    }
    if c.security_impact {
        why.push("the change has security impact".to_string());
    }
    if c.public_api_impact {
        why.push("the change touches a public API".to_string());
    }
    if c.change_size == "large" {
        why.push("the change is large".to_string());
    }
    if c.confidence < min_confidence {
        why.push(format!("the confidence {:.2} is below {:.2}", c.confidence, min_confidence));
    }
    if why.is_empty() {
        (Classification::Simple, why)
    } else {
        (Classification::Complex, why)
    }
}

// ── refinement ───────────────────────────────────────────────────────────

pub const PLAN_OPEN: &str = "===PLAN===";
pub const PLAN_CLOSE: &str = "===END PLAN===";

/// Split a refinement reply into the text for the operator and the plan it
/// proposes, if any. The plan markers are replaced by visible headings
/// (`label` names the plan version).
pub fn split_plan(reply: &str, label: &str) -> (String, Option<String>) {
    let Some(plan) = last_block(reply, PLAN_OPEN, PLAN_CLOSE) else {
        return (reply.trim().to_string(), None);
    };
    let plan = plan.to_string();
    let shown = reply
        .replace(PLAN_OPEN, &format!("── {label} ──"))
        .replace(PLAN_CLOSE, &format!("── end of {label} ──"));
    (shown.trim().to_string(), (!plan.is_empty()).then_some(plan))
}

/// An operator message in an item's thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorCommand {
    /// Approve the latest plan; `Some(v)` names the version the operator
    /// read, which must be the latest.
    ApprovePlan(Option<u32>),
    ApproveComment,
    SkipComment,
    Cancel,
    /// Release an item held for hidden content; the code names the hold
    /// the operator reviewed (`None` is refused with the current code).
    Release(Option<String>),
    /// Anything else: a message for the refinement agent or the thread.
    Message,
}

/// Read an operator message. Only a message that consists of the command
/// alone is a command, so a sentence that contains "approve" stays a
/// message. Approval is decided by code from the operator's own message
/// (never by the model), because it releases implementation work.
pub fn parse_command(text: &str) -> OperatorCommand {
    let t = text.trim().trim_end_matches(['.', '!']).trim().to_lowercase();
    let words: Vec<&str> = t.split_whitespace().collect();
    let version = |w: &str| w.strip_prefix('v').and_then(|n| n.parse::<u32>().ok());
    match words.as_slice() {
        ["approve"] | ["approved"] | ["approve", "plan"] => OperatorCommand::ApprovePlan(None),
        ["approve", v] | ["approve", "plan", v] if version(v).is_some() => OperatorCommand::ApprovePlan(version(v)),
        ["approve", "comment"] => OperatorCommand::ApproveComment,
        ["skip", "comment"] => OperatorCommand::SkipComment,
        ["cancel"] | ["cancel", "item"] => OperatorCommand::Cancel,
        ["release"] | ["release", "item"] => OperatorCommand::Release(None),
        ["release", code] if code.len() <= 64 && code.bytes().all(|c| c.is_ascii_hexdigit()) => {
            OperatorCommand::Release(Some(code.to_string()))
        }
        _ => OperatorCommand::Message,
    }
}

// ── WhatsApp groups ──────────────────────────────────────────────────────

/// True when one more group may be requested: fewer than `max_per_day`
/// requests in the 24 hours before `now`. `requested` holds the request
/// times (RFC3339).
pub fn group_budget_allows(requested: &[String], now: chrono::DateTime<chrono::Utc>, max_per_day: u32) -> bool {
    if max_per_day == 0 {
        return false;
    }
    let since = now - chrono::Duration::hours(24);
    let recent = requested
        .iter()
        .filter_map(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .filter(|t| t.with_timezone(&chrono::Utc) > since)
        .count();
    recent < max_per_day as usize
}

/// WhatsApp group subject for an item: `#<n> <short title>`, at most 60
/// characters.
pub fn group_subject(n: i64, title: &str) -> String {
    let head = format!("#{n} ");
    let room = 60usize.saturating_sub(head.chars().count());
    let title: String = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let short = if title.chars().count() > room {
        let mut s: String = title.chars().take(room.saturating_sub(1)).collect();
        s.push('…');
        s
    } else {
        title
    };
    format!("{head}{short}")
}

/// Branch name for an item: `nucleus/item-<n>`. Code-owned: no part of
/// the issue title goes into it, because the branch name is written into
/// the implementation agent's brief outside the data fence.
pub fn branch_name(n: i64) -> String {
    format!("nucleus/item-{n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_names_round_trip() {
        for s in Stage::ALL {
            assert_eq!(Stage::parse(s.as_str()), Some(s));
        }
        assert_eq!(Stage::parse("nope"), None);
    }

    #[test]
    fn transitions_follow_the_pipeline() {
        use Stage::*;
        use StageEvent as E;
        let ok = |from, ev: E, to| assert_eq!(transition(from, &ev).unwrap(), to, "{from:?} + {ev:?}");
        ok(Queued, E::EvalStarted, Eval);
        ok(Eval, E::EvalSimple, Implementation);
        ok(Eval, E::EvalNeedsPlan, Refinement);
        ok(Refinement, E::PlanApproved, Implementation);
        ok(Implementation, E::ImplementationDone, Pr);
        ok(Pr, E::PrOpened, Review);
        ok(Review, E::Finished, Closed);
        for s in [Queued, Eval, Refinement, Implementation, Pr, Review] {
            ok(s, E::Failed, Failed);
            ok(s, E::Cancel, Cancelled);
            ok(s, E::SourceClosed, Closed);
            ok(s, E::Stale, Stale);
        }
        ok(Failed, E::Stale, Stale);
        ok(Pr, E::Blocked, Blocked);
        ok(Review, E::Blocked, Blocked);
        ok(Blocked, E::Retry { failed_in: Pr }, Pr);
        ok(Blocked, E::Retry { failed_in: Review }, Review);
        ok(Queued, E::Blocked, Blocked);
        ok(Implementation, E::Blocked, Blocked);
        ok(Blocked, E::Retry { failed_in: Queued }, Queued);
        ok(Blocked, E::Cancel, Cancelled);
        ok(Blocked, E::Stale, Stale);
        ok(Failed, E::Cancel, Cancelled);
        ok(Failed, E::SourceClosed, Closed);
        ok(Failed, E::Retry { failed_in: Eval }, Queued);
        ok(Failed, E::Retry { failed_in: Queued }, Queued);
        ok(Failed, E::Retry { failed_in: Refinement }, Refinement);
        ok(Failed, E::Retry { failed_in: Implementation }, Implementation);
        ok(Failed, E::Retry { failed_in: Pr }, Pr);
        ok(Failed, E::Retry { failed_in: Review }, Review);
        for s in [Queued, Eval, Refinement, Implementation] {
            ok(s, E::Hold, Held);
            ok(Held, E::Release { held_in: s }, s);
        }
        ok(Held, E::Cancel, Cancelled);
        ok(Held, E::Stale, Stale);
        ok(Held, E::SourceClosed, Closed);
        ok(Failed, E::Retry { failed_in: Held }, Held);
    }

    #[test]
    fn transitions_refuse_what_does_not_apply() {
        use Stage::*;
        use StageEvent as E;
        let bad = |from, ev: E| assert!(transition(from, &ev).is_err(), "{from:?} + {ev:?} must fail");
        // No skipping: a simple eval cannot come from refinement, a plan
        // cannot be approved before refinement, a PR not before the work.
        bad(Queued, E::EvalSimple);
        bad(Queued, E::PlanApproved);
        bad(Eval, E::PlanApproved);
        bad(Refinement, E::EvalSimple);
        bad(Refinement, E::ImplementationDone);
        bad(Implementation, E::PlanApproved);
        bad(Implementation, E::PrOpened);
        bad(Review, E::PlanApproved);
        bad(Failed, E::Failed);
        bad(Refinement, E::Retry { failed_in: Eval });
        bad(Failed, E::Retry { failed_in: Closed });
        bad(Failed, E::Blocked);
        bad(Blocked, E::Blocked);
        bad(Blocked, E::Retry { failed_in: Closed });
        // Only a stage before an agent step holds; a release returns there.
        bad(Pr, E::Hold);
        bad(Review, E::Hold);
        bad(Held, E::Hold);
        bad(Refinement, E::Release { held_in: Refinement });
        bad(Held, E::Release { held_in: Pr });
        bad(Held, E::PlanApproved);
        // Terminal stages never change.
        for s in [Closed, Cancelled, Stale] {
            for ev in [E::Cancel, E::Failed, E::SourceClosed, E::Stale, E::Retry { failed_in: Eval }] {
                bad(s, ev);
            }
        }
    }

    fn eval_text(class: &str, conf: f64, size: &str, schema: bool) -> String {
        format!(
            "Thinking out loud.\n{EVAL_OPEN}\n{{\"classification\":\"{class}\",\"summary\":\"s\",\
             \"reasons\":[\"r1\"],\"criteria\":{{\"change_size\":\"{size}\",\"schema_impact\":{schema},\
             \"security_impact\":false,\"public_api_impact\":false,\"confidence\":{conf}}}}}\n{EVAL_CLOSE}\n"
        )
    }

    #[test]
    fn eval_parsing_and_escalation() {
        let e = parse_eval(&eval_text("simple", 0.9, "small", false), 0.7).unwrap();
        assert_eq!((e.classification.as_str(), e.effective.as_str()), ("simple", "simple"));
        assert!(e.escalations.is_empty());

        let e = parse_eval(&eval_text("simple", 0.5, "small", false), 0.7).unwrap();
        assert_eq!(e.effective, "complex");
        assert!(e.escalations[0].contains("confidence"), "{:?}", e.escalations);

        let e = parse_eval(&eval_text("simple", 0.95, "large", true), 0.7).unwrap();
        assert_eq!(e.effective, "complex");
        assert_eq!(e.escalations.len(), 2);

        // Feature and complex are never lowered.
        let e = parse_eval(&eval_text("feature", 0.99, "small", false), 0.7).unwrap();
        assert_eq!(e.effective, "feature");

        // The last block wins, and a code fence inside it is accepted.
        let fenced = eval_text("simple", 0.9, "small", false)
            .replace(&format!("{EVAL_OPEN}\n"), &format!("{EVAL_OPEN}\n```json\n"))
            .replace(&format!("\n{EVAL_CLOSE}"), &format!("\n```\n{EVAL_CLOSE}"));
        let t = format!("{}\n{fenced}", eval_text("complex", 0.9, "small", false));
        let e = parse_eval(&t, 0.7).unwrap();
        assert_eq!(e.classification, "simple");

        assert!(parse_eval("no block", 0.7).is_err());
        assert!(parse_eval(&eval_text("simple", 1.5, "small", false), 0.7).is_err());
        assert!(parse_eval(&eval_text("simple", 0.9, "huge", false), 0.7).is_err());
        assert!(parse_eval(&eval_text("trivial", 0.9, "small", false), 0.7).is_err());
        assert!(parse_eval(&eval_text("simple", 0.9, "small", false).replace("[\"r1\"]", "[]"), 0.7).is_err());
    }

    #[test]
    fn plan_extraction() {
        let (shown, plan) = split_plan("Here is the plan.\n===PLAN===\n1. a\n2. b\n===END PLAN===\nOK?", "plan v2");
        assert_eq!(plan.as_deref(), Some("1. a\n2. b"));
        assert!(shown.contains("── plan v2 ──") && shown.contains("── end of plan v2 ──") && !shown.contains("==="));
        let (shown, plan) = split_plan("Two questions first.", "plan v1");
        assert_eq!((shown.as_str(), plan), ("Two questions first.", None));
        let (_, plan) = split_plan("===PLAN===\n\n===END PLAN===", "plan v1");
        assert_eq!(plan, None, "an empty plan is not a plan");
    }

    #[test]
    fn operator_commands_are_whole_messages() {
        use OperatorCommand::*;
        assert_eq!(parse_command("approve"), ApprovePlan(None));
        assert_eq!(parse_command("  Approve!  "), ApprovePlan(None));
        assert_eq!(parse_command("approve plan"), ApprovePlan(None));
        assert_eq!(parse_command("approve v3"), ApprovePlan(Some(3)));
        assert_eq!(parse_command("approve plan v12"), ApprovePlan(Some(12)));
        assert_eq!(parse_command("approve comment"), ApproveComment);
        assert_eq!(parse_command("skip comment."), SkipComment);
        assert_eq!(parse_command("cancel"), Cancel);
        assert_eq!(parse_command("Release"), Release(None));
        assert_eq!(parse_command("release A1b2C3"), Release(Some("a1b2c3".into())));
        assert_eq!(parse_command("release now"), Message);
        assert_eq!(parse_command("release the item now"), Message);
        assert_eq!(parse_command("I approve of the idea but change step 2"), Message);
        assert_eq!(parse_command("approve vX"), Message);
        assert_eq!(parse_command("please cancel the second step"), Message);
    }

    #[test]
    fn group_budget_counts_the_last_24_hours() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-24T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let old = "2026-09-23T11:00:00.000Z".to_string();
        let recent = "2026-09-24T09:00:00.000Z".to_string();
        assert!(group_budget_allows(&[], now, 3));
        assert!(group_budget_allows(&[recent.clone(), recent.clone(), old.clone(), old.clone()], now, 3));
        assert!(!group_budget_allows(&[recent.clone(), recent.clone(), recent.clone()], now, 3));
        assert!(!group_budget_allows(&[], now, 0), "0 disables group creation");
    }

    #[test]
    fn subjects_and_branches() {
        assert_eq!(group_subject(7, "Fix  the\ttypo"), "#7 Fix the typo");
        let long = group_subject(12, &"word ".repeat(40));
        assert_eq!(long.chars().count(), 60);
        assert!(long.starts_with("#12 word") && long.ends_with('…'));
        assert_eq!(branch_name(3), "nucleus/item-3");
    }
}
