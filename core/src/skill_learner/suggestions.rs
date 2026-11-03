//! Skill suggestions (ADR-038): what the transcript review proposes and the
//! operator accepts or rejects.
//!
//! A suggestion is either `create` (a new skill) or `patch` (a full
//! replacement SKILL.md for an existing skill of the personal tree). A patch
//! is bound to the SHA-256 of the live SKILL.md it was written against;
//! accept refuses it when the live file has changed since.
//!
//! Every draft is checked before it is stored and again at accept
//! ([`prepare_draft`]): the ADR-017 contract (`skills::validate`), strict
//! YAML frontmatter (Claude Code's loader is strict), a class-level
//! kebab-case name, no credential-shaped text, a size limit. Code sets the
//! ownership fields: a created skill gets `flavor: learned` and
//! `created_by: agent`; a patch keeps the `created_by`, `flavor` and
//! `pinned` of the skill it replaces, so a patch never changes who owns a
//! skill or whether the curator may archive it.
//!
//! Accept writes only into the personal tree
//! (`skills::personal_skills_root`), never the committed `.claude/skills/`
//! and never the machine-wide tree.

use super::content_hash;
use crate::skills::{self, LibraryRoots};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};

/// Largest draft SKILL.md stored.
pub const MAX_DRAFT_BYTES: usize = 24 * 1024;
/// Longest skill name.
pub const MAX_NAME_CHARS: usize = 64;
/// Longest rationale, note, reject reason or "why new" text.
pub const MAX_TEXT_CHARS: usize = 1200;
/// Most quotes per evidence row, and the longest quote.
pub const MAX_QUOTES: usize = 5;
pub const MAX_QUOTE_CHARS: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum SuggestionKind {
    Create,
    Patch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum SuggestionStatus {
    Pending,
    Accepted,
    Rejected,
    /// Replaced by a revised suggestion (`superseded_by`), or made moot by
    /// another suggestion for the same skill being accepted.
    Superseded,
}

/// Which pass filed a suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum SuggestionOrigin {
    /// The live transcript sweep.
    Live,
    /// The backfill over old sessions.
    Backfill,
    /// `nucleus skills suggest`, from a session or the operator.
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum EvidenceSource {
    /// A Claude Code transcript.
    Transcript,
    /// The session index (the transcript file no longer exists).
    Index,
    /// Filed by hand with `nucleus skills suggest`.
    Manual,
}

macro_rules! str_enum {
    ($t:ty { $($v:ident => $s:literal),* $(,)? }) => {
        impl $t {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$v => $s),* }
            }
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some(Self::$v),)* _ => None }
            }
        }
    };
}
str_enum!(SuggestionKind { Create => "create", Patch => "patch" });
str_enum!(SuggestionStatus { Pending => "pending", Accepted => "accepted", Rejected => "rejected", Superseded => "superseded" });
str_enum!(SuggestionOrigin { Live => "live", Backfill => "backfill", Manual => "manual" });
str_enum!(EvidenceSource { Transcript => "transcript", Index => "index", Manual => "manual" });

/// One piece of evidence: a session the suggestion came from.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct Evidence {
    #[ts(type = "number")]
    pub id: i64,
    pub session_id: Option<String>,
    pub source: EvidenceSource,
    /// Agent label of the session (null for terminal sessions).
    pub agent: Option<String>,
    pub note: String,
    /// Short excerpts of the session's turns.
    pub quotes: Vec<String>,
    pub created_at: String,
}

/// A suggestion with its full draft and evidence.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct Suggestion {
    #[ts(type = "number")]
    pub id: i64,
    pub kind: SuggestionKind,
    /// The new skill's name (`create`) or the skill to replace (`patch`).
    pub name: String,
    /// The full proposed SKILL.md.
    pub draft: String,
    pub rationale: String,
    /// `patch` only: SHA-256 of the live SKILL.md the draft replaces.
    pub base_hash: Option<String>,
    pub origin: SuggestionOrigin,
    pub status: SuggestionStatus,
    pub reject_reason: Option<String>,
    #[ts(type = "number | null")]
    pub superseded_by: Option<i64>,
    /// A rejected suggestion for the same skill this one argues past.
    #[ts(type = "number | null")]
    pub overrides_rejected: Option<i64>,
    /// Why this is suggested again despite `overrides_rejected`.
    pub why_new: Option<String>,
    /// The operator replaced the draft.
    pub edited: bool,
    pub created_at: String,
    pub updated_at: String,
    pub decided_at: Option<String>,
    pub evidence: Vec<Evidence>,
}

/// A suggestion without its draft, for lists.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct SuggestionSummary {
    #[ts(type = "number")]
    pub id: i64,
    pub kind: SuggestionKind,
    pub name: String,
    pub status: SuggestionStatus,
    pub origin: SuggestionOrigin,
    pub rationale: String,
    pub reject_reason: Option<String>,
    #[ts(type = "number | null")]
    pub superseded_by: Option<i64>,
    pub edited: bool,
    #[ts(type = "number")]
    pub evidence_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// Evidence to attach.
#[derive(Debug, Clone)]
pub struct NewEvidence {
    pub session_id: Option<String>,
    pub source: EvidenceSource,
    pub agent: Option<String>,
    pub note: String,
    pub quotes: Vec<String>,
}

/// A suggestion to file.
#[derive(Debug, Clone)]
pub struct NewSuggestion {
    pub kind: SuggestionKind,
    pub name: String,
    pub draft: String,
    pub rationale: String,
    pub origin: SuggestionOrigin,
    /// `patch` only: the hash of the live SKILL.md the draft was written
    /// against. `None` binds it to the live file as it is now.
    pub expected_base: Option<String>,
    pub overrides_rejected: Option<i64>,
    pub why_new: Option<String>,
    pub evidence: NewEvidence,
}

/// What accept did.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct AcceptOutcome {
    #[ts(type = "number")]
    pub id: i64,
    pub name: String,
    /// The SKILL.md written.
    pub path: String,
    /// Other pending suggestions for the same skill, now superseded.
    #[ts(type = "number[]")]
    pub superseded: Vec<i64>,
}

/// A checked draft, ready to store or write.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedDraft {
    pub text: String,
    /// `patch`: hash of the live SKILL.md.
    pub base_hash: Option<String>,
}

/// True for a class-level skill name: lowercase kebab-case, at most
/// [`MAX_NAME_CHARS`].
pub fn valid_name(name: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"^[a-z0-9]+(?:-[a-z0-9]+)*$").expect("valid regex"));
    name.chars().count() <= MAX_NAME_CHARS && re.is_match(name) && skills::validate_dir_name(name).is_ok()
}

/// `text` trimmed and cut to `max` characters (for stored free text).
fn clip(text: &str, max: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let mut out: String = t.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Replace or add top-level frontmatter keys (`None` removes a key). Works
/// on lines, so the rest of the block keeps its exact text; a replaced
/// key's indented continuation lines go with it. Errors when there is no
/// `---` block.
pub fn set_frontmatter(content: &str, fields: &[(&str, Option<&str>)]) -> Result<String> {
    let content = content.replace("\r\n", "\n");
    let rest = content.strip_prefix("---\n").ok_or_else(|| anyhow!("missing `---` YAML frontmatter block"))?;
    let end = rest.find("\n---\n").or_else(|| rest.strip_suffix("\n---").map(|r| r.len()));
    let end = end.ok_or_else(|| anyhow!("the frontmatter block is not closed with `---`"))?;
    let (yaml, body) = (&rest[..end], &rest[end..]);
    let mut out_lines: Vec<String> = Vec::new();
    let mut skipping = false;
    let mut done = vec![false; fields.len()];
    for line in yaml.lines() {
        let top = !line.starts_with(' ') && !line.starts_with('\t') && !line.is_empty();
        if skipping && !top {
            continue;
        }
        skipping = false;
        let key = line.split_once(':').map(|(k, _)| k.trim()).filter(|_| top);
        if let Some(i) = key.and_then(|k| fields.iter().position(|(f, _)| *f == k)) {
            skipping = true;
            if !done[i] {
                done[i] = true;
                if let Some(v) = fields[i].1 {
                    out_lines.push(format!("{}: {v}", fields[i].0));
                }
            }
            continue;
        }
        out_lines.push(line.to_string());
    }
    for (i, (k, v)) in fields.iter().enumerate() {
        if let (false, Some(v)) = (done[i], v) {
            out_lines.push(format!("{k}: {v}"));
        }
    }
    Ok(format!("---\n{}{}", out_lines.join("\n"), body))
}

/// The personal skill `name` as it is on disk: its SKILL.md path and bytes.
/// Errors when it does not exist or its directory is a symlink.
fn live_personal_skill(roots: &LibraryRoots, name: &str) -> Result<(PathBuf, Vec<u8>)> {
    let dir = roots.personal.join(name);
    let meta = std::fs::symlink_metadata(&dir)
        .map_err(|_| anyhow!("there is no personal skill `{name}` to patch"))?;
    if meta.file_type().is_symlink() {
        bail!("the personal skill `{name}` is a symlink; patch its target by hand");
    }
    let path = dir.join(skills::SKILL_FILE);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok((path, bytes))
}

/// Check a draft and set its ownership fields. `Err` carries every problem
/// found, so a caller can ask for one corrected draft.
///
/// `base`: for a `patch`, `Some(hash)` requires the live SKILL.md to still
/// have that hash; `None` takes the live file as it is now.
pub fn prepare_draft(
    workspace_root: &Path,
    roots: &LibraryRoots,
    kind: SuggestionKind,
    name: &str,
    draft: &str,
    base: Option<&str>,
) -> std::result::Result<PreparedDraft, Vec<String>> {
    let mut issues = Vec::new();
    if !valid_name(name) {
        issues.push(format!(
            "name {name:?} is not a class-level kebab-case name (a-z, 0-9 and single hyphens, at most {MAX_NAME_CHARS} characters)"
        ));
        return Err(issues);
    }
    if draft.len() > MAX_DRAFT_BYTES {
        issues.push(format!("the draft is {} bytes; the limit is {MAX_DRAFT_BYTES}", draft.len()));
        return Err(issues);
    }
    let (fields, base_hash): (Vec<(&str, Option<String>)>, Option<String>) = match kind {
        SuggestionKind::Create => {
            let lib = skills::read_library(roots);
            let taken = lib.active_tiers_named(&[name], None);
            if !taken.is_empty() {
                let tiers: Vec<&str> = taken.iter().map(|t| t.as_str()).collect();
                issues.push(format!("a skill named `{name}` already exists (tier {})", tiers.join(", ")));
                return Err(issues);
            }
            (
                vec![
                    ("name", Some(name.to_string())),
                    ("flavor", Some("learned".into())),
                    ("created_by", Some("agent".into())),
                ],
                None,
            )
        }
        SuggestionKind::Patch => {
            let (path, bytes) = match live_personal_skill(roots, name) {
                Ok(v) => v,
                Err(e) => return Err(vec![format!("{e:#}")]),
            };
            let hash = content_hash(&bytes);
            if let Some(expected) = base {
                if expected != hash {
                    return Err(vec![format!("the skill `{name}` changed after the draft was written")]);
                }
            }
            let live = String::from_utf8_lossy(&bytes);
            let fm = skills::parse_frontmatter(&live, &path).unwrap_or_default();
            let mut fields = vec![
                ("name", Some(name.to_string())),
                ("created_by", fm.created_by.clone()),
                ("pinned", fm.pinned.then(|| "true".to_string())),
            ];
            if let Some(flavor) = fm.flavor.filter(|f| !f.trim().is_empty()) {
                fields.push(("flavor", Some(flavor)));
            }
            (fields, Some(hash))
        }
    };
    let field_refs: Vec<(&str, Option<&str>)> = fields.iter().map(|(k, v)| (*k, v.as_deref())).collect();
    let text = match set_frontmatter(draft.trim_start_matches('\u{feff}'), &field_refs) {
        Ok(t) => t,
        Err(e) => return Err(vec![format!("{e:#}")]),
    };
    let text = if text.ends_with('\n') { text } else { format!("{text}\n") };
    if text.len() > MAX_DRAFT_BYTES {
        issues.push(format!("the draft is {} bytes with its frontmatter set; the limit is {MAX_DRAFT_BYTES}", text.len()));
    }
    issues.extend(skills::validate(&text));
    if let Some(yaml) = skills::split_frontmatter(&text) {
        if let Err(e) = serde_yaml::from_str::<skills::Frontmatter>(yaml) {
            issues.push(format!(
                "the frontmatter is not valid YAML ({e}); quote any value that contains `: `"
            ));
        }
    }
    let redacted = crate::secret_filter::CredentialRules::from_workspace(workspace_root).redact(&text);
    if !redacted.hits.is_empty() {
        let mut kinds = redacted.hits.clone();
        kinds.sort();
        kinds.dedup();
        issues.push(format!("the draft contains credential-shaped text ({})", kinds.join(", ")));
    }
    if issues.is_empty() { Ok(PreparedDraft { text, base_hash }) } else { Err(issues) }
}

fn evidence_quotes(quotes: &[String]) -> String {
    let q: Vec<String> = quotes
        .iter()
        .map(|q| clip(q, MAX_QUOTE_CHARS))
        .filter(|q| !q.is_empty())
        .take(MAX_QUOTES)
        .collect();
    serde_json::to_string(&q).unwrap_or_else(|_| "[]".into())
}

async fn insert_evidence(conn: &mut sqlx::SqliteConnection, suggestion_id: i64, e: &NewEvidence) -> Result<()> {
    sqlx::query(
        "INSERT INTO suggestion_evidence (suggestion_id, session_id, source, agent, note, quotes, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(suggestion_id)
    .bind(e.session_id.as_deref())
    .bind(e.source.as_str())
    .bind(e.agent.as_deref())
    .bind(clip(&e.note, MAX_TEXT_CHARS))
    .bind(evidence_quotes(&e.quotes))
    .bind(crate::timestamp::now())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The pending suggestion for skill `name`, if any.
async fn pending_for(conn: &mut sqlx::SqliteConnection, name: &str) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar("SELECT id FROM suggestions WHERE name = ?1 AND status = 'pending' ORDER BY id LIMIT 1")
        .bind(name)
        .fetch_optional(&mut *conn)
        .await?)
}

/// Check the rejected-before rule for `s` inside a transaction.
async fn check_rejected(conn: &mut sqlx::SqliteConnection, s: &NewSuggestion) -> Result<()> {
    let rejected: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT id, reject_reason FROM suggestions WHERE name = ?1 AND status = 'rejected' ORDER BY id",
    )
    .bind(&s.name)
    .fetch_all(&mut *conn)
    .await?;
    let Some((last, reason)) = rejected.last() else {
        if s.overrides_rejected.is_some() {
            bail!("overrides_rejected names no rejected suggestion for `{}`", s.name);
        }
        return Ok(());
    };
    match s.overrides_rejected {
        Some(id) if rejected.iter().any(|(r, _)| *r == id) => {
            if s.why_new.as_deref().map(str::trim).unwrap_or("").is_empty() {
                bail!("suggesting `{}` again after the rejection of #{id} needs why_new", s.name);
            }
            Ok(())
        }
        Some(id) => bail!("#{id} is not a rejected suggestion for `{}`", s.name),
        None => bail!(
            "a suggestion for `{}` was rejected before (#{last}{}); name it in overrides_rejected and give why_new",
            s.name,
            reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default()
        ),
    }
}

/// File a new pending suggestion. Refused when another suggestion for the
/// same skill is pending (add evidence to it instead), when the skill was
/// rejected before and `overrides_rejected`/`why_new` do not argue past it,
/// or when the draft fails [`prepare_draft`].
pub async fn create(
    pool: &SqlitePool,
    workspace_root: &Path,
    roots: &LibraryRoots,
    s: &NewSuggestion,
) -> Result<i64> {
    let prepared = prepare_draft(workspace_root, roots, s.kind, &s.name, &s.draft, s.expected_base.as_deref())
        .map_err(|issues| anyhow!("the draft for `{}` is not valid: {}", s.name, issues.join("; ")))?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(id) = pending_for(&mut tx, &s.name).await? {
        bail!("suggestion #{id} for `{}` is already pending; add evidence to it or revise it", s.name);
    }
    check_rejected(&mut tx, s).await?;
    let now = crate::timestamp::now();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO suggestions (kind, name, draft, rationale, base_hash, origin, status,
                                  overrides_rejected, why_new, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9, ?9) RETURNING id",
    )
    .bind(s.kind.as_str())
    .bind(&s.name)
    .bind(&prepared.text)
    .bind(clip(&s.rationale, MAX_TEXT_CHARS))
    .bind(prepared.base_hash.as_deref())
    .bind(s.origin.as_str())
    .bind(s.overrides_rejected)
    .bind(s.why_new.as_deref().map(|w| clip(w, MAX_TEXT_CHARS)))
    .bind(&now)
    .fetch_one(&mut *tx)
    .await?;
    insert_evidence(&mut tx, id, &s.evidence).await?;
    tx.commit().await?;
    Ok(id)
}

/// Attach evidence to a pending suggestion.
pub async fn add_evidence(pool: &SqlitePool, id: i64, e: &NewEvidence) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let touched = sqlx::query("UPDATE suggestions SET updated_at = ?2 WHERE id = ?1 AND status = 'pending'")
        .bind(id)
        .bind(crate::timestamp::now())
        .execute(&mut *tx)
        .await?;
    if touched.rows_affected() == 0 {
        bail!("suggestion #{id} is not pending");
    }
    insert_evidence(&mut tx, id, e).await?;
    tx.commit().await?;
    Ok(())
}

/// Replace a pending suggestion with a revised one for the same skill: the
/// new suggestion carries the old one's evidence plus `s.evidence`, and the
/// old one becomes `superseded`. Refused for a suggestion the operator
/// edited (his draft is not replaced by a model's).
pub async fn revise(
    pool: &SqlitePool,
    workspace_root: &Path,
    roots: &LibraryRoots,
    old_id: i64,
    s: &NewSuggestion,
) -> Result<i64> {
    let prepared = prepare_draft(workspace_root, roots, s.kind, &s.name, &s.draft, s.expected_base.as_deref())
        .map_err(|issues| anyhow!("the revised draft for `{}` is not valid: {}", s.name, issues.join("; ")))?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    #[allow(clippy::type_complexity)]
    let old: Option<(String, String, String, i64, Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT kind, name, status, edited, overrides_rejected, why_new FROM suggestions WHERE id = ?1",
    )
    .bind(old_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((kind, name, status, edited, overrides, why_new)) = old else {
        bail!("there is no suggestion #{old_id}");
    };
    if status != "pending" {
        bail!("suggestion #{old_id} is {status}, not pending");
    }
    if kind != s.kind.as_str() || name != s.name {
        bail!("a revision of #{old_id} must keep its kind ({kind}) and skill name ({name})");
    }
    if edited != 0 {
        bail!("suggestion #{old_id} was edited by the operator; add evidence instead");
    }
    let now = crate::timestamp::now();
    let new_id: i64 = sqlx::query_scalar(
        "INSERT INTO suggestions (kind, name, draft, rationale, base_hash, origin, status,
                                  overrides_rejected, why_new, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9, ?9) RETURNING id",
    )
    .bind(s.kind.as_str())
    .bind(&s.name)
    .bind(&prepared.text)
    .bind(clip(&s.rationale, MAX_TEXT_CHARS))
    .bind(prepared.base_hash.as_deref())
    .bind(s.origin.as_str())
    .bind(overrides)
    .bind(why_new)
    .bind(&now)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO suggestion_evidence (suggestion_id, session_id, source, agent, note, quotes, created_at)
         SELECT ?2, session_id, source, agent, note, quotes, created_at
           FROM suggestion_evidence WHERE suggestion_id = ?1 ORDER BY id",
    )
    .bind(old_id)
    .bind(new_id)
    .execute(&mut *tx)
    .await?;
    insert_evidence(&mut tx, new_id, &s.evidence).await?;
    sqlx::query(
        "UPDATE suggestions SET status = 'superseded', superseded_by = ?2, updated_at = ?3, decided_at = ?3
          WHERE id = ?1 AND status = 'pending'",
    )
    .bind(old_id)
    .bind(new_id)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(new_id)
}

fn summary_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<SuggestionSummary> {
    let kind: String = r.get("kind");
    let status: String = r.get("status");
    let origin: String = r.get("origin");
    Ok(SuggestionSummary {
        id: r.get("id"),
        kind: SuggestionKind::parse(&kind).ok_or_else(|| anyhow!("unknown kind {kind}"))?,
        name: r.get("name"),
        status: SuggestionStatus::parse(&status).ok_or_else(|| anyhow!("unknown status {status}"))?,
        origin: SuggestionOrigin::parse(&origin).ok_or_else(|| anyhow!("unknown origin {origin}"))?,
        rationale: r.get("rationale"),
        reject_reason: r.get("reject_reason"),
        superseded_by: r.get("superseded_by"),
        edited: r.get::<i64, _>("edited") != 0,
        evidence_count: r.get("evidence_count"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    })
}

/// Suggestions with one of `statuses` (all when empty), newest first.
pub async fn list(pool: &SqlitePool, statuses: &[SuggestionStatus]) -> Result<Vec<SuggestionSummary>> {
    let rows = sqlx::query(
        "SELECT s.id, s.kind, s.name, s.status, s.origin, s.rationale, s.reject_reason,
                s.superseded_by, s.edited, s.created_at, s.updated_at,
                (SELECT COUNT(*) FROM suggestion_evidence e WHERE e.suggestion_id = s.id) AS evidence_count
           FROM suggestions s ORDER BY s.id DESC",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for r in &rows {
        let s = summary_from_row(r)?;
        if statuses.is_empty() || statuses.contains(&s.status) {
            out.push(s);
        }
    }
    Ok(out)
}

/// One suggestion with its draft and evidence.
pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Suggestion>> {
    let Some(r) = sqlx::query("SELECT * FROM suggestions WHERE id = ?1").bind(id).fetch_optional(pool).await?
    else {
        return Ok(None);
    };
    let ev = sqlx::query(
        "SELECT id, session_id, source, agent, note, quotes, created_at
           FROM suggestion_evidence WHERE suggestion_id = ?1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let mut evidence = Vec::new();
    for e in &ev {
        let source: String = e.get("source");
        let quotes: String = e.get("quotes");
        evidence.push(Evidence {
            id: e.get("id"),
            session_id: e.get("session_id"),
            source: EvidenceSource::parse(&source).ok_or_else(|| anyhow!("unknown source {source}"))?,
            agent: e.get("agent"),
            note: e.get("note"),
            quotes: serde_json::from_str(&quotes).unwrap_or_default(),
            created_at: e.get("created_at"),
        });
    }
    let kind: String = r.get("kind");
    let status: String = r.get("status");
    let origin: String = r.get("origin");
    Ok(Some(Suggestion {
        id: r.get("id"),
        kind: SuggestionKind::parse(&kind).ok_or_else(|| anyhow!("unknown kind {kind}"))?,
        name: r.get("name"),
        draft: r.get("draft"),
        rationale: r.get("rationale"),
        base_hash: r.get("base_hash"),
        origin: SuggestionOrigin::parse(&origin).ok_or_else(|| anyhow!("unknown origin {origin}"))?,
        status: SuggestionStatus::parse(&status).ok_or_else(|| anyhow!("unknown status {status}"))?,
        reject_reason: r.get("reject_reason"),
        superseded_by: r.get("superseded_by"),
        overrides_rejected: r.get("overrides_rejected"),
        why_new: r.get("why_new"),
        edited: r.get::<i64, _>("edited") != 0,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        decided_at: r.get("decided_at"),
        evidence,
    }))
}

/// Reject a pending suggestion, with an optional reason the later reviews
/// see.
pub async fn reject(pool: &SqlitePool, id: i64, reason: Option<&str>) -> Result<()> {
    let now = crate::timestamp::now();
    let reason = reason.map(|r| clip(r, MAX_TEXT_CHARS)).filter(|r| !r.is_empty());
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let done = sqlx::query(
        "UPDATE suggestions SET status = 'rejected', reject_reason = ?2, updated_at = ?3, decided_at = ?3
          WHERE id = ?1 AND status = 'pending'",
    )
    .bind(id)
    .bind(reason)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        bail!("suggestion #{id} is not pending");
    }
    tx.commit().await?;
    Ok(())
}

/// Replace the draft of a pending suggestion with the operator's text. The
/// text is checked like any draft; a patch keeps the base hash it was
/// filed against, so accept still refuses it if the live skill changed.
pub async fn edit(
    pool: &SqlitePool,
    workspace_root: &Path,
    roots: &LibraryRoots,
    id: i64,
    draft: &str,
) -> Result<()> {
    let row: Option<(String, String, String, Option<String>)> =
        sqlx::query_as("SELECT kind, name, status, base_hash FROM suggestions WHERE id = ?1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let Some((kind, name, status, base_hash)) = row else {
        bail!("there is no suggestion #{id}");
    };
    if status != "pending" {
        bail!("suggestion #{id} is {status}, not pending");
    }
    let kind = SuggestionKind::parse(&kind).ok_or_else(|| anyhow!("unknown kind {kind}"))?;
    // The ownership fields come from the live skill; the base hash is the
    // one the suggestion was filed against, checked again at accept.
    let prepared = prepare_draft(workspace_root, roots, kind, &name, draft, None)
        .map_err(|issues| anyhow!("the draft is not valid: {}", issues.join("; ")))?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let done = sqlx::query(
        "UPDATE suggestions SET draft = ?2, edited = 1, updated_at = ?3
          WHERE id = ?1 AND status = 'pending' AND base_hash IS ?4",
    )
    .bind(id)
    .bind(&prepared.text)
    .bind(crate::timestamp::now())
    .bind(base_hash.as_deref())
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        bail!("suggestion #{id} changed while it was being edited");
    }
    tx.commit().await?;
    Ok(())
}

/// Refuse when the personal tree, either of its two parent directories
/// under the workspace (`.nucleus`, `.nucleus/.claude`), or the skill's own
/// directory is a symlink: a write through one would land outside the
/// personal tree.
fn ensure_real_dirs(roots: &LibraryRoots, name: &str) -> Result<()> {
    let mut dirs: Vec<&Path> = vec![&roots.personal];
    if let Some(p) = roots.personal.parent() {
        dirs.push(p);
        if let Some(pp) = p.parent() {
            dirs.push(pp);
        }
    }
    let skill_dir = roots.personal.join(name);
    dirs.push(&skill_dir);
    for d in dirs {
        if std::fs::symlink_metadata(d).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("{} is a symlink; accept writes only into real directories of the personal tree", d.display());
        }
    }
    Ok(())
}

/// Write `text` to `path` through a new temporary file in the same
/// directory (created exclusively, so an existing file or symlink at that
/// name is never written through) and a rename over `path`.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    let tmp = dir.join(format!(".{}.tmp-{}", skills::SKILL_FILE, uuid::Uuid::new_v4().simple()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let written = f.write_all(text.as_bytes()).and_then(|_| f.sync_all());
    drop(f);
    if let Err(e) = written.and_then(|_| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
}

/// Accept a pending suggestion: check the draft again, write it to
/// `<personal tree>/<name>/SKILL.md`, mark it accepted, and supersede every
/// other pending suggestion for the same skill.
///
/// A `create` is refused when a skill with that name exists in any active
/// tier; a `patch` when the live SKILL.md no longer has the suggestion's
/// base hash. Accepting again after a crash between the file write and the
/// status change finds the file already equal to the draft and completes.
pub async fn accept(
    pool: &SqlitePool,
    workspace_root: &Path,
    roots: &LibraryRoots,
    id: i64,
) -> Result<AcceptOutcome> {
    // Held from the base-hash check through the rename, so no other Nucleus
    // writer changes the skill in between.
    let _library = super::library_lock(workspace_root, std::time::Duration::from_secs(10)).await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(String, String, String, String, Option<String>)> =
        sqlx::query_as("SELECT kind, name, draft, status, base_hash FROM suggestions WHERE id = ?1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((kind, name, draft, status, base_hash)) = row else {
        bail!("there is no suggestion #{id}");
    };
    if status != "pending" {
        bail!("suggestion #{id} is {status}, not pending");
    }
    let kind = SuggestionKind::parse(&kind).ok_or_else(|| anyhow!("unknown kind {kind}"))?;
    if !valid_name(&name) {
        bail!("suggestion #{id} has an invalid skill name {name:?}");
    }
    ensure_real_dirs(roots, &name)?;
    let path = roots.personal.join(&name).join(skills::SKILL_FILE);
    let already = std::fs::read(&path).ok().is_some_and(|b| b == draft.as_bytes());
    if !already {
        if kind == SuggestionKind::Patch && base_hash.is_none() {
            bail!("suggestion #{id} is a patch without a base hash");
        }
        let prepared = prepare_draft(workspace_root, roots, kind, &name, &draft, base_hash.as_deref())
            .map_err(|issues| anyhow!("suggestion #{id} cannot be accepted: {}", issues.join("; ")))?;
        match kind {
            SuggestionKind::Create => {
                std::fs::create_dir_all(&roots.personal)
                    .with_context(|| format!("creating {}", roots.personal.display()))?;
                let dir = roots.personal.join(&name);
                match std::fs::create_dir(&dir) {
                    Ok(()) => {}
                    // Left by an accept that stopped before its file was in
                    // place: a real directory without a SKILL.md (a symlink
                    // was refused above; a SKILL.md would have failed the
                    // name check in prepare_draft).
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() && !path.exists() => {}
                    Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
                }
                ensure_real_dirs(roots, &name)?;
                write_atomic(&path, &prepared.text)?;
            }
            SuggestionKind::Patch => write_atomic(&path, &prepared.text)?,
        }
    }
    let now = crate::timestamp::now();
    sqlx::query(
        "UPDATE suggestions SET status = 'accepted', updated_at = ?2, decided_at = ?2
          WHERE id = ?1 AND status = 'pending'",
    )
    .bind(id)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    let superseded: Vec<i64> = sqlx::query_scalar(
        "UPDATE suggestions SET status = 'superseded', superseded_by = ?1, updated_at = ?3, decided_at = ?3
          WHERE name = ?2 AND status = 'pending' AND id != ?1 RETURNING id",
    )
    .bind(id)
    .bind(&name)
    .bind(&now)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(AcceptOutcome { id, name, path: path.to_string_lossy().into_owned(), superseded })
}

/// Number of suggestions filed by `origin` at or after `since` (a
/// [`crate::timestamp`] value).
pub async fn count_created_since(pool: &SqlitePool, origin: SuggestionOrigin, since: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM suggestions WHERE origin = ?1 AND created_at >= ?2")
        .bind(origin.as_str())
        .bind(since)
        .fetch_one(pool)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRAFT: &str = "---\nname: ignored\ndescription: \"Deploy a service: build, push, verify\"\nflavor: recipe\ncreated_by: someone\n---\n\n# When to invoke\nWhen deploying.\n\n# Steps\n1. Build.\n\n# Failure modes\n- The push fails: retry once.\n";

    struct Ws {
        _tmp: tempfile::TempDir,
        ws: PathBuf,
        roots: LibraryRoots,
        pool: SqlitePool,
    }

    async fn ws() -> Ws {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let roots = LibraryRoots::new(&ws, Some(&tmp.path().join("home")));
        let pool = super::super::open(&ws).await.unwrap();
        Ws { _tmp: tmp, ws, roots, pool }
    }

    fn evidence(session: &str) -> NewEvidence {
        NewEvidence {
            session_id: Some(session.into()),
            source: EvidenceSource::Transcript,
            agent: None,
            note: "the operator corrected the deploy order".into(),
            quotes: vec!["USER: build before you push".into()],
        }
    }

    fn new(kind: SuggestionKind, name: &str, draft: &str) -> NewSuggestion {
        NewSuggestion {
            kind,
            name: name.into(),
            draft: draft.into(),
            rationale: "recurring deploy steps".into(),
            origin: SuggestionOrigin::Backfill,
            expected_base: None,
            overrides_rejected: None,
            why_new: None,
            evidence: evidence("sess-1"),
        }
    }

    fn put_skill(root: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(root.join(name)).unwrap();
        std::fs::write(root.join(name).join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn frontmatter_fields_are_replaced_added_and_removed() {
        let src = "---\nname: x\ntags:\n  - a\n  - b\ndescription: d\n---\nbody\n";
        let out = set_frontmatter(src, &[("name", Some("y")), ("tags", None), ("flavor", Some("learned"))]).unwrap();
        assert_eq!(out, "---\nname: y\ndescription: d\nflavor: learned\n---\nbody\n");
        assert!(set_frontmatter("no block", &[]).is_err());
    }

    #[test]
    fn names_must_be_class_level_kebab_case() {
        for ok in ["deploy", "rust-service-deploy", "a1-b2"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "Deploy", "a--b", "-a", "a_b", "a.b", "../x", &"a".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn create_normalizes_ownership_and_refuses_bad_drafts() {
        let w = ws().await;
        let id = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Create, "service-deploy", DRAFT)).await.unwrap();
        let s = get(&w.pool, id).await.unwrap().unwrap();
        assert!(s.draft.contains("name: service-deploy\n"));
        assert!(s.draft.contains("flavor: learned\n") && s.draft.contains("created_by: agent\n"));
        assert!(!s.draft.contains("someone") && !s.draft.contains("recipe"));
        assert_eq!(s.evidence.len(), 1);
        assert_eq!(s.evidence[0].quotes, vec!["USER: build before you push".to_string()]);

        // Missing failure modes.
        let no_fail = DRAFT.replace("# Failure modes\n- The push fails: retry once.\n", "");
        let err = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Create, "other", &no_fail)).await.unwrap_err();
        assert!(format!("{err:#}").contains("failure modes"), "{err:#}");
        // Unquoted colon in the description: strict YAML fails.
        let colon = DRAFT.replace("\"Deploy a service: build, push, verify\"", "Deploy a service: build");
        let err = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Create, "other", &colon)).await.unwrap_err();
        assert!(format!("{err:#}").contains("not valid YAML"), "{err:#}");
        // A credential-shaped token.
        let leak = DRAFT.replace("1. Build.", &format!("1. Use {}{}.", "sk-ant-", "a".repeat(30)));
        let err = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Create, "other", &leak)).await.unwrap_err();
        assert!(format!("{err:#}").contains("credential"), "{err:#}");
        // A second pending suggestion for the same name.
        let err = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Create, "service-deploy", DRAFT)).await.unwrap_err();
        assert!(format!("{err:#}").contains("already pending"), "{err:#}");
        // A name taken by a committed skill.
        put_skill(&w.roots.repo, "committed", DRAFT);
        let err = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Create, "committed", DRAFT)).await.unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    }

    #[tokio::test]
    async fn accept_writes_only_the_personal_tree_and_supersedes_siblings() {
        let w = ws().await;
        let id = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Create, "service-deploy", DRAFT)).await.unwrap();
        let out = accept(&w.pool, &w.ws, &w.roots, id).await.unwrap();
        let written = std::fs::read_to_string(w.roots.personal.join("service-deploy/SKILL.md")).unwrap();
        assert_eq!(written, get(&w.pool, id).await.unwrap().unwrap().draft);
        assert!(out.path.ends_with(".nucleus/.claude/skills/service-deploy/SKILL.md"));
        assert!(!w.roots.repo.exists(), "the committed tree is never written");
        assert_eq!(get(&w.pool, id).await.unwrap().unwrap().status, SuggestionStatus::Accepted);
        assert!(accept(&w.pool, &w.ws, &w.roots, id).await.is_err(), "not pending any more");

        // A patch of the accepted skill is bound to the file just written.
        let p = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Patch, "service-deploy", &DRAFT.replace("Build.", "Build and test.")))
            .await
            .unwrap();
        let s = get(&w.pool, p).await.unwrap().unwrap();
        assert_eq!(s.base_hash.as_deref(), Some(content_hash(written.as_bytes()).as_str()));
        assert!(s.draft.contains("created_by: agent"), "the live skill's owner is kept");
        let out = accept(&w.pool, &w.ws, &w.roots, p).await.unwrap();
        assert!(out.superseded.is_empty());
        assert!(std::fs::read_to_string(&out.path).unwrap().contains("Build and test."));
    }

    #[tokio::test]
    async fn patch_accept_is_bound_to_the_base_hash() {
        let w = ws().await;
        let hand = "---\nname: hand\ndescription: d\nflavor: recipe\npinned: true\n---\n\n# When to invoke\nx\n# Steps\n1\n# Failure modes\n- z\n";
        put_skill(&w.roots.personal, "hand", hand);
        let p = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Patch, "hand", DRAFT)).await.unwrap();
        let s = get(&w.pool, p).await.unwrap().unwrap();
        assert!(!s.draft.contains("created_by"), "a hand-written skill stays hand-written");
        assert!(s.draft.contains("pinned: true") && s.draft.contains("flavor: recipe"));

        // The operator changes the skill after the suggestion was filed.
        std::fs::write(w.roots.personal.join("hand/SKILL.md"), format!("{hand}\nmore\n")).unwrap();
        let err = accept(&w.pool, &w.ws, &w.roots, p).await.unwrap_err();
        assert!(format!("{err:#}").contains("changed after"), "{err:#}");
        assert!(std::fs::read_to_string(w.roots.personal.join("hand/SKILL.md")).unwrap().ends_with("more\n"));
        assert_eq!(get(&w.pool, p).await.unwrap().unwrap().status, SuggestionStatus::Pending);

        // A stale expected base is refused at filing time too.
        let mut stale = new(SuggestionKind::Create, "x", DRAFT);
        stale.kind = SuggestionKind::Patch;
        stale.name = "hand".into();
        stale.expected_base = Some("0".repeat(64));
        reject(&w.pool, p, None).await.unwrap();
        stale.overrides_rejected = Some(p);
        stale.why_new = Some("new evidence".into());
        assert!(create(&w.pool, &w.ws, &w.roots, &stale).await.is_err());
        // Patching a skill that is not in the personal tree is refused.
        assert!(create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Patch, "missing", DRAFT)).await.is_err());
    }

    #[tokio::test]
    async fn reject_edit_revise_and_the_rejected_before_rule() {
        let w = ws().await;
        let a = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Create, "service-deploy", DRAFT)).await.unwrap();
        add_evidence(&w.pool, a, &evidence("sess-2")).await.unwrap();

        // Revise: new id, evidence carried over, old superseded.
        let b = revise(&w.pool, &w.ws, &w.roots, a, &new(SuggestionKind::Create, "service-deploy", &DRAFT.replace("Build.", "Build twice.")))
            .await
            .unwrap();
        let old = get(&w.pool, a).await.unwrap().unwrap();
        assert_eq!((old.status, old.superseded_by), (SuggestionStatus::Superseded, Some(b)));
        let newer = get(&w.pool, b).await.unwrap().unwrap();
        assert_eq!(newer.evidence.len(), 3);
        assert!(add_evidence(&w.pool, a, &evidence("sess-3")).await.is_err());

        // Edit: the operator's draft is checked, and a model revision no
        // longer replaces it.
        assert!(edit(&w.pool, &w.ws, &w.roots, b, "not a skill").await.is_err());
        edit(&w.pool, &w.ws, &w.roots, b, &DRAFT.replace("Build.", "Build by hand.")).await.unwrap();
        let edited = get(&w.pool, b).await.unwrap().unwrap();
        assert!(edited.edited && edited.draft.contains("Build by hand."));
        assert!(revise(&w.pool, &w.ws, &w.roots, b, &new(SuggestionKind::Create, "service-deploy", DRAFT)).await.is_err());

        // Reject, then suggesting the same skill again needs a reason.
        reject(&w.pool, b, Some("too narrow")).await.unwrap();
        assert!(reject(&w.pool, b, None).await.is_err());
        let err = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Create, "service-deploy", DRAFT)).await.unwrap_err();
        assert!(format!("{err:#}").contains("too narrow"), "{err:#}");
        let mut again = new(SuggestionKind::Create, "service-deploy", DRAFT);
        again.overrides_rejected = Some(b);
        assert!(create(&w.pool, &w.ws, &w.roots, &again).await.is_err(), "why_new is required");
        again.why_new = Some("three more sessions repeat the correction".into());
        let c = create(&w.pool, &w.ws, &w.roots, &again).await.unwrap();

        let pending = list(&w.pool, &[SuggestionStatus::Pending]).await.unwrap();
        assert_eq!(pending.iter().map(|s| s.id).collect::<Vec<_>>(), vec![c]);
        assert_eq!(list(&w.pool, &[]).await.unwrap().len(), 3);
        let since = "2000-01-01T00:00:00.000Z";
        assert_eq!(count_created_since(&w.pool, SuggestionOrigin::Backfill, since).await.unwrap(), 3);
        assert_eq!(count_created_since(&w.pool, SuggestionOrigin::Live, since).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn accept_refuses_symlinks_and_recovers_an_interrupted_create() {
        let w = ws().await;
        let a = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Create, "service-deploy", DRAFT)).await.unwrap();
        // A crash after the directory was made: retrying completes.
        std::fs::create_dir_all(w.roots.personal.join("service-deploy")).unwrap();
        accept(&w.pool, &w.ws, &w.roots, a).await.unwrap();
        // A skill directory that is a symlink to somewhere else is refused.
        let outside = w.ws.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let b = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Create, "linked", DRAFT)).await.unwrap();
        std::os::unix::fs::symlink(&outside, w.roots.personal.join("linked")).unwrap();
        let err = accept(&w.pool, &w.ws, &w.roots, b).await.unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none(), "nothing written outside");
    }

    #[tokio::test]
    async fn accept_completes_when_the_file_was_already_written() {
        let w = ws().await;
        let id = create(&w.pool, &w.ws, &w.roots, &new(SuggestionKind::Create, "service-deploy", DRAFT)).await.unwrap();
        let draft = get(&w.pool, id).await.unwrap().unwrap().draft;
        put_skill(&w.roots.personal, "service-deploy", &draft);
        accept(&w.pool, &w.ws, &w.roots, id).await.unwrap();
        assert_eq!(get(&w.pool, id).await.unwrap().unwrap().status, SuggestionStatus::Accepted);
    }
}
