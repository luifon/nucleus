//! Skills surface — walk the operator-personal and repo-committed
//! skill directories, parse SKILL.md frontmatter, expose as JSON.
//!
//! Per ADR-008 storage convention:
//!   - `~/.claude/skills/<name>/SKILL.md`   — operator-personal (not committed)
//!   - `<repo>/.claude/skills/<name>/SKILL.md` — committed (ships with repo)
//!
//! Frontmatter shape (ADR-008 extends Claude Code's contract):
//!   name?, description, flavor?, mcp_needed?, last_used?,
//!   last_failure?, failure_count_30d?, notify_on_failure?, tags?,
//!   trigger?, arguments?, allowed-tools?
//!
//! The display layer falls back to the directory name when `name` is
//! omitted (the Claude Code convention). Missing telemetry fields
//! (last_used / failure_count_30d / notify_on_failure) render as `—`
//! in the UI — ADR-008 defines them but no population code exists
//! yet (see ADR-015 §"Future work — Skill telemetry population").

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct SkillsState {
    pub operator_root: PathBuf,
    pub repo_root: PathBuf,
}

pub fn router(state: Arc<SkillsState>) -> Router {
    Router::new()
        .route("/list", get(list_skills))
        .route("/body", get(get_body))
        .with_state(state)
}

#[derive(Serialize)]
struct Skill {
    /// Display name. Frontmatter `name`, falling back to directory name.
    name: String,
    /// One-line summary from frontmatter `description`. Required by
    /// Claude Code; should always be present.
    description: String,
    /// Source tier — "personal" (operator-only, ~/.claude/skills/) or
    /// "repo" (committed under .claude/skills/).
    tier: &'static str,
    /// Absolute path to the SKILL.md file. Used by the frontend's
    /// "open in $EDITOR" hint.
    path: String,
    /// ADR-008 fields. Optional because operator-authored skills may
    /// omit them; the dashboard renders `—` for missing values.
    flavor: Option<String>,
    mcp_needed: Option<Vec<String>>,
    last_used: Option<String>,
    last_failure: Option<String>,
    failure_count_30d: Option<i64>,
    notify_on_failure: Option<Vec<String>>,
    tags: Option<Vec<String>>,
    /// Trigger hint (e.g. "manual", "reminder", "manual | reminder").
    /// Free-form string from frontmatter `trigger`.
    trigger: Option<String>,
}

#[derive(Deserialize, Default)]
struct ParsedFrontmatter {
    name: Option<String>,
    description: Option<String>,
    flavor: Option<String>,
    #[serde(default, deserialize_with = "string_or_vec")]
    mcp_needed: Option<Vec<String>>,
    last_used: Option<String>,
    last_failure: Option<String>,
    failure_count_30d: Option<i64>,
    #[serde(default, deserialize_with = "string_or_vec")]
    notify_on_failure: Option<Vec<String>>,
    #[serde(default, deserialize_with = "string_or_vec")]
    tags: Option<Vec<String>>,
    trigger: Option<String>,
}

/// Frontmatter authors flip between `key: value` and `key: [a, b]`
/// even within the same field — accept either shape. Empty / null
/// reads as None.
fn string_or_vec<'de, D>(de: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = serde_yaml::Value::deserialize(de)?;
    match value {
        serde_yaml::Value::Null => Ok(None),
        serde_yaml::Value::String(s) if s.is_empty() => Ok(None),
        serde_yaml::Value::String(s) => Ok(Some(vec![s])),
        serde_yaml::Value::Sequence(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    serde_yaml::Value::String(s) => out.push(s),
                    other => out.push(serde_yaml::to_string(&other).unwrap_or_default().trim().to_string()),
                }
            }
            Ok(Some(out))
        }
        other => Err(D::Error::custom(format!(
            "expected string or sequence, got {:?}",
            other
        ))),
    }
}

async fn list_skills(State(s): State<Arc<SkillsState>>) -> Result<Json<Vec<Skill>>, SkillsError> {
    let mut out = Vec::new();
    out.extend(walk(&s.operator_root, "personal").await?);
    out.extend(walk(&s.repo_root, "repo").await?);
    // Stable order: tier first, then name.
    out.sort_by(|a, b| a.tier.cmp(b.tier).then_with(|| a.name.cmp(&b.name)));
    Ok(Json(out))
}

async fn walk(root: &Path, tier: &'static str) -> Result<Vec<Skill>, SkillsError> {
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(e) => e,
        // Operator hasn't created the personal tree yet (or the repo
        // tier is empty) — that's fine, return an empty list.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(SkillsError::Io(format!("reading {}: {}", root.display(), e))),
    };
    while let Some(dirent) = entries.next_entry().await.map_err(|e| SkillsError::Io(e.to_string()))? {
        let path = dirent.path();
        let file_type = match dirent.file_type().await {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !file_type.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !tokio::fs::try_exists(&skill_md).await.unwrap_or(false) {
            continue;
        }
        let dir_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let content = match tokio::fs::read_to_string(&skill_md).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("skills: failed to read {}: {}", skill_md.display(), e);
                continue;
            }
        };
        let fm = parse_frontmatter(&content, &skill_md).unwrap_or_default();
        out.push(Skill {
            name: fm.name.unwrap_or(dir_name),
            description: fm.description.unwrap_or_default(),
            tier,
            path: skill_md.to_string_lossy().into_owned(),
            flavor: fm.flavor,
            mcp_needed: fm.mcp_needed,
            last_used: fm.last_used,
            last_failure: fm.last_failure,
            failure_count_30d: fm.failure_count_30d,
            notify_on_failure: fm.notify_on_failure,
            tags: fm.tags,
            trigger: fm.trigger,
        });
    }
    Ok(out)
}

/// Split out the leading `---`-delimited YAML block and deserialize.
/// Returns None if the file has no frontmatter (rare for skills but
/// not invalid).
///
/// **Two-pass strategy.** Strict YAML first — when the frontmatter is
/// clean we get the full ADR-008 field set. On failure we fall back
/// to a forgiving line-by-line `key: value` extractor that recovers
/// at minimum `name` + `description`. This keeps the surface useful
/// when a skill author writes a description like
/// `natural-language colons` (the `: ` inside the value trips strict
/// YAML), without forcing every author to remember YAML quoting rules.
///
/// Parse failures are logged at WARN with the file path so the
/// operator notices and can tighten the source.
fn parse_frontmatter(content: &str, path: &Path) -> Option<ParsedFrontmatter> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    let yaml = &rest[..end];
    match serde_yaml::from_str::<ParsedFrontmatter>(yaml) {
        Ok(fm) => Some(fm),
        Err(e) => {
            tracing::warn!(
                "skills: strict YAML failed for {} ({}); falling back to lenient line parse",
                path.display(),
                e
            );
            Some(parse_frontmatter_lenient(yaml))
        }
    }
}

/// Best-effort line-by-line parser for cases where strict YAML
/// rejects the frontmatter. Only handles the simple `key: value`
/// shape; anything that looks like a list, multi-line value, or
/// continuation is dropped. The recovered fields are always a
/// subset of what strict YAML would have produced.
fn parse_frontmatter_lenient(yaml: &str) -> ParsedFrontmatter {
    let mut fm = ParsedFrontmatter::default();
    for line in yaml.lines() {
        // Skip indented continuation / list items — only consider
        // top-level key:value lines.
        if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-') {
            continue;
        }
        // Find the first `: ` (with space). Without the space it's
        // a colon-in-value and not a separator.
        let Some(idx) = line.find(": ") else { continue };
        let key = line[..idx].trim();
        let raw_value = line[idx + 2..].trim();
        // Strip outer quotes (single or double) without trying to
        // unescape — display layer doesn't care about embedded
        // quotes for the fields we recover here.
        let value = strip_quotes(raw_value).to_string();
        match key {
            "name"        => fm.name = Some(value),
            "description" => fm.description = Some(value),
            "flavor"      => fm.flavor = Some(value),
            "trigger"     => fm.trigger = Some(value),
            "last_used"   => fm.last_used = Some(value),
            "last_failure"=> fm.last_failure = Some(value),
            _ => {}
        }
    }
    fm
}

fn strip_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[derive(Deserialize)]
struct BodyQ {
    path: String,
}

/// Returns the raw SKILL.md content (frontmatter + body). The frontend
/// renders it as markdown. Path-traversal guarded by requiring the
/// path to be canonicalized inside one of the two known roots.
async fn get_body(
    State(s): State<Arc<SkillsState>>,
    Query(q): Query<BodyQ>,
) -> Result<String, SkillsError> {
    let requested = PathBuf::from(&q.path);
    let canonical = tokio::fs::canonicalize(&requested)
        .await
        .map_err(|e| SkillsError::Io(format!("canonicalizing {}: {}", q.path, e)))?;
    let in_operator = canonical_under(&canonical, &s.operator_root).await;
    let in_repo = canonical_under(&canonical, &s.repo_root).await;
    if !in_operator && !in_repo {
        return Err(SkillsError::OutsideRoots);
    }
    if !canonical.file_name().and_then(|n| n.to_str()).is_some_and(|n| n == "SKILL.md") {
        return Err(SkillsError::OutsideRoots);
    }
    let body = tokio::fs::read_to_string(&canonical)
        .await
        .map_err(|e| SkillsError::Io(format!("reading {}: {}", canonical.display(), e)))?;
    Ok(body)
}

async fn canonical_under(p: &Path, root: &Path) -> bool {
    match tokio::fs::canonicalize(root).await {
        Ok(canon_root) => p.starts_with(&canon_root),
        Err(_) => false,
    }
}

#[derive(Debug)]
pub enum SkillsError {
    Io(String),
    OutsideRoots,
}

impl IntoResponse for SkillsError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Io(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
            Self::OutsideRoots => (
                StatusCode::FORBIDDEN,
                "path is not inside either skills tree".to_string(),
            ),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
