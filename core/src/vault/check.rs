//! Vault check (ADR-035): a deterministic structural report over the vault,
//! with a small set of safe fixes.
//!
//! No Claude session is involved. The report covers:
//!
//! - **duplicates** — the same file name in several folders; near-identical
//!   titles; a series of three or more dated notes on one theme in one
//!   folder (notes that should have been appends, CLAUDE.md Rule 9.4); and
//!   notes whose bodies are near-identical (MinHash over 5-word shingles).
//! - **broken links** — `[[targets]]` that resolve to no file. Links in
//!   code are ignored; `|alias`, table-escaped `\|alias`, `#heading` and
//!   `#^block` suffixes are handled. Resolution follows Obsidian: case-
//!   insensitive, by file name anywhere in the vault or by path suffix.
//! - **orphans** — notes no other note links to (exempt globs apply).
//! - **stale inbox** — `0-Inbox` notes older than `inbox_max_age_days`.
//! - **frontmatter** — missing block, invalid YAML, or a missing required
//!   key (Rule 9.7).
//! - **unknown source** — `source:` values outside the configured
//!   vocabulary, grouped by value.
//! - **empty files** — 0-byte files, empty notes, canvases with no nodes.
//!
//! Fixes (only with `apply`, each recorded on its finding):
//!
//! - delete an empty file whose name starts with `Untitled` (Obsidian's
//!   default name for a new note, canvas or base) and that nothing links to;
//! - add `created: <date>` from the file's birth time to a note that lacks
//!   it (valid or missing frontmatter only; the file's mtime is restored).
//!
//! Notes are never moved or renamed. Excluded paths and credential notes
//! are never read for findings, never listed, and never modified.

use super::exclude::{Exclusions, GlobSet};
use super::note::{self, Frontmatter, ParsedNote};
use super::scan::{self, VaultFile};
use crate::config::VaultCheckConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use unicode_normalization::UnicodeNormalization;

pub const DB_PATH: &str = "memory/vault_check.db";

/// How many runs keep their full finding list. Older runs keep only their
/// counts, which is all the trend needs.
const KEEP_FINDINGS_FOR_RUNS: i64 = 26;

pub const KIND_DUPLICATE_NAME: &str = "duplicate_name";
pub const KIND_SIMILAR_TITLE: &str = "similar_title";
pub const KIND_DATED_SERIES: &str = "dated_series";
pub const KIND_DUPLICATE_CONTENT: &str = "duplicate_content";
pub const KIND_BROKEN_LINK: &str = "broken_link";
pub const KIND_ORPHAN: &str = "orphan";
pub const KIND_STALE_INBOX: &str = "stale_inbox";
pub const KIND_FRONTMATTER: &str = "frontmatter";
pub const KIND_UNKNOWN_SOURCE: &str = "unknown_source";
pub const KIND_EMPTY_FILE: &str = "empty_file";

// ─── wire types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS, PartialEq)]
#[ts(export)]
pub struct Finding {
    /// One of the `KIND_*` values.
    pub kind: String,
    /// The note the finding is about; `None` for group findings.
    pub path: Option<String>,
    pub detail: String,
    /// Other paths involved: the members of a group finding, the notes of
    /// an unknown `source:` value, or, for a broken link, a folder whose
    /// name equals the link target.
    pub related: Vec<String>,
    pub fixed: bool,
    /// What `--apply` did, when it did something.
    pub fix_action: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ts_rs::TS, PartialEq)]
#[ts(export)]
pub struct CheckCounts {
    /// Duplicate groups (same name, similar title, dated series, same content).
    #[ts(type = "number")]
    pub duplicates: i64,
    #[ts(type = "number")]
    pub broken_links: i64,
    #[ts(type = "number")]
    pub orphans: i64,
    #[ts(type = "number")]
    pub stale_inbox: i64,
    #[ts(type = "number")]
    pub missing_frontmatter: i64,
    /// Notes whose `source:` is outside the vocabulary.
    #[ts(type = "number")]
    pub unknown_source: i64,
    #[ts(type = "number")]
    pub empty_files: i64,
    /// Fixes applied in this run.
    #[ts(type = "number")]
    pub fixed: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct CheckReport {
    #[ts(type = "number | null")]
    pub run_id: Option<i64>,
    pub started_at: String,
    pub finished_at: String,
    /// `manual` or `scheduled`.
    pub trigger: String,
    pub applied: bool,
    #[ts(type = "number")]
    pub notes_scanned: i64,
    /// Files skipped by the exclusion rules (paths and credential notes).
    #[ts(type = "number")]
    pub files_excluded: i64,
    #[ts(type = "number")]
    pub duration_ms: i64,
    pub counts: CheckCounts,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct CheckRunSummary {
    #[ts(type = "number")]
    pub id: i64,
    pub started_at: String,
    pub trigger: String,
    pub applied: bool,
    #[ts(type = "number")]
    pub notes_scanned: i64,
    #[ts(type = "number")]
    pub duration_ms: i64,
    pub counts: CheckCounts,
}

// ─── options ────────────────────────────────────────────────────────────────

pub struct CheckOptions {
    pub apply: bool,
    pub trigger: String,
    pub inbox_max_age_days: i64,
    pub required_frontmatter: Vec<String>,
    pub source_vocabulary: Vec<String>,
    pub orphan_exempt: GlobSet,
    pub frontmatter_exempt: GlobSet,
    /// "Today" for age calculations (local date).
    pub today: chrono::NaiveDate,
}

impl CheckOptions {
    pub fn from_config(cfg: &VaultCheckConfig, apply: bool, trigger: &str) -> Result<Self> {
        Ok(Self {
            apply,
            trigger: trigger.to_string(),
            inbox_max_age_days: cfg.inbox_max_age_days,
            required_frontmatter: cfg.required_frontmatter.clone(),
            source_vocabulary: cfg.source_vocabulary.clone(),
            orphan_exempt: GlobSet::new(&cfg.orphan_exempt).context("[vault_check] orphan_exempt")?,
            frontmatter_exempt: GlobSet::new(&cfg.frontmatter_exempt)
                .context("[vault_check] frontmatter_exempt")?,
            today: chrono::Local::now().date_naive(),
        })
    }
}

// ─── analysis ───────────────────────────────────────────────────────────────

struct Note {
    file: VaultFile,
    parsed: ParsedNote,
    text: String,
}

/// Run the check over `vault`. Fixes are applied only when `opts.apply`.
pub fn run(vault: &Path, ex: &Exclusions, opts: &CheckOptions) -> Result<CheckReport> {
    let started = std::time::Instant::now();
    let started_at = crate::timestamp::now();
    let walk = scan::walk(vault, ex)?;

    let mut notes: Vec<Note> = Vec::new();
    let mut others: Vec<VaultFile> = Vec::new();
    // Every file a link may point at, including excluded ones: a link to an
    // excluded note is not broken. Paths only; excluded files are not read.
    let mut targets: Vec<String> = walk.excluded.clone();
    let mut files_excluded = walk.excluded.len() as i64;
    for f in walk.files {
        targets.push(f.rel.clone());
        if !f.is_markdown() {
            others.push(f);
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&f.abs) else { continue };
        if ex.content_excluded(&text) {
            files_excluded += 1;
            continue;
        }
        let parsed = note::parse(&f.rel, &text);
        notes.push(Note { file: f, parsed, text });
    }
    let resolver = Resolver::new(&targets);

    let mut findings: Vec<Finding> = Vec::new();
    let mut counts = CheckCounts::default();

    // Links → broken findings + inbound graph.
    let mut inbound: HashMap<String, usize> = HashMap::new();
    for n in &notes {
        let mut seen: HashSet<String> = HashSet::new();
        for l in &n.parsed.links {
            match resolver.resolve(&l.target, &n.file.rel) {
                Some(t) => {
                    if t != n.file.rel {
                        *inbound.entry(t).or_default() += 1;
                    }
                }
                None => {
                    if seen.insert(l.target.to_lowercase()) {
                        counts.broken_links += 1;
                        // `[[Folder]]` does not open a folder in Obsidian;
                        // name the folder so the fix (link its README or
                        // index) is obvious.
                        let folder = resolver.folder(&l.target);
                        let hint = if folder.is_some() { "; a folder with this name exists" } else { "" };
                        findings.push(Finding {
                            kind: KIND_BROKEN_LINK.into(),
                            path: Some(n.file.rel.clone()),
                            detail: format!("[[{}]] (line {}) resolves to no file{hint}", l.target, l.line),
                            related: folder.into_iter().collect(),
                            fixed: false,
                            fix_action: None,
                        });
                    }
                }
            }
        }
        for md in &n.parsed.md_links {
            if let Some(t) = resolver.resolve_relative(md, &n.file.rel) {
                if t != n.file.rel {
                    *inbound.entry(t).or_default() += 1;
                }
            }
        }
    }

    // Orphans.
    for n in &notes {
        if inbound.get(&n.file.rel).copied().unwrap_or(0) == 0 && !opts.orphan_exempt.matches(&n.file.rel) {
            counts.orphans += 1;
            findings.push(simple(KIND_ORPHAN, &n.file.rel, "no other note links here".into()));
        }
    }

    // Stale inbox.
    for n in notes.iter().filter(|n| n.file.rel.starts_with("0-Inbox/")) {
        if n.file.file_name().eq_ignore_ascii_case("README.md") {
            continue;
        }
        let date = n
            .parsed
            .created()
            .and_then(|c| parse_date(&c))
            .or_else(|| n.file.birthtime.and_then(unix_to_local_date))
            .or_else(|| unix_to_local_date(n.file.mtime));
        if let Some(d) = date {
            let age = (opts.today - d).num_days();
            if age > opts.inbox_max_age_days {
                counts.stale_inbox += 1;
                findings.push(simple(KIND_STALE_INBOX, &n.file.rel, format!("in 0-Inbox for {age} days (since {d})")));
            }
        }
    }

    // Frontmatter + source vocabulary.
    let mut by_source: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut created_fixes: Vec<(usize, usize)> = Vec::new(); // (note idx, finding idx)
    for (i, n) in notes.iter().enumerate() {
        if opts.frontmatter_exempt.matches(&n.file.rel) {
            continue;
        }
        let fm = &n.parsed.frontmatter;
        let detail = match fm {
            Frontmatter::Missing => Some("no frontmatter block".to_string()),
            Frontmatter::Invalid => Some("frontmatter is not a valid YAML mapping".to_string()),
            Frontmatter::Valid(_) => {
                let missing: Vec<&str> = opts
                    .required_frontmatter
                    .iter()
                    .filter(|k| !fm.has_key(k))
                    .map(|s| s.as_str())
                    .collect();
                (!missing.is_empty()).then(|| format!("missing: {}", missing.join(", ")))
            }
        };
        if let Some(detail) = detail {
            counts.missing_frontmatter += 1;
            let needs_created = opts.required_frontmatter.iter().any(|k| k == "created")
                && !matches!(fm, Frontmatter::Invalid)
                && !fm.has_key("created");
            findings.push(simple(KIND_FRONTMATTER, &n.file.rel, detail));
            // Empty notes are reported as empty, not given frontmatter.
            if needs_created && n.file.birthtime.is_some() && !note_is_empty(&n.text) {
                created_fixes.push((i, findings.len() - 1));
            }
        }
        if let Some(src) = n.parsed.source() {
            if !source_known(&src, &opts.source_vocabulary) {
                by_source.entry(src).or_default().push(n.file.rel.clone());
            }
        }
    }
    for (value, paths) in by_source {
        counts.unknown_source += paths.len() as i64;
        findings.push(Finding {
            kind: KIND_UNKNOWN_SOURCE.into(),
            path: None,
            detail: format!("source: {value} ({} notes) is not in [vault_check] source_vocabulary", paths.len()),
            related: paths,
            fixed: false,
            fix_action: None,
        });
    }

    // Empty files.
    let mut empty_deletes: Vec<(std::path::PathBuf, usize, i64)> = Vec::new();
    let empties = notes
        .iter()
        .filter(|n| n.file.size == 0 || note_is_empty(&n.text))
        .map(|n| &n.file)
        .chain(others.iter().filter(|f| f.size == 0 || other_is_empty(f)));
    for f in empties {
        counts.empty_files += 1;
        findings.push(simple(KIND_EMPTY_FILE, &f.rel, if f.size == 0 { "0 bytes".into() } else { "no content".into() }));
        let untitled = f.file_name().to_lowercase().starts_with("untitled");
        let linked = inbound.get(&f.rel).copied().unwrap_or(0) > 0;
        if untitled && !linked {
            empty_deletes.push((f.abs.clone(), findings.len() - 1, f.mtime));
        }
    }

    // Duplicates.
    for g in duplicate_groups(&notes) {
        counts.duplicates += 1;
        findings.push(g);
    }

    // Fixes.
    if opts.apply {
        for (abs, fi, mtime) in empty_deletes {
            match delete_if_unchanged(&abs, mtime) {
                Ok(true) => {
                    findings[fi].fixed = true;
                    findings[fi].fix_action = Some("deleted".into());
                    counts.fixed += 1;
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(path = %abs.display(), err = %e, "vault-check: delete failed"),
            }
        }
        for (ni, fi) in created_fixes {
            let n = &notes[ni];
            let Some(date) = n.file.birthtime.and_then(unix_to_local_date) else { continue };
            match add_created(&n.file, &n.text, &n.parsed.frontmatter, date) {
                Ok(true) => {
                    let still_missing: Vec<&str> = opts
                        .required_frontmatter
                        .iter()
                        .filter(|k| k.as_str() != "created" && !n.parsed.frontmatter.has_key(k))
                        .map(|s| s.as_str())
                        .collect();
                    findings[fi].fixed = still_missing.is_empty();
                    findings[fi].fix_action = Some(format!("added created: {date}"));
                    counts.fixed += 1;
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(path = %n.file.rel, err = %e, "vault-check: created fix failed"),
            }
        }
    }

    Ok(CheckReport {
        run_id: None,
        started_at,
        finished_at: crate::timestamp::now(),
        trigger: opts.trigger.clone(),
        applied: opts.apply,
        notes_scanned: notes.len() as i64,
        files_excluded,
        duration_ms: started.elapsed().as_millis() as i64,
        counts,
        findings,
    })
}

fn simple(kind: &str, path: &str, detail: String) -> Finding {
    Finding {
        kind: kind.into(),
        path: Some(path.to_string()),
        detail,
        related: vec![],
        fixed: false,
        fix_action: None,
    }
}

fn parse_date(s: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

fn unix_to_local_date(t: i64) -> Option<chrono::NaiveDate> {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(t, 0).single().map(|d| d.date_naive())
}

/// Each `+`- or `,`-separated part must match a vocabulary entry; an entry
/// ending in `*` matches by prefix.
pub fn source_known(value: &str, vocab: &[String]) -> bool {
    value
        .split(['+', ','])
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .all(|part| {
            vocab.iter().any(|v| {
                let v = v.trim().to_lowercase();
                match v.strip_suffix('*') {
                    Some(prefix) => part.starts_with(prefix),
                    None => part == v,
                }
            })
        })
}

/// A note with no text, or only a frontmatter block.
fn note_is_empty(text: &str) -> bool {
    let (_, body, _) = note::split_frontmatter(text);
    body.trim().is_empty()
}

/// A canvas with no nodes (`{}` or `{"nodes":[]}`); other non-markdown
/// files count as empty only at 0 bytes.
fn other_is_empty(f: &VaultFile) -> bool {
    if !f.rel.to_lowercase().ends_with(".canvas") || f.size > 4096 {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(&f.abs) else { return false };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => v
            .get("nodes")
            .and_then(|n| n.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true),
        Err(_) => text.trim().is_empty(),
    }
}

fn delete_if_unchanged(abs: &Path, mtime: i64) -> Result<bool> {
    let meta = std::fs::symlink_metadata(abs)?;
    let now_mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if !meta.is_file() || now_mtime != mtime {
        return Ok(false);
    }
    std::fs::remove_file(abs)?;
    tracing::info!(path = %abs.display(), "vault-check: deleted empty untitled file");
    Ok(true)
}

/// Insert `created: <date>` into the note, in place (same inode, so the
/// birth time survives), then restore the original mtime. Skips the note if
/// it changed since it was read.
fn add_created(f: &VaultFile, text: &str, fm: &Frontmatter, date: chrono::NaiveDate) -> Result<bool> {
    let current = std::fs::read_to_string(&f.abs)?;
    if current != text || text.starts_with('\u{feff}') {
        return Ok(false);
    }
    let new_text = match fm {
        Frontmatter::Valid(_) => {
            let Some(nl) = text.find('\n') else { return Ok(false) };
            format!("{}created: {date}\n{}", &text[..nl + 1], &text[nl + 1..])
        }
        Frontmatter::Missing => format!("---\ncreated: {date}\n---\n{text}"),
        Frontmatter::Invalid => return Ok(false),
    };
    let meta = std::fs::metadata(&f.abs)?;
    let mtime = meta.modified()?;
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().write(true).truncate(true).open(&f.abs)?;
        file.write_all(new_text.as_bytes())?;
        file.set_modified(mtime)?;
    }
    tracing::info!(path = %f.rel, %date, "vault-check: added created");
    Ok(true)
}

// ─── link resolution ────────────────────────────────────────────────────────

/// Obsidian-style link resolution over a fixed file list.
pub struct Resolver {
    /// lowercase file name (with extension) → paths
    by_name: HashMap<String, Vec<String>>,
    /// lowercase path → original path
    by_path: HashMap<String, String>,
    /// lowercase folder name → shortest folder path with that name
    folders: HashMap<String, String>,
}

impl Resolver {
    pub fn new(paths: &[String]) -> Self {
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        let mut by_path = HashMap::new();
        let mut folders: HashMap<String, String> = HashMap::new();
        for p in paths {
            let lower = p.to_lowercase();
            let name = lower.rsplit('/').next().unwrap_or(&lower).to_string();
            by_name.entry(name).or_default().push(p.clone());
            by_path.insert(lower, p.clone());
            let mut dir = p.as_str();
            while let Some((parent, _)) = dir.rsplit_once('/') {
                let key = parent.rsplit('/').next().unwrap_or(parent).to_lowercase();
                let slot = folders.entry(key).or_insert_with(|| parent.to_string());
                if parent.len() < slot.len() {
                    *slot = parent.to_string();
                }
                dir = parent;
            }
        }
        for v in by_name.values_mut() {
            // Obsidian prefers the shortest path when a name is ambiguous.
            v.sort_by_key(|p| (p.matches('/').count(), p.clone()));
        }
        Self { by_name, by_path, folders }
    }

    /// A folder whose name equals the link target, for the broken-link hint.
    pub fn folder(&self, target: &str) -> Option<String> {
        let t: String = target.nfc().collect::<String>().to_lowercase();
        let t = t.trim().trim_end_matches('/');
        let key = t.rsplit('/').next().unwrap_or(t);
        self.folders.get(key).cloned()
    }

    /// Resolve a wiki-link target (`Note`, `folder/Note`, `file.png`).
    pub fn resolve(&self, target: &str, from: &str) -> Option<String> {
        let t: String = target.nfc().collect::<String>().to_lowercase();
        let t = t.trim().trim_start_matches("./").trim_start_matches('/');
        if t.is_empty() {
            return None;
        }
        if t.starts_with("../") {
            return self.resolve_relative(t, from).or_else(|| self.resolve_relative(&format!("{t}.md"), from));
        }
        let with_md = if t.ends_with(".md") { t.to_string() } else { format!("{t}.md") };
        self.lookup(&with_md).or_else(|| self.lookup(t))
    }

    fn lookup(&self, t: &str) -> Option<String> {
        if t.contains('/') {
            if let Some(p) = self.by_path.get(t) {
                return Some(p.clone());
            }
            let name = t.rsplit('/').next()?;
            let suffix = format!("/{t}");
            return self
                .by_name
                .get(name)?
                .iter()
                .find(|p| p.to_lowercase().ends_with(&suffix))
                .cloned();
        }
        self.by_name.get(t).and_then(|v| v.first().cloned())
    }

    /// Resolve a path relative to the linking note's folder
    /// (`[x](../other.md)`), URL-decoding `%20`.
    pub fn resolve_relative(&self, target: &str, from: &str) -> Option<String> {
        let decoded = target.replace("%20", " ");
        let mut parts: Vec<&str> = from.split('/').collect();
        parts.pop();
        for seg in decoded.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                s => parts.push(s),
            }
        }
        let joined = parts.join("/").nfc().collect::<String>().to_lowercase();
        self.by_path.get(&joined).cloned().or_else(|| self.lookup(&joined))
    }
}

// ─── duplicates ─────────────────────────────────────────────────────────────

/// Lowercase, fold diacritics, words only.
pub fn normalize_words(s: &str) -> Vec<String> {
    let folded: String = s
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase();
    folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// Split a stem into (date prefix present, the rest as words). Recognizes
/// `YYYY-MM-DD`, `YYYY-MM` and `YYYY-Www` at the start or the end.
fn strip_date(stem: &str) -> (bool, Vec<String>) {
    let words = normalize_words(stem);
    let is_year = |w: &str| w.len() == 4 && w.starts_with("20") && w.chars().all(|c| c.is_ascii_digit());
    let is_part = |w: &str| {
        (w.len() <= 2 && w.chars().all(|c| c.is_ascii_digit()))
            || (w.len() == 3 && w.starts_with('w') && w[1..].chars().all(|c| c.is_ascii_digit()))
    };
    let mut start = 0;
    let mut end = words.len();
    let mut dated = false;
    if words.first().is_some_and(|w| is_year(w)) {
        dated = true;
        start = 1;
        while start < words.len() && start < 3 && is_part(&words[start]) {
            start += 1;
        }
    } else if end >= 2 {
        // Trailing date: find a year within the last three words.
        if let Some(pos) = (end.saturating_sub(3)..end).find(|&i| is_year(&words[i])) {
            if words[pos + 1..].iter().all(|w| is_part(w)) {
                dated = true;
                end = pos;
            }
        }
    }
    (dated, words[start.min(end)..end].to_vec())
}

fn jaccard(a: &HashSet<&str>, b: &HashSet<&str>) -> f64 {
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 { 0.0 } else { inter as f64 / union as f64 }
}

struct UnionFind(Vec<usize>);
impl UnionFind {
    fn new(n: usize) -> Self {
        Self((0..n).collect())
    }
    fn find(&mut self, x: usize) -> usize {
        let p = self.0[x];
        if p == x {
            return x;
        }
        let r = self.find(p);
        self.0[x] = r;
        r
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.0[rb] = ra;
        }
    }
    fn groups(&mut self, members: &[usize]) -> Vec<Vec<usize>> {
        let mut m: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for &i in members {
            let r = self.find(i);
            m.entry(r).or_default().push(i);
        }
        m.into_values().filter(|g| g.len() > 1).collect()
    }
}

fn folder_of(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn stem_of(rel: &str) -> &str {
    let file = rel.rsplit('/').next().unwrap_or(rel);
    file.strip_suffix(".md").unwrap_or(file)
}

const SIMILAR_TITLE_JACCARD: f64 = 0.8;
const DATED_SERIES_MIN: usize = 3;
const CONTENT_MIN_WORDS: usize = 60;
const CONTENT_JACCARD: f64 = 0.6;
const MINHASH_K: usize = 64;

fn duplicate_groups(notes: &[Note]) -> Vec<Finding> {
    let mut out = Vec::new();
    let daily = |rel: &str| rel.starts_with("2-Daily-Notes/");
    let candidates: Vec<usize> = (0..notes.len())
        .filter(|&i| {
            let rel = &notes[i].file.rel;
            !daily(rel) && !super::is_generic_name(notes[i].file.file_name())
        })
        .collect();
    let paths = |g: &[usize]| g.iter().map(|&i| notes[i].file.rel.clone()).collect::<Vec<_>>();

    // (a) Same file name in several folders: `[[name]]` is ambiguous.
    let mut by_stem: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for &i in &candidates {
        by_stem.entry(normalize_words(stem_of(&notes[i].file.rel)).join(" ")).or_default().push(i);
    }
    let mut paired: HashSet<(usize, usize)> = HashSet::new();
    for (_, g) in by_stem.iter().filter(|(k, g)| !k.is_empty() && g.len() > 1) {
        for &a in g {
            for &b in g {
                paired.insert((a, b));
            }
        }
        out.push(Finding {
            kind: KIND_DUPLICATE_NAME.into(),
            path: None,
            detail: format!("{} notes share the name \"{}\"", g.len(), stem_of(&notes[g[0]].file.rel)),
            related: paths(g),
            fixed: false,
            fix_action: None,
        });
    }

    // (b) Dated series: ≥3 date-stamped notes on one theme in one folder.
    let stripped: Vec<(bool, Vec<String>)> = notes.iter().map(|n| strip_date(stem_of(&n.file.rel))).collect();
    let mut series: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for &i in &candidates {
        let (dated, words) = &stripped[i];
        if *dated && !words.is_empty() {
            series
                .entry((folder_of(&notes[i].file.rel).to_string(), words.join(" ")))
                .or_default()
                .push(i);
        }
    }
    let mut in_series: HashMap<usize, usize> = HashMap::new();
    for (sid, ((folder, theme), g)) in series.iter().enumerate() {
        if g.len() < DATED_SERIES_MIN {
            continue;
        }
        for &i in g {
            in_series.insert(i, sid);
        }
        out.push(Finding {
            kind: KIND_DATED_SERIES.into(),
            path: None,
            detail: format!(
                "{} dated notes on \"{}\" in {} — candidates for one note with appends",
                g.len(),
                theme,
                if folder.is_empty() { "the vault root" } else { folder }
            ),
            related: paths(g),
            fixed: false,
            fix_action: None,
        });
    }
    let same_series = |a: usize, b: usize| {
        in_series.get(&a).is_some_and(|x| in_series.get(&b) == Some(x))
    };

    // (c) Similar titles (date stripped, word Jaccard).
    let title_words: Vec<HashSet<&str>> = stripped.iter().map(|(_, w)| w.iter().map(|s| s.as_str()).collect()).collect();
    let mut uf = UnionFind::new(notes.len());
    let mut similar_members = Vec::new();
    for (x, &a) in candidates.iter().enumerate() {
        if title_words[a].len() < 2 {
            continue;
        }
        for &b in &candidates[x + 1..] {
            if title_words[b].len() < 2 || paired.contains(&(a, b)) || same_series(a, b) {
                continue;
            }
            if jaccard(&title_words[a], &title_words[b]) >= SIMILAR_TITLE_JACCARD {
                uf.union(a, b);
                similar_members.push(a);
                similar_members.push(b);
                paired.insert((a, b));
                paired.insert((b, a));
            }
        }
    }
    similar_members.sort();
    similar_members.dedup();
    for g in uf.groups(&similar_members) {
        out.push(Finding {
            kind: KIND_SIMILAR_TITLE.into(),
            path: None,
            detail: format!("{} notes with near-identical titles", g.len()),
            related: paths(&g),
            fixed: false,
            fix_action: None,
        });
    }

    // (d) Near-identical bodies: MinHash over 5-word shingles.
    let sigs: Vec<Option<[u64; MINHASH_K]>> = notes.iter().map(|n| minhash(&n.parsed.body)).collect();
    let mut uf = UnionFind::new(notes.len());
    let mut content_members = Vec::new();
    let all: Vec<usize> = (0..notes.len()).filter(|&i| sigs[i].is_some()).collect();
    for (x, &a) in all.iter().enumerate() {
        let sa = sigs[a].as_ref().unwrap();
        for &b in &all[x + 1..] {
            if paired.contains(&(a, b)) || same_series(a, b) {
                continue;
            }
            let sb = sigs[b].as_ref().unwrap();
            let same = sa.iter().zip(sb.iter()).filter(|(p, q)| p == q).count();
            if same as f64 / MINHASH_K as f64 >= CONTENT_JACCARD {
                uf.union(a, b);
                content_members.push(a);
                content_members.push(b);
            }
        }
    }
    content_members.sort();
    content_members.dedup();
    for g in uf.groups(&content_members) {
        out.push(Finding {
            kind: KIND_DUPLICATE_CONTENT.into(),
            path: None,
            detail: format!("{} notes with near-identical text", g.len()),
            related: paths(&g),
            fixed: false,
            fix_action: None,
        });
    }
    out
}

/// MinHash signature of a body's 5-word shingles; `None` for short bodies.
fn minhash(body: &str) -> Option<[u64; MINHASH_K]> {
    let words = normalize_words(body);
    if words.len() < CONTENT_MIN_WORDS {
        return None;
    }
    let mut sig = [u64::MAX; MINHASH_K];
    for w in words.windows(5) {
        let base = fnv1a(w.join(" ").as_bytes());
        for (k, slot) in sig.iter_mut().enumerate() {
            let h = mix(base ^ (k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            if h < *slot {
                *slot = h;
            }
        }
    }
    Some(sig)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// splitmix64 finalizer.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

// ─── history store ──────────────────────────────────────────────────────────

const MIGRATIONS: &[crate::migrate::Migration] = &[crate::migrate::Migration {
    version: 1,
    name: "adr035-vault-check",
    step: crate::migrate::Step::Sql(
        "CREATE TABLE IF NOT EXISTS check_runs (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at          TEXT NOT NULL,
            finished_at         TEXT NOT NULL,
            trigger             TEXT NOT NULL,
            applied             INTEGER NOT NULL,
            notes_scanned       INTEGER NOT NULL,
            files_excluded      INTEGER NOT NULL,
            duration_ms         INTEGER NOT NULL,
            duplicates          INTEGER NOT NULL,
            broken_links        INTEGER NOT NULL,
            orphans             INTEGER NOT NULL,
            stale_inbox         INTEGER NOT NULL,
            missing_frontmatter INTEGER NOT NULL,
            unknown_source      INTEGER NOT NULL,
            empty_files         INTEGER NOT NULL,
            fixed               INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS check_findings (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id     INTEGER NOT NULL REFERENCES check_runs(id) ON DELETE CASCADE,
            kind       TEXT NOT NULL,
            path       TEXT,
            detail     TEXT NOT NULL,
            related    TEXT NOT NULL,
            fixed      INTEGER NOT NULL,
            fix_action TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_check_findings_run ON check_findings(run_id)",
    ),
}];

pub async fn open(workspace_root: &Path) -> Result<SqlitePool> {
    open_at(&workspace_root.join(DB_PATH)).await
}

pub async fn open_at(db: &Path) -> Result<SqlitePool> {
    let pool = crate::db::open(db).await?;
    crate::migrate::migrate(&pool, MIGRATIONS).await?;
    Ok(pool)
}

/// Store a report; returns the run id. Findings of runs older than the
/// latest [`KEEP_FINDINGS_FOR_RUNS`] are deleted (their counts remain).
pub async fn record(pool: &SqlitePool, r: &CheckReport) -> Result<i64> {
    let mut tx = pool.begin().await?;
    let c = &r.counts;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO check_runs (started_at, finished_at, trigger, applied, notes_scanned,
            files_excluded, duration_ms, duplicates, broken_links, orphans, stale_inbox,
            missing_frontmatter, unknown_source, empty_files, fixed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         RETURNING id",
    )
    .bind(&r.started_at)
    .bind(&r.finished_at)
    .bind(&r.trigger)
    .bind(r.applied)
    .bind(r.notes_scanned)
    .bind(r.files_excluded)
    .bind(r.duration_ms)
    .bind(c.duplicates)
    .bind(c.broken_links)
    .bind(c.orphans)
    .bind(c.stale_inbox)
    .bind(c.missing_frontmatter)
    .bind(c.unknown_source)
    .bind(c.empty_files)
    .bind(c.fixed)
    .fetch_one(&mut *tx)
    .await?;
    for f in &r.findings {
        sqlx::query(
            "INSERT INTO check_findings (run_id, kind, path, detail, related, fixed, fix_action)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(id)
        .bind(&f.kind)
        .bind(&f.path)
        .bind(&f.detail)
        .bind(serde_json::to_string(&f.related)?)
        .bind(f.fixed)
        .bind(&f.fix_action)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "DELETE FROM check_findings WHERE run_id NOT IN
            (SELECT id FROM check_runs ORDER BY id DESC LIMIT ?1)",
    )
    .bind(KEEP_FINDINGS_FOR_RUNS)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

fn counts_from_row(r: &sqlx::sqlite::SqliteRow) -> CheckCounts {
    CheckCounts {
        duplicates: r.get("duplicates"),
        broken_links: r.get("broken_links"),
        orphans: r.get("orphans"),
        stale_inbox: r.get("stale_inbox"),
        missing_frontmatter: r.get("missing_frontmatter"),
        unknown_source: r.get("unknown_source"),
        empty_files: r.get("empty_files"),
        fixed: r.get("fixed"),
    }
}

/// Most recent runs, newest first.
pub async fn runs(pool: &SqlitePool, limit: i64) -> Result<Vec<CheckRunSummary>> {
    let rows = sqlx::query("SELECT * FROM check_runs ORDER BY id DESC LIMIT ?1")
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| CheckRunSummary {
            id: r.get("id"),
            started_at: r.get("started_at"),
            trigger: r.get("trigger"),
            applied: r.get::<i64, _>("applied") != 0,
            notes_scanned: r.get("notes_scanned"),
            duration_ms: r.get("duration_ms"),
            counts: counts_from_row(r),
        })
        .collect())
}

/// The latest run with its findings.
pub async fn latest(pool: &SqlitePool) -> Result<Option<CheckReport>> {
    let Some(r) = sqlx::query("SELECT * FROM check_runs ORDER BY id DESC LIMIT 1")
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let id: i64 = r.get("id");
    let findings = sqlx::query(
        "SELECT kind, path, detail, related, fixed, fix_action FROM check_findings
          WHERE run_id = ?1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|f| Finding {
        kind: f.get("kind"),
        path: f.get("path"),
        detail: f.get("detail"),
        related: serde_json::from_str(&f.get::<String, _>("related")).unwrap_or_default(),
        fixed: f.get::<i64, _>("fixed") != 0,
        fix_action: f.get("fix_action"),
    })
    .collect();
    Ok(Some(CheckReport {
        run_id: Some(id),
        started_at: r.get("started_at"),
        finished_at: r.get("finished_at"),
        trigger: r.get("trigger"),
        applied: r.get::<i64, _>("applied") != 0,
        notes_scanned: r.get("notes_scanned"),
        files_excluded: r.get("files_excluded"),
        duration_ms: r.get("duration_ms"),
        counts: counts_from_row(&r),
        findings,
    }))
}

/// One-line WhatsApp summary: non-zero counts only.
pub fn summary_line(c: &CheckCounts, link: Option<&str>) -> String {
    let parts: Vec<String> = [
        (c.duplicates, "duplicate", "duplicates"),
        (c.broken_links, "broken link", "broken links"),
        (c.orphans, "orphan", "orphans"),
        (c.stale_inbox, "stale inbox note", "stale inbox notes"),
        (c.missing_frontmatter, "frontmatter issue", "frontmatter issues"),
        (c.unknown_source, "unknown source", "unknown sources"),
        (c.empty_files, "empty file", "empty files"),
        (c.fixed, "fixed", "fixed"),
    ]
    .iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, one, many)| format!("{n} {}", if *n == 1 { one } else { many }))
    .collect();
    let body = if parts.is_empty() { "clean".to_string() } else { parts.join(", ") };
    match link {
        Some(l) => format!("🗂️ vault check: {body} — {l}"),
        None => format!("🗂️ vault check: {body}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VaultCheckConfig;
    use std::fs;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn fm(created: &str, source: &str) -> String {
        format!("---\ncreated: {created}\nsource: {source}\n---\n")
    }

    const LONG: &str = "The quarterly planning session covered the migration of the \
        billing service to the new queue, the retirement of the legacy cron host, the \
        on-call rotation for the next two months, and the budget for load testing. \
        Everyone agreed that the queue migration goes first because the cron host \
        depends on it, and that load testing waits until the queue is stable in \
        production for at least one full week without incidents or manual restarts.";

    /// Synthetic vault. Every name and body is invented for the test.
    fn fixture(root: &Path) {
        let today = chrono::Local::now().date_naive();
        let old = (today - chrono::Duration::days(30)).to_string();
        let recent = (today - chrono::Duration::days(2)).to_string();
        write(root, "0-Inbox/README.md", "# Inbox\n");
        write(root, "0-Inbox/old-capture.md", &format!("{}Old capture.\n", fm(&old, "whatsapp-braindump")));
        write(root, "0-Inbox/new-capture.md", &format!("{}New capture.\n", fm(&recent, "whatsapp-braindump")));
        write(
            root,
            "3-Projects/Alpha/index.md",
            &format!(
                "{}# Alpha\n\n[[engine-notes]] [[Engine-Notes|alias]] [[folder-a/shared-name]]\n\
                 | table | [[budget plan\\|budget]] |\n[[missing-note]] [[Missing-Note#h]]\n\
                 ```\n[[inside-fence]]\n```\n`[[inline-code]]` ![[diagram.png]] [[Homelab/router]]\n\
                 [[#local]] [[Beta]] [[Team]]\n",
                fm("2026-01-01", "manual")
            ),
        );
        write(root, "3-Projects/Alpha/engine-notes.md", &format!("{}# Engine\n\nSee [[index]].\n", fm("2026-01-02", "claude-code-research")));
        write(root, "3-Projects/Alpha/budget plan.md", &format!("{}# Budget\n", fm("2026-01-03", "made-up-writer")));
        write(root, "3-Projects/Alpha/diagram.png", "png");
        write(root, "3-Projects/Beta.md", &format!("{}# Beta\n\n{LONG}\n", fm("2026-01-04", "manual")));
        write(root, "7-Archives/Beta copy.md", &format!("{}# Beta copy\n\n{LONG}\n", fm("2026-01-04", "manual")));
        write(root, "3-Projects/folder-a/shared-name.md", &format!("{}x\n", fm("2026-01-05", "manual")));
        write(root, "5-Resources/folder-b/shared-name.md", &format!("{}y [[shared-name]]\n", fm("2026-01-05", "manual")));
        for d in ["2026-03-01", "2026-03-08", "2026-03-15"] {
            write(root, &format!("4-Areas/Team/{d}-standup.md"), &format!("{}notes [[Beta]]\n", fm(d, "manual")));
        }
        write(root, "4-Areas/Team/weekly review plan notes.md", &format!("{}a [[Beta]]\n", fm("2026-01-06", "manual")));
        write(root, "4-Areas/Team/weekly review plan notes v2.md", &format!("{}b [[Beta]]\n", fm("2026-01-06", "voice-dictation+research")));
        write(root, "6-Slipbox/no-frontmatter.md", "Just text [[Beta]].\n");
        write(root, "6-Slipbox/broken-yaml.md", "---\ncreated: [oops\n---\ntext\n");
        write(root, "6-Slipbox/lonely.md", &format!("{}Nobody links here.\n", fm("2026-01-07", "manual")));
        write(root, "Untitled.canvas", "{}");
        write(root, "Untitled 1.md", "");
        write(root, "6-Slipbox/placeholder.md", "   \n");
        write(root, "6-Slipbox/refers.md", &format!("{}[[placeholder]] [[lonely-typo]]\n", fm("2026-01-08", "manual")));
        // Excluded: never read, never reported, but links to them resolve.
        write(root, "4-Areas/Homelab/router.md", "# Router\n[[nowhere]]\n");
        write(root, "6-Slipbox/wifi.md", "- password: correct-horse\n[[also-nowhere]]\n");
        write(root, ".obsidian/app.json", "{}");
    }

    fn opts(apply: bool) -> CheckOptions {
        CheckOptions::from_config(&VaultCheckConfig::default(), apply, "manual").unwrap()
    }

    fn ex() -> Exclusions {
        Exclusions::new(&[], "").unwrap()
    }

    fn of<'a>(r: &'a CheckReport, kind: &str) -> Vec<&'a Finding> {
        r.findings.iter().filter(|f| f.kind == kind).collect()
    }

    #[test]
    fn report_covers_every_finding_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path();
        fixture(vault);
        let r = run(vault, &ex(), &opts(false)).unwrap();

        // Excluded notes never appear anywhere in the report.
        for f in &r.findings {
            for p in f.path.iter().chain(f.related.iter()) {
                assert!(!p.contains("Homelab") && !p.contains("wifi"), "leaked {p}");
            }
        }
        assert_eq!(r.files_excluded, 2);

        // Broken links: code ignored, aliases/escapes/headings handled, case
        // folded, one finding per distinct target per note.
        let broken: Vec<String> = of(&r, KIND_BROKEN_LINK).iter().map(|f| f.detail.clone()).collect();
        assert_eq!(broken.len(), 3, "{broken:?}");
        assert!(broken[0].contains("[[missing-note]]"));
        assert!(broken[1].contains("[[Team]]") && broken[1].contains("a folder with this name exists"));
        assert!(broken[2].contains("[[lonely-typo]]"));
        assert_eq!(r.counts.broken_links, 3);
        let folder_hint = of(&r, KIND_BROKEN_LINK)[1];
        assert_eq!(folder_hint.related, vec!["4-Areas/Team".to_string()]);

        // Orphans: Alpha/index is linked from engine-notes via [[index]];
        // lonely is not; exempt folders are skipped.
        let orphans: Vec<&str> = of(&r, KIND_ORPHAN).iter().map(|f| f.path.as_deref().unwrap()).collect();
        assert!(orphans.contains(&"6-Slipbox/lonely.md"), "{orphans:?}");
        assert!(!orphans.contains(&"3-Projects/Alpha/engine-notes.md"));
        assert!(!orphans.iter().any(|p| p.starts_with("0-Inbox/") || p.starts_with("7-Archives/")));

        // Stale inbox: only the 30-day-old capture; README skipped.
        let stale = of(&r, KIND_STALE_INBOX);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].path.as_deref(), Some("0-Inbox/old-capture.md"));

        // Frontmatter.
        let fmf: Vec<(&str, &str)> = of(&r, KIND_FRONTMATTER)
            .iter()
            .map(|f| (f.path.as_deref().unwrap(), f.detail.as_str()))
            .collect();
        assert!(fmf.contains(&("6-Slipbox/no-frontmatter.md", "no frontmatter block")));
        assert!(fmf.iter().any(|(p, d)| *p == "6-Slipbox/broken-yaml.md" && d.contains("not a valid")));
        assert!(!fmf.iter().any(|(p, _)| p.ends_with("README.md")));

        // Source vocabulary: prefix entries and `+` joins accepted.
        let unknown = of(&r, KIND_UNKNOWN_SOURCE);
        assert_eq!(unknown.len(), 1, "{unknown:?}");
        assert!(unknown[0].detail.contains("made-up-writer"));
        assert_eq!(r.counts.unknown_source, 1);

        // Empty files.
        let empty: Vec<&str> = of(&r, KIND_EMPTY_FILE).iter().map(|f| f.path.as_deref().unwrap()).collect();
        assert_eq!(empty, vec!["6-Slipbox/placeholder.md", "Untitled 1.md", "Untitled.canvas"]);

        // Duplicates.
        let names = of(&r, KIND_DUPLICATE_NAME);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].related.len(), 2);
        let series = of(&r, KIND_DATED_SERIES);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].related.len(), 3);
        assert!(series[0].detail.contains("standup"));
        let similar = of(&r, KIND_SIMILAR_TITLE);
        assert_eq!(similar.len(), 1, "{similar:?}");
        let content = of(&r, KIND_DUPLICATE_CONTENT);
        assert_eq!(content.len(), 1, "{content:?}");
        assert!(content[0].related.contains(&"7-Archives/Beta copy.md".to_string()));
        assert_eq!(r.counts.duplicates, 4);

        // Report-only: nothing changed on disk.
        assert!(vault.join("Untitled.canvas").exists());
        assert_eq!(fs::read_to_string(vault.join("6-Slipbox/no-frontmatter.md")).unwrap(), "Just text [[Beta]].\n");
        assert_eq!(r.counts.fixed, 0);
    }

    #[test]
    fn apply_fixes_only_unambiguous_cases() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path();
        fixture(vault);
        let before = fs::metadata(vault.join("6-Slipbox/no-frontmatter.md")).unwrap().modified().unwrap();
        let r = run(vault, &ex(), &opts(true)).unwrap();

        // Untitled empties deleted; a linked placeholder is kept.
        assert!(!vault.join("Untitled.canvas").exists());
        assert!(!vault.join("Untitled 1.md").exists());
        assert!(vault.join("6-Slipbox/placeholder.md").exists());

        // `created` added, mtime preserved, invalid YAML left alone.
        let text = fs::read_to_string(vault.join("6-Slipbox/no-frontmatter.md")).unwrap();
        assert!(text.starts_with("---\ncreated: "), "{text}");
        assert!(text.ends_with("---\nJust text [[Beta]].\n"));
        let after = fs::metadata(vault.join("6-Slipbox/no-frontmatter.md")).unwrap().modified().unwrap();
        assert_eq!(before, after);
        assert_eq!(fs::read_to_string(vault.join("6-Slipbox/broken-yaml.md")).unwrap(), "---\ncreated: [oops\n---\ntext\n");

        // Excluded files untouched.
        assert_eq!(fs::read_to_string(vault.join("4-Areas/Homelab/router.md")).unwrap(), "# Router\n[[nowhere]]\n");

        let fixed: Vec<&Finding> = r.findings.iter().filter(|f| f.fix_action.is_some()).collect();
        assert_eq!(r.counts.fixed as usize, fixed.len());
        assert_eq!(fixed.len(), 3, "{fixed:?}");
        // The note still lacks `source`, so its finding stays open.
        let nf = fixed.iter().find(|f| f.path.as_deref() == Some("6-Slipbox/no-frontmatter.md")).unwrap();
        assert!(!nf.fixed);

        // A second pass finds nothing left to fix.
        let r2 = run(vault, &ex(), &opts(true)).unwrap();
        assert_eq!(r2.counts.fixed, 0);
        let d = r2
            .findings
            .iter()
            .find(|f| f.path.as_deref() == Some("6-Slipbox/no-frontmatter.md") && f.kind == KIND_FRONTMATTER)
            .unwrap();
        assert_eq!(d.detail, "missing: source");
    }

    #[test]
    fn resolver_follows_obsidian_rules() {
        let paths: Vec<String> = ["a/Note.md", "b/c/Note.md", "x/Other Name.md", "img/pic.png", "Top.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let r = Resolver::new(&paths);
        assert_eq!(r.resolve("note", "z.md").as_deref(), Some("a/Note.md"));
        assert_eq!(r.resolve("c/Note", "z.md").as_deref(), Some("b/c/Note.md"));
        assert_eq!(r.resolve("other name", "z.md").as_deref(), Some("x/Other Name.md"));
        assert_eq!(r.resolve("pic.png", "z.md").as_deref(), Some("img/pic.png"));
        assert_eq!(r.resolve("Top.md", "z.md").as_deref(), Some("Top.md"));
        assert_eq!(r.resolve("nope", "z.md"), None);
        assert_eq!(r.resolve_relative("../x/Other%20Name.md", "a/Note.md").as_deref(), Some("x/Other Name.md"));
    }

    #[test]
    fn source_vocabulary_matching() {
        let v: Vec<String> = ["manual", "claude-code*"].iter().map(|s| s.to_string()).collect();
        assert!(source_known("manual", &v));
        assert!(source_known("Claude-Code-Research", &v));
        assert!(source_known("manual+claude-code", &v));
        assert!(!source_known("manual+other", &v));
        assert!(!source_known("other", &v));
    }

    #[test]
    fn dates_are_stripped_from_titles() {
        assert_eq!(strip_date("2026-03-01-standup"), (true, vec!["standup".to_string()]));
        assert_eq!(strip_date("2026-W19-discord"), (true, vec!["discord".to_string()]));
        assert_eq!(strip_date("review 2026-05"), (true, vec!["review".to_string()]));
        assert_eq!(strip_date("plain title"), (false, vec!["plain".to_string(), "title".to_string()]));
    }

    #[tokio::test]
    async fn history_roundtrip_and_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        fixture(&vault);
        let r = run(&vault, &ex(), &opts(false)).unwrap();
        let pool = open_at(&tmp.path().join("check.db")).await.unwrap();
        let id = record(&pool, &r).await.unwrap();
        let got = latest(&pool).await.unwrap().unwrap();
        assert_eq!(got.run_id, Some(id));
        assert_eq!(got.counts, r.counts);
        assert_eq!(got.findings, r.findings);
        record(&pool, &r).await.unwrap();
        assert_eq!(runs(&pool, 10).await.unwrap().len(), 2);

        let line = summary_line(
            &CheckCounts { duplicates: 3, broken_links: 5, fixed: 2, ..Default::default() },
            Some("https://example.invalid/vault/check"),
        );
        assert_eq!(line, "🗂️ vault check: 3 duplicates, 5 broken links, 2 fixed — https://example.invalid/vault/check");
        assert_eq!(summary_line(&CheckCounts::default(), None), "🗂️ vault check: clean");
    }
}
