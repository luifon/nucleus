//! `nucleus skills` — skill suggestions from the terminal (ADR-038).
//!
//! The skill-gap learner files suggestions (a new skill, or a full
//! replacement of a personal skill) instead of writing skills; the operator
//! reads and decides them here or on the dashboard. A session that finds a
//! loaded skill wrong or incomplete files a patch suggestion with
//! `nucleus skills suggest`.
//!
//! Who may do what is decided from the process tree (`crate::caller`), not
//! from arguments: only the operator (a terminal or a Claude Code session he
//! started, not reacting to an agent message) may accept, reject or edit; a
//! suggestion only waits for him, so any session may file one.
//!
//! Examples:
//!   nucleus skills suggestions list
//!   nucleus skills suggestions show 12
//!   nucleus skills suggestions accept 12
//!   nucleus skills suggestions reject 12 --reason "too narrow"
//!   nucleus skills suggestions edit 12 --file draft.md
//!   nucleus skills suggest --kind patch --name some-skill --base <sha256> \
//!     --rationale "step 3 uses the old flag" --file - <<'EOF'
//!   …full SKILL.md…
//!   EOF

use crate::caller::{self, Caller, Role};
use crate::skill_learner::{
    self,
    suggestions::{
        self, EvidenceSource, NewEvidence, NewSuggestion, SuggestionKind, SuggestionOrigin, SuggestionStatus,
    },
};
use crate::skills::LibraryRoots;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use std::io::Read;
use std::path::Path;

#[derive(Parser)]
#[command(name = "skills", about = "Skill suggestions: list, show, accept, reject, edit, file (ADR-038)")]
struct Cli {
    // No --workspace-root: the caller check reads the caller's transcript
    // under the configured workspace, and a second spelling of the same
    // directory would make that check look in the wrong place.
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Read and decide the suggestions.
    Suggestions {
        #[command(subcommand)]
        cmd: SuggestionsCmd,
    },
    /// File a suggestion: a new skill or a full replacement of a personal
    /// skill. Prints its id.
    Suggest {
        #[arg(long, value_enum)]
        kind: KindArg,
        /// The new skill's name, or the personal skill to replace.
        #[arg(long)]
        name: String,
        /// Why the library should change.
        #[arg(long)]
        rationale: String,
        /// The full proposed SKILL.md; `-` reads it from stdin.
        #[arg(long)]
        file: String,
        /// Patch only, required: the SHA-256 of the SKILL.md you read before
        /// writing the draft (`shasum -a 256 <path>`). The suggestion is
        /// refused if the skill changed since, so a draft never replaces a
        /// version its author did not see.
        #[arg(long)]
        base: Option<String>,
        /// The session the evidence comes from. Only the operator may name
        /// one; a session's own id is taken from its process.
        #[arg(long)]
        session: Option<String>,
        /// A short excerpt of the session that shows the need (repeatable).
        #[arg(long = "quote")]
        quotes: Vec<String>,
        /// A rejected suggestion for the same skill this one argues past.
        #[arg(long, requires = "why_new")]
        overrides_rejected: Option<i64>,
        /// What is new since that rejection.
        #[arg(long)]
        why_new: Option<String>,
    },
}

#[derive(Subcommand)]
enum SuggestionsCmd {
    /// List suggestions (pending by default).
    List {
        #[arg(long, value_enum, default_value = "pending")]
        status: StatusArg,
        #[arg(long)]
        json: bool,
    },
    /// Show one suggestion with its draft and evidence.
    Show {
        id: i64,
        #[arg(long)]
        json: bool,
    },
    /// Write the draft into the personal skill tree and mark it accepted.
    Accept { id: i64 },
    /// Reject a pending suggestion. Later reviews see the reason.
    Reject {
        id: i64,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Replace the draft of a pending suggestion; `--file -` reads stdin.
    Edit {
        id: i64,
        #[arg(long)]
        file: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum KindArg {
    Create,
    Patch,
}

#[derive(Clone, Copy, ValueEnum)]
enum StatusArg {
    Pending,
    Accepted,
    Rejected,
    Superseded,
    All,
}

fn read_text(file: &str) -> Result<String> {
    if file == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).context("reading stdin")?;
        return Ok(s);
    }
    std::fs::read_to_string(file).with_context(|| format!("reading {file}"))
}

/// Accept, reject and edit change the skill library or the operator's
/// decisions: the operator only, never a turn reacting to an agent message.
fn require_operator(caller: &Caller, what: &str) -> Result<()> {
    if caller.role != Role::Operator {
        bail!("only the operator may {what} a suggestion (caller: {:?})", caller.role);
    }
    if caller.inbound_hop > 0 {
        bail!("this turn is reacting to an agent message; {what} the suggestion from a turn of your own");
    }
    Ok(())
}

pub async fn run(args: Vec<std::ffi::OsString>) -> Result<()> {
    let cli = Cli::parse_from(args);
    let ws = crate::config::Settings::load().context("loading settings")?.workspace_root()?;
    let roots = LibraryRoots::for_workspace(&ws);
    match cli.cmd {
        Cmd::Suggestions { cmd } => suggestions_cmd(&ws, &roots, cmd).await,
        Cmd::Suggest { kind, name, rationale, file, base, session, quotes, overrides_rejected, why_new } => {
            let base = match (kind, base) {
                (KindArg::Patch, None) => {
                    bail!("a patch needs --base <sha256 of the SKILL.md you read before drafting>")
                }
                (KindArg::Create, Some(_)) => bail!("--base is for patches"),
                (_, b) => b.map(|b| b.trim().to_lowercase()),
            };
            let caller = caller::detect(&ws).await?;
            if let Role::Unknown(reason) = &caller.role {
                bail!("cannot tell who is calling ({reason}); refusing to file a suggestion");
            }
            let session_id = match (session, &caller.role) {
                (Some(s), Role::Operator) => Some(s),
                (Some(_), _) => bail!("--session is for the operator; a session files under its own id"),
                (None, _) => caller.session_id.clone(),
            };
            let draft = read_text(&file)?;
            let pool = skill_learner::open(&ws).await?;
            let s = NewSuggestion {
                kind: match kind {
                    KindArg::Create => SuggestionKind::Create,
                    KindArg::Patch => SuggestionKind::Patch,
                },
                name,
                draft,
                rationale,
                origin: SuggestionOrigin::Manual,
                expected_base: base,
                overrides_rejected,
                why_new,
                evidence: NewEvidence {
                    session_id,
                    source: EvidenceSource::Manual,
                    agent: caller.agent.clone(),
                    note: String::new(),
                    quotes,
                },
            };
            let id = suggestions::create(&pool, &ws, &roots, &s).await?;
            println!("filed suggestion #{id} ({} {}). The operator decides it: nucleus skills suggestions show {id}", s.kind.as_str(), s.name);
            Ok(())
        }
    }
}

async fn suggestions_cmd(ws: &Path, roots: &LibraryRoots, cmd: SuggestionsCmd) -> Result<()> {
    match cmd {
        SuggestionsCmd::List { status, json } => {
            let Some(pool) = skill_learner::open_read_only(ws).await? else {
                if json {
                    println!("[]");
                } else {
                    println!("no suggestions yet");
                }
                return Ok(());
            };
            let statuses: Vec<SuggestionStatus> = match status {
                StatusArg::Pending => vec![SuggestionStatus::Pending],
                StatusArg::Accepted => vec![SuggestionStatus::Accepted],
                StatusArg::Rejected => vec![SuggestionStatus::Rejected],
                StatusArg::Superseded => vec![SuggestionStatus::Superseded],
                StatusArg::All => vec![],
            };
            let rows = suggestions::list(&pool, &statuses).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            if rows.is_empty() {
                println!("no suggestions");
            }
            for s in &rows {
                println!(
                    "#{:<4} {:<10} {:<7} {:<8} {:<34} {} evidence  {}",
                    s.id,
                    s.status.as_str(),
                    s.kind.as_str(),
                    s.origin.as_str(),
                    s.name,
                    s.evidence_count,
                    s.created_at.get(..10).unwrap_or("")
                );
                println!("      {}", one_line(&s.rationale, 110));
                if let Some(r) = &s.reject_reason {
                    println!("      rejected: {}", one_line(r, 100));
                }
            }
            Ok(())
        }
        SuggestionsCmd::Show { id, json } => {
            let pool = skill_learner::open_read_only(ws).await?.context("there are no suggestions yet")?;
            let s = suggestions::get(&pool, id).await?.with_context(|| format!("there is no suggestion #{id}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
                return Ok(());
            }
            println!("#{} {} {} — {} ({}, filed {})", s.id, s.kind.as_str(), s.name, s.status.as_str(), s.origin.as_str(), s.created_at);
            if let Some(r) = &s.reject_reason {
                println!("rejected: {r}");
            }
            if let Some(n) = s.superseded_by {
                println!("superseded by #{n}");
            }
            if let (Some(o), Some(w)) = (s.overrides_rejected, &s.why_new) {
                println!("argues past rejected #{o}: {w}");
            }
            if s.edited {
                println!("draft edited by the operator");
            }
            println!("\nrationale: {}\n", s.rationale);
            println!("evidence:");
            for e in &s.evidence {
                println!(
                    "  - {} {}{}{}",
                    e.source.as_str(),
                    e.session_id.as_deref().unwrap_or("(no session)"),
                    e.agent.as_deref().map(|a| format!(" [{a}]")).unwrap_or_default(),
                    if e.note.is_empty() { String::new() } else { format!(": {}", e.note) }
                );
                for q in &e.quotes {
                    println!("      > {}", one_line(q, 200));
                }
            }
            if s.kind == SuggestionKind::Patch {
                let live = roots.personal.join(&s.name).join(crate::skills::SKILL_FILE);
                let state = match (std::fs::read(&live), &s.base_hash) {
                    (Ok(b), Some(h)) if skill_learner::content_hash(&b) == *h => "unchanged since the suggestion",
                    (Ok(_), _) => "CHANGED since the suggestion; accept will refuse it",
                    (Err(_), _) => "MISSING; accept will refuse it",
                };
                println!("\nlive skill {}: {state}", live.display());
            }
            println!("\n----- draft SKILL.md -----\n{}", s.draft);
            Ok(())
        }
        SuggestionsCmd::Accept { id } => {
            require_operator(&caller::detect(ws).await?, "accept")?;
            let pool = skill_learner::open(ws).await?;
            let out = suggestions::accept(&pool, ws, roots, id).await?;
            println!("accepted #{id}: wrote {}", out.path);
            if !out.superseded.is_empty() {
                let ids: Vec<String> = out.superseded.iter().map(|i| format!("#{i}")).collect();
                println!("superseded the other pending suggestions for {}: {}", out.name, ids.join(", "));
            }
            Ok(())
        }
        SuggestionsCmd::Reject { id, reason } => {
            require_operator(&caller::detect(ws).await?, "reject")?;
            let pool = skill_learner::open(ws).await?;
            suggestions::reject(&pool, id, reason.as_deref()).await?;
            println!("rejected #{id}");
            Ok(())
        }
        SuggestionsCmd::Edit { id, file } => {
            require_operator(&caller::detect(ws).await?, "edit")?;
            let draft = read_text(&file)?;
            let pool = skill_learner::open(ws).await?;
            suggestions::edit(&pool, ws, roots, id, &draft).await?;
            println!("replaced the draft of #{id}");
            Ok(())
        }
    }
}

/// `s` on one line, cut to `max` characters.
fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
