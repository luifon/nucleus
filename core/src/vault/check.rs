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
//! - **oversized** — notes over [`super::MAX_NOTE_BYTES`], skipped unread.
//!
//! Fixes (only with `apply`, each recorded on its finding; see the "fixes"
//! section for the checks each one makes before it touches a file):
//!
//! - move an empty file whose name starts with `Untitled` (Obsidian's
//!   default name for a new note, canvas or base) and that nothing links to
//!   into the quarantine;
//! - add `created: <date>` from the file's birth time to a note that has no
//!   `created` key (valid or missing frontmatter only; the file's mtime is
//!   kept).
//!
//! Notes are otherwise never moved or renamed. Excluded paths and
//! credential notes are never read for findings, never listed, and never
//! modified.

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
pub const KIND_OVERSIZED: &str = "oversized";

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
    /// Notes over the size limit, not checked.
    #[ts(type = "number")]
    pub oversized: i64,
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
    /// Where fixes put removed files and recovery copies
    /// (`<workspace>/`[`QUARANTINE_DIR`] for `vault-check`). Required when
    /// `apply` is set and there is something to fix.
    pub quarantine_dir: Option<std::path::PathBuf>,
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
            quarantine_dir: None,
        })
    }
}

// ─── analysis ───────────────────────────────────────────────────────────────

struct Note {
    file: VaultFile,
    parsed: ParsedNote,
    text: String,
}

/// An empty file that `apply` may move to the quarantine.
struct DeleteCandidate {
    file: VaultFile,
    finding: usize,
}

/// A note that `apply` may give a `created:` key.
struct CreatedCandidate {
    note: usize,
    finding: usize,
}

/// The result of reading the vault, before any fix.
pub struct Analysis {
    notes: Vec<Note>,
    findings: Vec<Finding>,
    counts: CheckCounts,
    files_excluded: i64,
    deletes: Vec<DeleteCandidate>,
    created: Vec<CreatedCandidate>,
    started: std::time::Instant,
    started_at: String,
}

/// Run the check over `vault`. Fixes are applied only when `opts.apply`.
pub fn run(vault: &Path, ex: &Exclusions, opts: &CheckOptions) -> Result<CheckReport> {
    let mut a = analyze(vault, ex, opts)?;
    if opts.apply {
        apply(&mut a, vault, ex, opts)?;
    }
    Ok(a.into_report(opts))
}

impl Analysis {
    fn into_report(self, opts: &CheckOptions) -> CheckReport {
        CheckReport {
            run_id: None,
            started_at: self.started_at,
            finished_at: crate::timestamp::now(),
            trigger: opts.trigger.clone(),
            applied: opts.apply,
            notes_scanned: self.notes.len() as i64,
            files_excluded: self.files_excluded,
            duration_ms: self.started.elapsed().as_millis() as i64,
            counts: self.counts,
            findings: self.findings,
        }
    }
}

/// Markdown notes the check may read, and what it left out.
struct Loaded {
    notes: Vec<Note>,
    others: Vec<VaultFile>,
    /// Every path a link may resolve to, excluded files included.
    targets: Vec<String>,
    /// Paths that may appear in a finding (not excluded).
    reportable: Vec<String>,
    oversized: Vec<VaultFile>,
    files_excluded: i64,
}

fn load(vault: &Path, ex: &Exclusions) -> Result<Loaded> {
    let walk = scan::walk(vault, ex)?;
    let mut l = Loaded {
        notes: Vec::new(),
        others: Vec::new(),
        // A link to an excluded note is not broken. Paths only; excluded
        // files are not read, and their paths are used for nothing else.
        targets: walk.excluded.clone(),
        reportable: Vec::new(),
        oversized: Vec::new(),
        files_excluded: walk.excluded.len() as i64,
    };
    for f in walk.files {
        l.targets.push(f.rel.clone());
        if !f.is_markdown() {
            l.reportable.push(f.rel.clone());
            l.others.push(f);
            continue;
        }
        if f.size > super::MAX_NOTE_BYTES {
            l.reportable.push(f.rel.clone());
            l.oversized.push(f);
            continue;
        }
        let text = match super::read_note_capped(&f.abs) {
            Ok(Some(t)) => t,
            Ok(None) => {
                l.reportable.push(f.rel.clone());
                l.oversized.push(f);
                continue;
            }
            Err(_) => continue,
        };
        if ex.content_excluded(&text) {
            l.files_excluded += 1;
            continue;
        }
        l.reportable.push(f.rel.clone());
        let parsed = note::parse(&f.rel, &text);
        l.notes.push(Note { file: f, parsed, text });
    }
    Ok(l)
}

/// How many other notes link to each path (wiki-links and relative
/// markdown links).
fn inbound_counts(notes: &[Note], resolver: &Resolver) -> HashMap<String, usize> {
    let mut inbound: HashMap<String, usize> = HashMap::new();
    for n in notes {
        let wiki = n.parsed.links.iter().filter_map(|l| resolver.resolve(&l.target, &n.file.rel));
        let md = n.parsed.md_links.iter().filter_map(|m| resolver.resolve_relative(m, &n.file.rel));
        for t in wiki.chain(md) {
            if t != n.file.rel {
                *inbound.entry(t).or_default() += 1;
            }
        }
    }
    inbound
}

/// Read the vault and build the report, without changing anything.
pub fn analyze(vault: &Path, ex: &Exclusions, opts: &CheckOptions) -> Result<Analysis> {
    let started = std::time::Instant::now();
    let started_at = crate::timestamp::now();
    let Loaded { notes, others, targets, reportable, oversized, files_excluded } = load(vault, ex)?;
    // Excluded paths resolve links, but only reportable paths name the
    // folder in a broken-link hint.
    let resolver = Resolver::new(&targets, &reportable);

    let mut findings: Vec<Finding> = Vec::new();
    let mut counts = CheckCounts::default();

    for f in &oversized {
        counts.oversized += 1;
        findings.push(simple(
            KIND_OVERSIZED,
            &f.rel,
            format!("{} bytes, over the {} byte limit; not checked", f.size, super::MAX_NOTE_BYTES),
        ));
    }

    // Broken links.
    let inbound = inbound_counts(&notes, &resolver);
    for n in &notes {
        let mut seen: HashSet<String> = HashSet::new();
        for l in &n.parsed.links {
            if resolver.resolve(&l.target, &n.file.rel).is_some() || !seen.insert(l.target.to_lowercase()) {
                continue;
            }
            counts.broken_links += 1;
            // `[[Folder]]` does not open a folder in Obsidian; name the
            // folder so the fix (link its README or index) is obvious.
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
    let mut created: Vec<CreatedCandidate> = Vec::new();
    for (i, n) in notes.iter().enumerate() {
        if opts.frontmatter_exempt.matches(&n.file.rel) {
            continue;
        }
        let fm = &n.parsed.frontmatter;
        let detail = match fm {
            Frontmatter::Missing => Some("no frontmatter block".to_string()),
            Frontmatter::Invalid => Some("frontmatter is not a valid YAML mapping".to_string()),
            Frontmatter::Valid(_) => {
                // Absent keys and keys present with no value are reported
                // apart: only an absent key can be added by a fix.
                let (mut missing, mut empty) = (Vec::new(), Vec::new());
                for k in opts.required_frontmatter.iter().filter(|k| !fm.has_key(k)) {
                    if fm.contains_key(k) { empty.push(k.as_str()) } else { missing.push(k.as_str()) }
                }
                let mut parts = Vec::new();
                if !missing.is_empty() {
                    parts.push(format!("missing: {}", missing.join(", ")));
                }
                if !empty.is_empty() {
                    parts.push(format!("empty: {}", empty.join(", ")));
                }
                (!parts.is_empty()).then(|| parts.join("; "))
            }
        };
        if let Some(detail) = detail {
            counts.missing_frontmatter += 1;
            let needs_created = opts.required_frontmatter.iter().any(|k| k == "created")
                && !matches!(fm, Frontmatter::Invalid)
                && !fm.contains_key("created");
            findings.push(simple(KIND_FRONTMATTER, &n.file.rel, detail));
            // Empty notes are reported as empty, not given frontmatter.
            if needs_created && n.file.birthtime.is_some() && !note_is_empty(&n.text) {
                created.push(CreatedCandidate { note: i, finding: findings.len() - 1 });
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
    let mut deletes: Vec<DeleteCandidate> = Vec::new();
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
            deletes.push(DeleteCandidate { file: f.clone(), finding: findings.len() - 1 });
        }
    }

    // Duplicates.
    for g in duplicate_groups(&notes) {
        counts.duplicates += 1;
        findings.push(g);
    }

    Ok(Analysis { notes, findings, counts, files_excluded, deletes, created, started, started_at })
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

/// Largest file the empty-file rules read.
const EMPTY_PROBE_BYTES: u64 = 4096;

/// A canvas with no nodes (`{}` or `{"nodes":[]}`); other non-markdown
/// files count as empty only at 0 bytes.
fn other_is_empty(f: &VaultFile) -> bool {
    if !f.rel.to_lowercase().ends_with(".canvas") || f.size > EMPTY_PROBE_BYTES {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(&f.abs) else { return false };
    canvas_is_empty(&text)
}

fn canvas_is_empty(text: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => v
            .get("nodes")
            .and_then(|n| n.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true),
        Err(_) => text.trim().is_empty(),
    }
}

/// The empty-file rule applied to bytes read from an open file.
fn bytes_are_empty(rel: &str, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let lower = rel.to_lowercase();
    let Ok(text) = std::str::from_utf8(bytes) else { return false };
    if lower.ends_with(".md") {
        note_is_empty(text)
    } else if lower.ends_with(".canvas") {
        canvas_is_empty(text)
    } else {
        false
    }
}

// ─── fixes ──────────────────────────────────────────────────────────────────
//
// Both fixes act on a file only when it is still the file the analysis read:
// the same device and inode, size, and nanosecond mtime, opened without
// following a symlink. Nothing is unlinked: an empty file is renamed into
// the quarantine, and a note that gets `created:` is first copied there.
// The quarantine is `memory/vault-quarantine/<run>/` in the workspace
// (Nucleus-owned, outside the vault, so Obsidian and its sync never see
// it); run folders older than [`QUARANTINE_RETENTION_DAYS`] are removed at
// the start of the next applying run.

/// Workspace-relative quarantine root used by `vault-check`.
pub const QUARANTINE_DIR: &str = "memory/vault-quarantine";
pub const QUARANTINE_RETENTION_DAYS: i64 = 30;
/// Run folder names: `<UTC timestamp>-<pid>`.
const RUN_DIR_FORMAT: &str = "%Y%m%dT%H%M%SZ";

enum Outcome {
    Done(String),
    Skipped(&'static str),
}

fn apply(a: &mut Analysis, vault: &Path, ex: &Exclusions, opts: &CheckOptions) -> Result<()> {
    if a.deletes.is_empty() && a.created.is_empty() {
        return Ok(());
    }
    let quarantine = opts
        .quarantine_dir
        .as_deref()
        .context("vault check: applying fixes needs a quarantine directory")?;
    purge_quarantine(quarantine, chrono::Utc::now());
    let run_dir = quarantine.join(format!("{}-{}", chrono::Utc::now().format(RUN_DIR_FORMAT), std::process::id()));

    // Inbound links as they are now, not as they were at analysis time: a
    // note written since then may link to a candidate.
    let inbound_now = if a.deletes.is_empty() {
        HashMap::new()
    } else {
        let l = load(vault, ex)?;
        inbound_counts(&l.notes, &Resolver::new(&l.targets, &l.reportable))
    };
    for c in std::mem::take(&mut a.deletes) {
        let outcome = if inbound_now.get(&c.file.rel).copied().unwrap_or(0) > 0 {
            Ok(Outcome::Skipped("a note links to it now"))
        } else {
            quarantine_empty(&c.file, &run_dir)
        };
        record_outcome(&mut a.findings[c.finding], &mut a.counts, outcome, true);
    }

    for c in std::mem::take(&mut a.created) {
        let n = &a.notes[c.note];
        let Some(date) = n.file.birthtime.and_then(unix_to_local_date) else { continue };
        let outcome = add_created(&n.file, &n.text, &n.parsed.frontmatter, date, &run_dir);
        // The finding is closed only when `created` was its only problem.
        let only_created = opts
            .required_frontmatter
            .iter()
            .filter(|k| k.as_str() != "created")
            .all(|k| n.parsed.frontmatter.has_key(k));
        record_outcome(&mut a.findings[c.finding], &mut a.counts, outcome, only_created);
    }
    Ok(())
}

fn record_outcome(f: &mut Finding, counts: &mut CheckCounts, outcome: Result<Outcome>, closes: bool) {
    match outcome {
        Ok(Outcome::Done(action)) => {
            f.fixed = closes;
            f.fix_action = Some(action);
            counts.fixed += 1;
        }
        Ok(Outcome::Skipped(why)) => f.fix_action = Some(format!("not applied: {why}")),
        Err(e) => {
            // Logged without the note's path: scheduled logs carry counts only.
            tracing::warn!(kind = %f.kind, err = %e, "vault-check: fix failed");
            f.fix_action = Some(format!("not applied: {e}"));
        }
    }
}

/// Remove quarantine run folders older than the retention.
fn purge_quarantine(quarantine: &Path, now: chrono::DateTime<chrono::Utc>) {
    let Ok(entries) = std::fs::read_dir(quarantine) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(stamp) = name.split('-').next() else { continue };
        let Ok(t) = chrono::NaiveDateTime::parse_from_str(stamp, RUN_DIR_FORMAT) else { continue };
        if (now.naive_utc() - t).num_days() > QUARANTINE_RETENTION_DAYS {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)
}

/// Open `f` for a fix: no symlink, still the scanned file. `None` when it
/// is not.
fn open_same(f: &VaultFile) -> Result<Option<std::fs::File>> {
    let file = match open_no_follow(&f.abs) {
        Ok(file) => file,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) || e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    Ok(f.same_file(&file.metadata()?).then_some(file))
}

/// Move an empty file into the quarantine after checking, on the open file,
/// that it is the scanned file and still empty. After the rename the moved
/// file's identity is checked again; a different file is moved back.
fn quarantine_empty(f: &VaultFile, run_dir: &Path) -> Result<Outcome> {
    use std::io::Read;
    let Some(file) = open_same(f)? else { return Ok(Outcome::Skipped("changed since the scan")) };
    if f.size > EMPTY_PROBE_BYTES {
        return Ok(Outcome::Skipped("no longer empty"));
    }
    let mut bytes = Vec::new();
    (&file).take(EMPTY_PROBE_BYTES + 1).read_to_end(&mut bytes)?;
    if !bytes_are_empty(&f.rel, &bytes) {
        return Ok(Outcome::Skipped("no longer empty"));
    }
    let dest = run_dir.join("deleted").join(&f.rel);
    std::fs::create_dir_all(dest.parent().context("quarantine path")?)?;
    // The path must still name the verified file right before the rename.
    if !std::fs::symlink_metadata(&f.abs).is_ok_and(|m| f.same_file(&m)) {
        return Ok(Outcome::Skipped("changed since the scan"));
    }
    if let Err(e) = std::fs::rename(&f.abs, &dest) {
        if e.kind() == std::io::ErrorKind::CrossesDevices {
            anyhow::bail!("the quarantine is on another filesystem than the vault");
        }
        return Err(e.into());
    }
    if std::fs::symlink_metadata(&dest).is_ok_and(|m| f.same_file(&m)) {
        return Ok(Outcome::Done("moved to the vault-check quarantine".into()));
    }
    // Another file took the path between the check and the rename: put it
    // back. `hard_link` fails if something was created at the path since.
    if std::fs::hard_link(&dest, &f.abs).is_ok() {
        let _ = std::fs::remove_file(&dest);
        return Ok(Outcome::Skipped("changed during the move; restored"));
    }
    anyhow::bail!("changed during the move; left in the quarantine run folder")
}

/// Line ending of the note's first line (`\r\n` or `\n`).
fn line_ending(text: &str) -> &'static str {
    match text.find('\n') {
        Some(i) if i > 0 && text.as_bytes()[i - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

/// Insert `created: <date>` into a note that has no `created` key. The note
/// is opened once without following symlinks and checked to be the scanned
/// file with the analysed text; the original is copied to the quarantine;
/// the new text is written to a temporary file in the same folder with the
/// original's permissions and mtime, and renamed over the note after a
/// final identity check. The note's line ending is kept. The rename gives
/// the note a new inode, so its filesystem birth time becomes the time of
/// the fix; the `created:` value keeps the original date.
fn add_created(
    f: &VaultFile,
    text: &str,
    fm: &Frontmatter,
    date: chrono::NaiveDate,
    run_dir: &Path,
) -> Result<Outcome> {
    use std::io::{Read, Write};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if text.starts_with('\u{feff}') {
        return Ok(Outcome::Skipped("the note starts with a byte-order mark"));
    }
    if fm.contains_key("created") {
        return Ok(Outcome::Skipped("the note already has a created key"));
    }
    let Some(file) = open_same(f)? else { return Ok(Outcome::Skipped("changed since the scan")) };
    let mut current = String::new();
    (&file).take(super::MAX_NOTE_BYTES + 1).read_to_string(&mut current)?;
    if current != text {
        return Ok(Outcome::Skipped("changed since the scan"));
    }
    let eol = line_ending(text);
    let new_text = match fm {
        Frontmatter::Valid(_) => {
            let Some(nl) = text.find('\n') else { return Ok(Outcome::Skipped("no line after the frontmatter start")) };
            format!("{}created: {date}{eol}{}", &text[..nl + 1], &text[nl + 1..])
        }
        Frontmatter::Missing => format!("---{eol}created: {date}{eol}---{eol}{text}"),
        Frontmatter::Invalid => return Ok(Outcome::Skipped("invalid YAML")),
    };

    // Recovery copy.
    let backup = run_dir.join("backup").join(&f.rel);
    std::fs::create_dir_all(backup.parent().context("quarantine path")?)?;
    std::fs::write(&backup, text)?;

    let meta = file.metadata()?;
    let dir = f.abs.parent().context("note has no parent folder")?;
    let name = f.abs.file_name().context("note has no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.vault-check-{}.tmp", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut t = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        t.write_all(new_text.as_bytes())?;
        t.set_permissions(std::fs::Permissions::from_mode(meta.permissions().mode() & 0o7777))?;
        t.set_modified(meta.modified()?)?;
        t.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    // Final check: the open note was not written to, and the path still
    // names it (no swap for another file or a symlink).
    let unchanged = file.metadata().is_ok_and(|m| f.same_file(&m))
        && std::fs::symlink_metadata(&f.abs).is_ok_and(|m| f.same_file(&m));
    if !unchanged {
        let _ = std::fs::remove_file(&tmp);
        return Ok(Outcome::Skipped("changed during the fix"));
    }
    if let Err(e) = std::fs::rename(&tmp, &f.abs) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(Outcome::Done(format!("added created: {date}")))
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
    /// `paths` are every link target, excluded files included (a link to
    /// an excluded note is not broken). The folder hints come only from
    /// `hint_paths`, the paths a finding may name, so an excluded folder's
    /// name never appears in a report.
    pub fn new(paths: &[String], hint_paths: &[String]) -> Self {
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        let mut by_path = HashMap::new();
        let mut folders: HashMap<String, String> = HashMap::new();
        for p in paths {
            let lower = p.to_lowercase();
            let name = lower.rsplit('/').next().unwrap_or(&lower).to_string();
            by_name.entry(name).or_default().push(p.clone());
            by_path.insert(lower, p.clone());
        }
        for p in hint_paths {
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

    // (c) Similar titles: the parsed title (frontmatter `title`, first H1,
    // else the file stem), date stripped, word Jaccard.
    let titles: Vec<Vec<String>> = notes.iter().map(|n| strip_date(&n.parsed.title).1).collect();
    let title_words: Vec<HashSet<&str>> = titles.iter().map(|w| w.iter().map(|s| s.as_str()).collect()).collect();
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
    let sigs: Vec<Option<[u64; MINHASH_K]>> = notes.iter().map(|n| minhash(n.parsed.body(&n.text))).collect();
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

const MIGRATIONS: &[crate::migrate::Migration] = &[
    crate::migrate::Migration {
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
    },
    crate::migrate::Migration {
        version: 2,
        name: "adr035-oversized-and-schedule-claims",
        step: crate::migrate::Step::Sql(
            "ALTER TABLE check_runs ADD COLUMN oversized INTEGER NOT NULL DEFAULT 0;
            CREATE TABLE IF NOT EXISTS scheduled_claims (
                occurrence   TEXT PRIMARY KEY,
                claimed_at   TEXT NOT NULL,
                run_id       INTEGER,
                completed_at TEXT
            )",
        ),
    },
];

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
            missing_frontmatter, unknown_source, empty_files, fixed, oversized)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
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
    .bind(c.oversized)
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
        oversized: r.get("oversized"),
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

// ─── scheduled occurrences ──────────────────────────────────────────────────

/// Result of [`claim_occurrence`].
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// This process runs the occurrence.
    Acquired,
    /// A previous run completed it; nothing to do.
    Completed,
    /// Another process holds a live claim.
    Busy,
}

/// Claim one scheduled occurrence, keyed by its cron match time. The row is
/// the lock: two `--scheduled` processes serialize on `BEGIN IMMEDIATE`,
/// and only one sees `Acquired`. A claim that was never completed (the
/// process died) can be taken over once it is older than `stale_after`.
pub async fn claim_occurrence(
    pool: &SqlitePool,
    occurrence: &str,
    now: chrono::DateTime<chrono::Utc>,
    stale_after: chrono::Duration,
) -> Result<Claim> {
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let result = async {
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT claimed_at, completed_at FROM scheduled_claims WHERE occurrence = ?1")
                .bind(occurrence)
                .fetch_optional(&mut *conn)
                .await?;
        let claim = match row {
            None => {
                sqlx::query("INSERT INTO scheduled_claims (occurrence, claimed_at) VALUES (?1, ?2)")
                    .bind(occurrence)
                    .bind(now.to_rfc3339())
                    .execute(&mut *conn)
                    .await?;
                Claim::Acquired
            }
            Some((_, Some(_))) => Claim::Completed,
            Some((claimed_at, None)) => {
                let stale = chrono::DateTime::parse_from_rfc3339(&claimed_at)
                    .map(|t| t.with_timezone(&chrono::Utc) + stale_after <= now)
                    .unwrap_or(true);
                if stale {
                    sqlx::query("UPDATE scheduled_claims SET claimed_at = ?2 WHERE occurrence = ?1")
                        .bind(occurrence)
                        .bind(now.to_rfc3339())
                        .execute(&mut *conn)
                        .await?;
                    Claim::Acquired
                } else {
                    Claim::Busy
                }
            }
        };
        anyhow::Ok(claim)
    }
    .await;
    match result {
        Ok(c) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(c)
        }
        Err(e) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(e)
        }
    }
}

/// Mark a claimed occurrence completed.
pub async fn complete_occurrence(
    pool: &SqlitePool,
    occurrence: &str,
    run_id: Option<i64>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    sqlx::query("UPDATE scheduled_claims SET completed_at = ?2, run_id = ?3 WHERE occurrence = ?1")
        .bind(occurrence)
        .bind(now.to_rfc3339())
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(())
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
        (c.oversized, "oversized note", "oversized notes"),
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

    /// Options that apply fixes, with a quarantine outside the vault.
    fn apply_opts(quarantine: &Path) -> CheckOptions {
        let mut o = opts(true);
        o.quarantine_dir = Some(quarantine.to_path_buf());
        o
    }

    fn applied(r: &CheckReport) -> Vec<&Finding> {
        r.findings
            .iter()
            .filter(|f| f.fix_action.as_deref().is_some_and(|a| !a.starts_with("not applied")))
            .collect()
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
        let vault = &tmp.path().join("vault");
        let quarantine = tmp.path().join("quarantine");
        fixture(vault);
        let before = fs::metadata(vault.join("6-Slipbox/no-frontmatter.md")).unwrap().modified().unwrap();
        let r = run(vault, &ex(), &apply_opts(&quarantine)).unwrap();

        // Untitled empties moved to the quarantine; a linked placeholder is kept.
        assert!(!vault.join("Untitled.canvas").exists());
        assert!(!vault.join("Untitled 1.md").exists());
        assert!(vault.join("6-Slipbox/placeholder.md").exists());
        let run_dir = fs::read_dir(&quarantine).unwrap().next().unwrap().unwrap().path();
        assert!(run_dir.join("deleted/Untitled.canvas").exists());
        assert!(run_dir.join("deleted/Untitled 1.md").exists());
        // Recovery copy of the note that got `created`.
        assert_eq!(
            fs::read_to_string(run_dir.join("backup/6-Slipbox/no-frontmatter.md")).unwrap(),
            "Just text [[Beta]].\n"
        );

        // `created` added, mtime preserved, invalid YAML left alone.
        let text = fs::read_to_string(vault.join("6-Slipbox/no-frontmatter.md")).unwrap();
        assert!(text.starts_with("---\ncreated: "), "{text}");
        assert!(text.ends_with("---\nJust text [[Beta]].\n"));
        let after = fs::metadata(vault.join("6-Slipbox/no-frontmatter.md")).unwrap().modified().unwrap();
        assert_eq!(before, after);
        assert_eq!(fs::read_to_string(vault.join("6-Slipbox/broken-yaml.md")).unwrap(), "---\ncreated: [oops\n---\ntext\n");

        // Excluded files untouched.
        assert_eq!(fs::read_to_string(vault.join("4-Areas/Homelab/router.md")).unwrap(), "# Router\n[[nowhere]]\n");

        let fixed = applied(&r);
        assert_eq!(r.counts.fixed as usize, fixed.len());
        assert_eq!(fixed.len(), 3, "{fixed:?}");
        // The note still lacks `source`, so its finding stays open.
        let nf = fixed.iter().find(|f| f.path.as_deref() == Some("6-Slipbox/no-frontmatter.md")).unwrap();
        assert!(!nf.fixed);

        // A second pass finds nothing left to fix.
        let r2 = run(vault, &ex(), &apply_opts(&quarantine)).unwrap();
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
        let r = Resolver::new(&paths, &paths);
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

    /// A broken `[[Homelab]]` in an ordinary note must not name the
    /// excluded folder in the report.
    #[test]
    fn folder_hints_never_name_excluded_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path();
        write(vault, "4-Areas/Homelab/router.md", "# Router\n");
        write(vault, "4-Areas/Team/a.md", &format!("{}x\n", fm("2026-01-01", "manual")));
        write(vault, "6-Slipbox/n.md", &format!("{}[[Homelab]] [[Team]]\n", fm("2026-01-01", "manual")));
        let r = run(vault, &ex(), &opts(false)).unwrap();
        let broken = of(&r, KIND_BROKEN_LINK);
        assert_eq!(broken.len(), 2, "{broken:?}");
        let homelab = broken.iter().find(|f| f.detail.contains("[[Homelab]]")).unwrap();
        assert!(homelab.related.is_empty() && !homelab.detail.contains("folder"), "{homelab:?}");
        let team = broken.iter().find(|f| f.detail.contains("[[Team]]")).unwrap();
        assert_eq!(team.related, vec!["4-Areas/Team".to_string()]);
    }

    #[test]
    fn similar_titles_use_parsed_titles() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path();
        let f = fm("2026-01-01", "manual");
        // Different file names, same title.
        write(vault, "6-Slipbox/a1.md", &format!("{f}# Quarterly planning review notes\n"));
        write(vault, "6-Slipbox/b2.md", &format!("{f}# Quarterly planning review notes\n"));
        // Near-identical file names, different titles.
        write(vault, "3-Projects/weekly sync meeting notes.md", &format!("{f}# Hiring pipeline\n"));
        write(vault, "3-Projects/weekly sync meeting notes v2.md", &format!("{f}# Garden irrigation\n"));
        let r = run(vault, &ex(), &opts(false)).unwrap();
        let similar = of(&r, KIND_SIMILAR_TITLE);
        assert_eq!(similar.len(), 1, "{similar:?}");
        assert_eq!(similar[0].related, vec!["6-Slipbox/a1.md".to_string(), "6-Slipbox/b2.md".to_string()]);
    }

    #[test]
    fn escaped_and_angle_bracket_links() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path();
        let f = fm("2026-01-01", "manual");
        write(vault, "6-Slipbox/target note.md", &format!("{f}x\n"));
        write(vault, "6-Slipbox/src.md", &format!("{f}\\[[not a link]] [t](<target note.md>)\n"));
        let r = run(vault, &ex(), &opts(false)).unwrap();
        assert!(of(&r, KIND_BROKEN_LINK).is_empty(), "{:?}", of(&r, KIND_BROKEN_LINK));
        let orphans: Vec<&str> = of(&r, KIND_ORPHAN).iter().map(|f| f.path.as_deref().unwrap()).collect();
        assert!(!orphans.contains(&"6-Slipbox/target note.md"), "{orphans:?}");
    }

    #[test]
    fn oversized_notes_are_counted_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path();
        write(vault, "0-Inbox/huge.md", &"word ".repeat(super::super::MAX_NOTE_BYTES as usize / 5 + 1));
        let r = run(vault, &ex(), &opts(false)).unwrap();
        assert_eq!(r.counts.oversized, 1);
        assert_eq!(r.notes_scanned, 0);
        assert_eq!(of(&r, KIND_OVERSIZED)[0].path.as_deref(), Some("0-Inbox/huge.md"));
    }

    /// `created:` present but empty is reported as empty and never gets a
    /// second `created` key; a CRLF note keeps CRLF.
    #[test]
    fn created_fix_respects_empty_keys_and_line_endings() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = &tmp.path().join("vault");
        let quarantine = tmp.path().join("q");
        write(vault, "6-Slipbox/empty-created.md", "---\ncreated:\nsource: manual\n---\nText.\n");
        write(vault, "6-Slipbox/quoted.md", "---\ncreated: \"\"\nsource: manual\n---\nText.\n");
        write(vault, "6-Slipbox/crlf.md", "---\r\nsource: manual\r\n---\r\nText.\r\n");
        write(vault, "6-Slipbox/crlf-bare.md", "Text.\r\nMore.\r\n");
        let r = run(vault, &ex(), &apply_opts(&quarantine)).unwrap();
        for p in ["6-Slipbox/empty-created.md", "6-Slipbox/quoted.md"] {
            let f = r.findings.iter().find(|f| f.path.as_deref() == Some(p) && f.kind == KIND_FRONTMATTER).unwrap();
            assert_eq!(f.detail, "empty: created");
            assert!(f.fix_action.is_none());
        }
        assert_eq!(fs::read_to_string(vault.join("6-Slipbox/empty-created.md")).unwrap(), "---\ncreated:\nsource: manual\n---\nText.\n");
        let crlf = fs::read_to_string(vault.join("6-Slipbox/crlf.md")).unwrap();
        assert!(crlf.starts_with("---\r\ncreated: ") && crlf.ends_with("\r\nsource: manual\r\n---\r\nText.\r\n"), "{crlf:?}");
        assert!(!crlf.replace("\r\n", "").contains('\n'));
        let bare = fs::read_to_string(vault.join("6-Slipbox/crlf-bare.md")).unwrap();
        assert!(bare.starts_with("---\r\ncreated: ") && bare.ends_with("\r\n---\r\nText.\r\nMore.\r\n"), "{bare:?}");
        // The rewritten note parses, with one created key.
        let n = note::parse("x.md", &crlf);
        assert!(n.frontmatter.has_key("created"));
    }

    fn scanned(vault: &Path, rel: &str) -> VaultFile {
        scan::walk(vault, &ex()).unwrap().files.into_iter().find(|f| f.rel == rel).unwrap()
    }

    /// H4: an empty `Untitled` file that gained content after the scan —
    /// even with the same size and mtime — or that a note links to now,
    /// is not removed.
    #[test]
    fn delete_rechecks_the_file_and_the_links() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = &tmp.path().join("vault");
        let run_dir = tmp.path().join("q/run");

        // Same size and restored mtime, different (non-empty) content.
        write(vault, "Untitled 2.md", "   \n");
        let f = scanned(vault, "Untitled 2.md");
        let mtime = fs::metadata(&f.abs).unwrap().modified().unwrap();
        fs::write(&f.abs, "abc\n").unwrap();
        fs::File::options().write(true).open(&f.abs).unwrap().set_modified(mtime).unwrap();
        assert!(matches!(quarantine_empty(&f, &run_dir).unwrap(), Outcome::Skipped("no longer empty")));
        assert_eq!(fs::read_to_string(&f.abs).unwrap(), "abc\n");

        // Replaced by another file (new inode).
        write(vault, "Untitled 3.md", "");
        let f = scanned(vault, "Untitled 3.md");
        fs::remove_file(&f.abs).unwrap();
        write(vault, "Untitled 3.md", "");
        assert!(matches!(quarantine_empty(&f, &run_dir).unwrap(), Outcome::Skipped(_)));
        assert!(f.abs.exists());

        // Replaced by a symlink.
        write(tmp.path(), "outside.md", "");
        write(vault, "Untitled 4.md", "");
        let f = scanned(vault, "Untitled 4.md");
        fs::remove_file(&f.abs).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("outside.md"), &f.abs).unwrap();
        assert!(matches!(quarantine_empty(&f, &run_dir).unwrap(), Outcome::Skipped(_)));
        assert!(tmp.path().join("outside.md").exists());

        // Linked after the analysis: the apply step re-reads the links.
        let v2 = &tmp.path().join("vault2");
        write(v2, "Untitled 5.md", "");
        let o = apply_opts(&tmp.path().join("q2"));
        let mut a = analyze(v2, &ex(), &o).unwrap();
        assert_eq!(a.deletes.len(), 1);
        write(v2, "6-Slipbox/new.md", &format!("{}[[Untitled 5]]\n", fm("2026-01-01", "manual")));
        apply(&mut a, v2, &ex(), &o).unwrap();
        assert!(v2.join("Untitled 5.md").exists());
        let r = a.into_report(&o);
        let f = r.findings.iter().find(|f| f.kind == KIND_EMPTY_FILE).unwrap();
        assert_eq!(f.fix_action.as_deref(), Some("not applied: a note links to it now"));
        assert_eq!(r.counts.fixed, 0);
    }

    /// H5: the `created` fix never writes through a swapped path or over a
    /// change made after the scan, and keeps the file mode.
    #[test]
    fn created_fix_is_atomic_and_rechecks() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let vault = &tmp.path().join("vault");
        let run_dir = tmp.path().join("q/run");
        let date = chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();

        // Mode kept; recovery copy written; no temp file left.
        write(vault, "6-Slipbox/a.md", "Body.\n");
        fs::set_permissions(vault.join("6-Slipbox/a.md"), fs::Permissions::from_mode(0o640)).unwrap();
        let f = scanned(vault, "6-Slipbox/a.md");
        let n = note::parse(&f.rel, "Body.\n");
        assert!(matches!(add_created(&f, "Body.\n", &n.frontmatter, date, &run_dir).unwrap(), Outcome::Done(_)));
        assert_eq!(fs::read_to_string(&f.abs).unwrap(), "---\ncreated: 2026-01-02\n---\nBody.\n");
        assert_eq!(fs::metadata(&f.abs).unwrap().permissions().mode() & 0o7777, 0o640);
        assert_eq!(fs::read_to_string(run_dir.join("backup/6-Slipbox/a.md")).unwrap(), "Body.\n");
        let leftovers: Vec<_> = fs::read_dir(vault.join("6-Slipbox")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with('.')).collect();
        assert!(leftovers.is_empty());

        // Written after the scan: skipped, the new content survives.
        write(vault, "6-Slipbox/b.md", "Body.\n");
        let f = scanned(vault, "6-Slipbox/b.md");
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&f.abs, "Body, edited on another device.\n").unwrap();
        assert!(matches!(add_created(&f, "Body.\n", &n.frontmatter, date, &run_dir).unwrap(), Outcome::Skipped(_)));
        assert_eq!(fs::read_to_string(&f.abs).unwrap(), "Body, edited on another device.\n");

        // Swapped for a symlink to a file outside the vault: untouched.
        write(tmp.path(), "outside.md", "Body.\n");
        write(vault, "6-Slipbox/c.md", "Body.\n");
        let f = scanned(vault, "6-Slipbox/c.md");
        fs::remove_file(&f.abs).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("outside.md"), &f.abs).unwrap();
        assert!(matches!(add_created(&f, "Body.\n", &n.frontmatter, date, &run_dir).unwrap(), Outcome::Skipped(_)));
        assert_eq!(fs::read_to_string(tmp.path().join("outside.md")).unwrap(), "Body.\n");
        assert!(fs::symlink_metadata(&f.abs).unwrap().file_type().is_symlink());
    }

    #[test]
    fn quarantine_retention() {
        let tmp = tempfile::tempdir().unwrap();
        let q = tmp.path();
        let now = chrono::Utc::now();
        let old = (now - chrono::Duration::days(QUARANTINE_RETENTION_DAYS + 2)).format(RUN_DIR_FORMAT).to_string();
        let new = (now - chrono::Duration::days(1)).format(RUN_DIR_FORMAT).to_string();
        for d in [format!("{old}-1"), format!("{new}-2"), "not-a-run".to_string()] {
            fs::create_dir_all(q.join(d).join("deleted")).unwrap();
        }
        purge_quarantine(q, now);
        assert!(!q.join(format!("{old}-1")).exists());
        assert!(q.join(format!("{new}-2")).exists());
        assert!(q.join("not-a-run").exists());
    }

    /// Two `--scheduled` processes claiming the same occurrence: one wins;
    /// after completion the occurrence stays done; a stale claim is taken over.
    #[tokio::test]
    async fn occurrence_claims() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("check.db");
        let (a, b) = (open_at(&db).await.unwrap(), open_at(&db).await.unwrap());
        let now = chrono::Utc::now();
        let stale = chrono::Duration::minutes(30);
        let (ra, rb) = tokio::join!(
            claim_occurrence(&a, "2026-09-27T20:00:00+00:00", now, stale),
            claim_occurrence(&b, "2026-09-27T20:00:00+00:00", now, stale)
        );
        let mut got = vec![ra.unwrap(), rb.unwrap()];
        got.sort_by_key(|c| format!("{c:?}"));
        assert_eq!(got, vec![Claim::Acquired, Claim::Busy]);
        let later = now + chrono::Duration::minutes(31);
        assert_eq!(claim_occurrence(&a, "2026-09-27T20:00:00+00:00", later, stale).await.unwrap(), Claim::Acquired);
        complete_occurrence(&a, "2026-09-27T20:00:00+00:00", Some(1), later).await.unwrap();
        assert_eq!(
            claim_occurrence(&b, "2026-09-27T20:00:00+00:00", later + stale * 3, stale).await.unwrap(),
            Claim::Completed
        );
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
