//! Shared skill-library access (ADR-008 / ADR-017).
//!
//! Discovery, SKILL.md frontmatter parsing, and the format validator — used
//! by the dashboard `/skills` handler (read) and the skill-gap learner
//! (read + write + validate) so both judge skills identically. Mirrors the
//! lib+bin sharing the reminders crate does.
//!
//! Two storage trees under the workspace root:
//!   - `<workspace>/.nucleus/.claude/skills/<name>/SKILL.md` — operator-personal
//!     (tier "personal"; `.nucleus/` is gitignored)
//!   - `<workspace>/.claude/skills/<name>/SKILL.md` — committed (tier "repo")
//!
//! plus the machine-wide `$HOME/.claude/skills` (tier "global"), which
//! Claude Code loads in every project, and two archive trees the dashboard
//! lists and manages (`SkillTier`, `LibraryRoots`).
//!
//! Claude Code loads `.claude/skills/` from every `--add-dir` directory, so
//! every Nucleus-spawned session gets `--add-dir <workspace>/.nucleus`
//! (see `claude_session::build_claude_args`). The paths are defined only by
//! the functions below. The learner only ever *writes* to the
//! operator-personal tree (Rule 1).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const SKILL_FILE: &str = "SKILL.md";

/// Gitignored operator-private directory under the workspace root. Passed to
/// every Nucleus-spawned session as `--add-dir`, so its `.claude/skills/`
/// tree loads alongside the repo's.
pub const PRIVATE_DIR: &str = ".nucleus";

/// `<workspace_root>/.nucleus` — the operator-private directory.
pub fn private_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(PRIVATE_DIR)
}

/// `<workspace_root>/.nucleus/.claude/skills` — the operator-personal skills
/// tree (tier "personal"). The learner writes here, and its `.archive/` and
/// `.rejected/` sub-directories live here.
pub fn personal_skills_root(workspace_root: &Path) -> PathBuf {
    private_dir(workspace_root).join(".claude").join("skills")
}

/// `<workspace_root>/.claude/skills` — the committed skills tree (tier "repo").
pub fn repo_skills_root(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".claude").join("skills")
}

/// Directory under the personal skills root where archived personal skills
/// live (the learner's curator and the dashboard both move skills here).
pub const ARCHIVE_DIR: &str = ".archive";

/// `<workspace_root>/.nucleus/.claude/skills/.archive` — archived personal
/// skills (tier "personal-archive").
pub fn personal_archive_root(workspace_root: &Path) -> PathBuf {
    personal_skills_root(workspace_root).join(ARCHIVE_DIR)
}

/// `$HOME/.claude/skills` — the machine-wide skills tree (tier "global").
/// Claude Code loads it in every session on the machine, in every project,
/// and a skill there wins over a same-named project or `--add-dir` skill.
/// `None` when HOME is unset or empty.
pub fn global_skills_root() -> Option<PathBuf> {
    home_dir().map(|h| global_skills_root_in(&h))
}

/// `$HOME/.claude/skills-archive` — archived machine-wide skills (tier
/// "global-archive"). Claude Code does not load this directory. `None` when
/// HOME is unset or empty.
pub fn global_archive_root() -> Option<PathBuf> {
    home_dir().map(|h| global_archive_root_in(&h))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn global_skills_root_in(home: &Path) -> PathBuf {
    home.join(".claude").join("skills")
}

fn global_archive_root_in(home: &Path) -> PathBuf {
    home.join(".claude").join("skills-archive")
}

/// Where a skill directory sits. Serialized kebab-case; the TypeScript type
/// is a string union.
///
/// Active tiers (Claude Code loads them):
///   - `personal` — `<workspace>/.nucleus/.claude/skills` (gitignored,
///     loaded into Nucleus sessions through `--add-dir`, ADR-032)
///   - `repo` — `<workspace>/.claude/skills` (committed)
///   - `global` — `$HOME/.claude/skills` (loaded in every project)
///
/// Archive tiers (not loaded):
///   - `personal-archive` — `<workspace>/.nucleus/.claude/skills/.archive`
///   - `global-archive` — `$HOME/.claude/skills-archive`
///
/// When two active tiers hold a skill with the same name, Claude Code loads
/// only one copy. Precedence: `global` > `repo` > `personal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum SkillTier {
    Personal,
    Repo,
    Global,
    PersonalArchive,
    GlobalArchive,
}

impl SkillTier {
    pub const ACTIVE: [SkillTier; 3] = [SkillTier::Global, SkillTier::Repo, SkillTier::Personal];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Repo => "repo",
            Self::Global => "global",
            Self::PersonalArchive => "personal-archive",
            Self::GlobalArchive => "global-archive",
        }
    }

    /// True for the tiers Claude Code loads.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Personal | Self::Repo | Self::Global)
    }

    /// Load precedence among active tiers; higher wins a name collision.
    /// Archive tiers return 0.
    pub fn precedence(self) -> u8 {
        match self {
            Self::Global => 3,
            Self::Repo => 2,
            Self::Personal => 1,
            Self::PersonalArchive | Self::GlobalArchive => 0,
        }
    }

    /// The archive tier of an active tier that has one (`personal`, `global`).
    pub fn archive(self) -> Option<SkillTier> {
        match self {
            Self::Personal => Some(Self::PersonalArchive),
            Self::Global => Some(Self::GlobalArchive),
            _ => None,
        }
    }

    /// The active tier an archive tier restores into.
    pub fn active(self) -> Option<SkillTier> {
        match self {
            Self::PersonalArchive => Some(Self::Personal),
            Self::GlobalArchive => Some(Self::Global),
            _ => None,
        }
    }
}

impl std::fmt::Display for SkillTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A parsed skill, ready for the dashboard API or the learner's library view.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct Skill {
    /// Frontmatter `name`, falling back to the directory name (CC convention).
    pub name: String,
    /// Name of the skill directory inside its tier root. Unique within a
    /// tier; together with `tier` it identifies the skill for the dashboard
    /// actions (move / archive / restore / delete). For archived skills it
    /// may carry a date suffix (`<name>-YYYY-MM-DD`).
    pub dir_name: String,
    pub description: String,
    /// Storage tier; see `SkillTier`.
    pub tier: SkillTier,
    /// Absolute path to the SKILL.md file, spelled through the tier root
    /// (a symlinked skill keeps the symlink path here, not its target). Pass
    /// it unchanged to `GET /skills/api/body`.
    pub path: String,
    /// Raw target of the skill directory when it is a symlink (for example a
    /// relative path into a vendor directory), else null. Symlinked skills
    /// cannot be moved or archived.
    pub symlink_target: Option<String>,
    /// Active tiers only: the tier whose same-named copy Claude Code loads
    /// instead of this one (precedence `global` > `repo` > `personal`). Null
    /// when this copy is the one that loads. Always null for archive tiers.
    pub shadowed_by: Option<SkillTier>,
    /// Other active tiers holding a skill with the same frontmatter name or
    /// directory name, highest precedence first. For an archived skill: the
    /// active tiers that hold its `restore_name` or frontmatter name, so a
    /// non-empty list means restore will be refused.
    pub also_in: Vec<SkillTier>,
    /// Archive tiers only: the directory name the skill gets on restore
    /// (frontmatter `name`, else the directory name without its trailing
    /// `-YYYY-MM-DD[-N]` or `-YYYYMMDDTHHMMSS` suffix). Null for active tiers.
    pub restore_name: Option<String>,
    pub flavor: Option<String>,
    /// ADR-017: "agent" when the learner authored it; None for hand-written.
    pub created_by: Option<String>,
    /// ADR-017: protected from the curator's auto-archive when true. The
    /// dashboard also refuses to archive a pinned skill.
    pub pinned: bool,
    pub mcp_needed: Option<Vec<String>>,
    pub last_used: Option<String>,
    pub last_failure: Option<String>,
    #[ts(type = "number | null")]
    pub failure_count_30d: Option<i64>,
    pub notify_on_failure: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub trigger: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub flavor: Option<String>,
    pub created_by: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, deserialize_with = "string_or_vec")]
    pub mcp_needed: Option<Vec<String>>,
    pub last_used: Option<String>,
    pub last_failure: Option<String>,
    pub failure_count_30d: Option<i64>,
    #[serde(default, deserialize_with = "string_or_vec")]
    pub notify_on_failure: Option<Vec<String>>,
    #[serde(default, deserialize_with = "string_or_vec")]
    pub tags: Option<Vec<String>>,
    pub trigger: Option<String>,
}

/// Frontmatter authors flip between `key: value` and `key: [a, b]` even within
/// one field — accept either. Empty / null reads as None.
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
                    other => out.push(
                        serde_yaml::to_string(&other).unwrap_or_default().trim().to_string(),
                    ),
                }
            }
            Ok(Some(out))
        }
        other => Err(D::Error::custom(format!(
            "expected string or sequence, got {other:?}"
        ))),
    }
}

/// Extract the leading `---`-delimited YAML block. None if absent.
pub fn split_frontmatter(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    Some(&rest[..end])
}

/// Parse SKILL.md frontmatter, strict YAML first then a lenient line parser.
/// Strict gets the full field set; lenient recovers `name`/`description` etc.
/// when a natural-language `: ` in a value trips strict YAML. None = no block.
pub fn parse_frontmatter(content: &str, path: &Path) -> Option<Frontmatter> {
    let yaml = split_frontmatter(content)?;
    match serde_yaml::from_str::<Frontmatter>(yaml) {
        Ok(fm) => Some(fm),
        Err(e) => {
            tracing::warn!(
                "skills: strict YAML failed for {} ({e}); lenient line parse",
                path.display()
            );
            Some(parse_frontmatter_lenient(yaml))
        }
    }
}

fn parse_frontmatter_lenient(yaml: &str) -> Frontmatter {
    let mut fm = Frontmatter::default();
    for line in yaml.lines() {
        if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-') {
            continue;
        }
        let Some(idx) = line.find(": ") else { continue };
        let key = line[..idx].trim();
        let value = strip_quotes(line[idx + 2..].trim()).to_string();
        match key {
            "name" => fm.name = Some(value),
            "description" => fm.description = Some(value),
            "flavor" => fm.flavor = Some(value),
            "created_by" => fm.created_by = Some(value),
            "pinned" => fm.pinned = value == "true",
            "trigger" => fm.trigger = Some(value),
            "last_used" => fm.last_used = Some(value),
            "last_failure" => fm.last_failure = Some(value),
            _ => {}
        }
    }
    fm
}

fn strip_quotes(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Synchronously read every `<dir>/SKILL.md` under `root`, tagged `tier`.
/// Missing root → empty (operator hasn't created the tree). Skips entries
/// whose name starts with a dot (the learner's `.archive/` and `.rejected/`
/// housekeeping dirs) and directories without a SKILL.md (they are not
/// skills). A symlinked skill directory is followed for reading and reported
/// through `symlink_target`. Collision fields (`shadowed_by`, `also_in`) are
/// left empty; `read_library` fills them.
pub fn read_skills(root: &Path, tier: SkillTier) -> Vec<Skill> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for dirent in entries.flatten() {
        let path = dirent.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        // Housekeeping dirs aren't skills.
        if dir_name.is_empty() || dir_name.starts_with('.') {
            continue;
        }
        let skill_md = path.join(SKILL_FILE);
        let content = match std::fs::read_to_string(&skill_md) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let symlink_target = std::fs::symlink_metadata(&path)
            .ok()
            .filter(|m| m.file_type().is_symlink())
            .and_then(|_| std::fs::read_link(&path).ok())
            .map(|t| t.to_string_lossy().into_owned());
        let fm = parse_frontmatter(&content, &skill_md).unwrap_or_default();
        let restore_name = (!tier.is_active())
            .then(|| derive_restore_name(fm.name.as_deref(), &dir_name));
        out.push(Skill {
            name: fm.name.unwrap_or_else(|| dir_name.clone()),
            dir_name,
            description: fm.description.unwrap_or_default(),
            tier,
            path: skill_md.to_string_lossy().into_owned(),
            symlink_target,
            shadowed_by: None,
            also_in: Vec::new(),
            restore_name,
            flavor: fm.flavor,
            created_by: fm.created_by,
            pinned: fm.pinned,
            mcp_needed: fm.mcp_needed,
            last_used: fm.last_used,
            last_failure: fm.last_failure,
            failure_count_30d: fm.failure_count_30d,
            notify_on_failure: fm.notify_on_failure,
            tags: fm.tags,
            trigger: fm.trigger,
        });
    }
    out
}

/// Every skill root the dashboard manages, by tier. The global and
/// global-archive roots are `None` when HOME is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryRoots {
    pub personal: PathBuf,
    pub repo: PathBuf,
    pub global: Option<PathBuf>,
    pub personal_archive: PathBuf,
    pub global_archive: Option<PathBuf>,
}

impl LibraryRoots {
    /// Roots for `workspace_root`, with the global roots under `home`.
    pub fn new(workspace_root: &Path, home: Option<&Path>) -> Self {
        Self {
            personal: personal_skills_root(workspace_root),
            repo: repo_skills_root(workspace_root),
            global: home.map(global_skills_root_in),
            personal_archive: personal_archive_root(workspace_root),
            global_archive: home.map(global_archive_root_in),
        }
    }

    /// Roots for `workspace_root`, with the global roots under `$HOME`.
    pub fn for_workspace(workspace_root: &Path) -> Self {
        Self::new(workspace_root, home_dir().as_deref())
    }

    pub fn root(&self, tier: SkillTier) -> Option<&Path> {
        match tier {
            SkillTier::Personal => Some(&self.personal),
            SkillTier::Repo => Some(&self.repo),
            SkillTier::Global => self.global.as_deref(),
            SkillTier::PersonalArchive => Some(&self.personal_archive),
            SkillTier::GlobalArchive => self.global_archive.as_deref(),
        }
    }

    /// Every known `(tier, root)` pair: active tiers by precedence, then the
    /// archive tiers.
    pub fn all(&self) -> Vec<(SkillTier, &Path)> {
        [
            SkillTier::Global,
            SkillTier::Repo,
            SkillTier::Personal,
            SkillTier::PersonalArchive,
            SkillTier::GlobalArchive,
        ]
        .into_iter()
        .filter_map(|t| self.root(t).map(|r| (t, r)))
        .collect()
    }
}

/// The whole skill library, grouped for the dashboard. Each group is sorted
/// by `name`, then `dir_name`.
#[derive(Debug, Clone, Default, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct SkillLibrary {
    /// Tier `personal`: `<workspace>/.nucleus/.claude/skills`.
    pub personal: Vec<Skill>,
    /// Tier `repo`: `<workspace>/.claude/skills` (committed; read-only in the
    /// dashboard).
    pub repo: Vec<Skill>,
    /// Tier `global`: `$HOME/.claude/skills`. Empty when HOME is unknown.
    pub global: Vec<Skill>,
    /// Tiers `personal-archive` and `global-archive`, told apart by `tier`.
    pub archived: Vec<Skill>,
}

impl SkillLibrary {
    /// Every active skill (personal, repo, global).
    pub fn active(&self) -> impl Iterator<Item = &Skill> {
        self.personal.iter().chain(&self.repo).chain(&self.global)
    }

    pub fn all(&self) -> impl Iterator<Item = &Skill> {
        self.active().chain(&self.archived)
    }

    /// The skill in `tier` whose directory is `dir_name`.
    pub fn find(&self, tier: SkillTier, dir_name: &str) -> Option<&Skill> {
        self.all().find(|s| s.tier == tier && s.dir_name == dir_name)
    }

    /// Active tiers (highest precedence first) holding a skill whose name or
    /// directory name is in `keys`, ignoring the entry `(tier, dir_name)`
    /// given in `except`.
    pub fn active_tiers_named(
        &self,
        keys: &[&str],
        except: Option<(SkillTier, &str)>,
    ) -> Vec<SkillTier> {
        let mut tiers: Vec<SkillTier> = self
            .active()
            .filter(|s| except.is_none_or(|(t, d)| !(s.tier == t && s.dir_name == d)))
            .filter(|s| keys.iter().any(|k| s.name == *k || s.dir_name == *k))
            .map(|s| s.tier)
            .collect();
        tiers.sort_by_key(|t| std::cmp::Reverse(t.precedence()));
        tiers.dedup();
        tiers
    }
}

/// Read every tier in `roots` and fill the collision fields.
pub fn read_library(roots: &LibraryRoots) -> SkillLibrary {
    let mut lib = SkillLibrary::default();
    for (tier, root) in roots.all() {
        let skills = read_skills(root, tier);
        match tier {
            SkillTier::Personal => lib.personal = skills,
            SkillTier::Repo => lib.repo = skills,
            SkillTier::Global => lib.global = skills,
            SkillTier::PersonalArchive | SkillTier::GlobalArchive => lib.archived.extend(skills),
        }
    }
    annotate_collisions(&mut lib);
    for group in [&mut lib.personal, &mut lib.repo, &mut lib.global, &mut lib.archived] {
        group.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.dir_name.cmp(&b.dir_name)));
    }
    lib
}

/// Fill `also_in` and `shadowed_by`. Two skills collide when their
/// frontmatter names or directory names match. Only active tiers count as
/// holders; an archived skill is compared by its `restore_name` and name.
fn annotate_collisions(lib: &mut SkillLibrary) {
    let snapshot = lib.clone();
    let annotate = |s: &mut Skill| {
        if s.tier.is_active() {
            let keys = [s.name.as_str(), s.dir_name.as_str()];
            let others = snapshot.active_tiers_named(&keys, Some((s.tier, &s.dir_name)));
            s.also_in = others.into_iter().filter(|t| *t != s.tier).collect();
            s.shadowed_by = s
                .also_in
                .iter()
                .copied()
                .find(|t| t.precedence() > s.tier.precedence());
        } else {
            let restore = s.restore_name.clone().unwrap_or_else(|| s.dir_name.clone());
            let keys = [s.name.as_str(), restore.as_str()];
            s.also_in = snapshot.active_tiers_named(&keys, None);
            s.shadowed_by = None;
        }
    };
    for group in [&mut lib.personal, &mut lib.repo, &mut lib.global, &mut lib.archived] {
        group.iter_mut().for_each(annotate);
    }
}

/// Check that `name` is a single safe directory name: only `[A-Za-z0-9._-]`,
/// 1–128 characters, no leading dot, no `..`. Rejects anything that could
/// address a path outside the root it is joined to.
pub fn validate_dir_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("skill directory name is empty".into());
    }
    if name.len() > 128 {
        return Err("skill directory name is longer than 128 characters".into());
    }
    if name.starts_with('.') {
        return Err(format!("skill directory name {name:?} starts with a dot"));
    }
    if name.contains("..") {
        return Err(format!("skill directory name {name:?} contains `..`"));
    }
    if let Some(c) = name.chars().find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))) {
        return Err(format!(
            "skill directory name {name:?} contains {c:?}; allowed characters are A-Z a-z 0-9 . _ -"
        ));
    }
    Ok(())
}

/// `dir_name` without a trailing archive suffix: `-YYYY-MM-DD`,
/// `-YYYY-MM-DD-N` (dashboard archive), or `-YYYYMMDDTHHMMSS` (the learner's
/// curator). Unchanged when there is no such suffix.
pub fn strip_archive_suffix(dir_name: &str) -> &str {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^(.+?)-(?:\d{4}-\d{2}-\d{2}(?:-\d+)?|\d{8}T\d{6})$").expect("valid regex")
    });
    re.captures(dir_name)
        .and_then(|c| c.get(1))
        .map_or(dir_name, |m| m.as_str())
}

/// The directory name an archived skill gets on restore: its frontmatter
/// `name` when that is a valid directory name, else `dir_name` without its
/// archive suffix.
pub fn derive_restore_name(frontmatter_name: Option<&str>, dir_name: &str) -> String {
    match frontmatter_name.map(str::trim) {
        Some(n) if validate_dir_name(n).is_ok() => n.to_string(),
        _ => strip_archive_suffix(dir_name).to_string(),
    }
}

/// The required headings every SKILL.md must carry (Rule 11 / ADR-008). The
/// `# Failure modes` one is the load-bearing check — an empty/absent one
/// signals the skill wasn't thought through.
const REQUIRED_SECTIONS: &[&str] = &["when to invoke", "steps", "failure modes"];

/// Validate a SKILL.md against the contract the learner's autonomous writes
/// must meet — the format gate (ADR-017). Returns the list of problems;
/// empty = valid. This is what makes "direct writes" as reliable as
/// skill-creator: a non-conforming write is caught mechanically.
pub fn validate(content: &str) -> Vec<String> {
    let mut issues = Vec::new();

    let Some(yaml) = split_frontmatter(content) else {
        issues.push("missing `---` YAML frontmatter block".into());
        return issues; // nothing else parseable without it
    };
    let fm = match serde_yaml::from_str::<Frontmatter>(yaml) {
        Ok(fm) => fm,
        Err(_) => parse_frontmatter_lenient(yaml),
    };
    if fm.description.as_deref().unwrap_or("").trim().is_empty() {
        issues.push("frontmatter `description` is required and must be non-empty".into());
    }
    if fm.flavor.as_deref().unwrap_or("").trim().is_empty() {
        issues.push("frontmatter `flavor` is required (recipe | learned)".into());
    }

    // Required body sections — match `#`/`##` headings case-insensitively.
    let body_lower = content.to_lowercase();
    for section in REQUIRED_SECTIONS {
        let h1 = format!("# {section}");
        let h2 = format!("## {section}");
        let present = body_lower
            .lines()
            .any(|l| l.trim_start().starts_with(&h1) || l.trim_start().starts_with(&h2));
        if !present {
            issues.push(format!("missing required section heading `# {section}`"));
        }
    }
    issues
}

/// Fire a detached on-the-fly skill review (ADR-017) for a conversation that
/// just crossed the nudge interval. Best-effort and fully decoupled: it spawns
/// `nucleus skill-gap-learner review` and returns immediately, so it never
/// blocks the caller's reply or fails it. A no-op if the binary isn't built
/// yet. The conversational agents call this when `AskResult.review_due`.
///
/// Deliberately a subprocess rather than a library call even though both halves
/// now live in one binary (ADR-030): the review must outlive the reply it was
/// triggered by, and it must not be able to fail the conversation turn.
pub fn fire_skill_review(workspace_root: &Path, venue: &str, chat_key: &str, transcript_path: &str) {
    use std::process::{Command, Stdio};
    let release = workspace_root.join("target/release/nucleus");
    let bin = if release.exists() {
        release
    } else {
        workspace_root.join("target/debug/nucleus")
    };
    if !bin.exists() {
        return;
    }
    let _ = Command::new(bin)
        .current_dir(workspace_root)
        .args([
            "skill-gap-learner",
            "review",
            "--transcript",
            transcript_path,
            "--venue",
            venue,
            "--chat-key",
            chat_key,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_roots_live_under_the_workspace() {
        let ws = Path::new("/ws");
        assert_eq!(private_dir(ws), PathBuf::from("/ws/.nucleus"));
        assert_eq!(personal_skills_root(ws), PathBuf::from("/ws/.nucleus/.claude/skills"));
        assert_eq!(repo_skills_root(ws), PathBuf::from("/ws/.claude/skills"));
        assert_eq!(personal_archive_root(ws), PathBuf::from("/ws/.nucleus/.claude/skills/.archive"));
        let roots = LibraryRoots::new(ws, Some(Path::new("/h")));
        assert_eq!(roots.global.as_deref(), Some(Path::new("/h/.claude/skills")));
        assert_eq!(roots.global_archive.as_deref(), Some(Path::new("/h/.claude/skills-archive")));
        let no_home = LibraryRoots::new(ws, None);
        assert!(no_home.global.is_none() && no_home.global_archive.is_none());
        assert_eq!(
            no_home.all().iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            vec![SkillTier::Repo, SkillTier::Personal, SkillTier::PersonalArchive]
        );
    }

    fn put(root: &Path, dir: &str, name: Option<&str>) {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        let name_line = name.map(|n| format!("name: {n}\n")).unwrap_or_default();
        std::fs::write(d.join(SKILL_FILE), format!("---\n{name_line}description: d\n---\nbody\n")).unwrap();
    }

    #[test]
    fn library_lists_every_tier_and_marks_collisions_and_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let home = tmp.path().join("home");
        let roots = LibraryRoots::new(&ws, Some(&home));
        let global = roots.global.clone().unwrap();

        put(&roots.personal, "alpha", None);
        put(&roots.personal, "beta-dir", Some("beta")); // collides with repo by name
        put(&roots.repo, "beta", None);
        put(&roots.repo, "gamma", None);
        put(&global, "gamma", None); // global shadows repo
        put(&global, "alpha", None); // global shadows personal
        // a vendor dir reached through a relative symlink
        put(&home.join("vendor"), "linked", None);
        std::os::unix::fs::symlink("../../vendor/linked", global.join("linked")).unwrap();
        // not a skill: no SKILL.md at the top level
        std::fs::create_dir_all(global.join("bundle").join("inner")).unwrap();
        put(&roots.personal_archive, "old-2026-08-24", Some("old"));
        put(&roots.personal_archive, "gamma-2026-01-02-3", None);
        put(roots.global_archive.as_ref().unwrap(), "retired", None);

        let lib = read_library(&roots);
        let ids = |v: &[Skill]| v.iter().map(|s| s.dir_name.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&lib.personal), vec!["alpha", "beta-dir"]);
        assert_eq!(ids(&lib.repo), vec!["beta", "gamma"]);
        assert_eq!(ids(&lib.global), vec!["alpha", "gamma", "linked"]);
        assert_eq!(lib.archived.len(), 3);

        let p_alpha = lib.find(SkillTier::Personal, "alpha").unwrap();
        assert_eq!(p_alpha.shadowed_by, Some(SkillTier::Global));
        assert_eq!(p_alpha.also_in, vec![SkillTier::Global]);
        let g_alpha = lib.find(SkillTier::Global, "alpha").unwrap();
        assert_eq!(g_alpha.shadowed_by, None);
        assert_eq!(g_alpha.also_in, vec![SkillTier::Personal]);
        let p_beta = lib.find(SkillTier::Personal, "beta-dir").unwrap();
        assert_eq!(p_beta.shadowed_by, Some(SkillTier::Repo));
        let r_gamma = lib.find(SkillTier::Repo, "gamma").unwrap();
        assert_eq!(r_gamma.shadowed_by, Some(SkillTier::Global));
        assert_eq!(lib.find(SkillTier::Repo, "beta").unwrap().shadowed_by, None);

        let linked = lib.find(SkillTier::Global, "linked").unwrap();
        assert_eq!(linked.symlink_target.as_deref(), Some("../../vendor/linked"));
        assert!(linked.path.starts_with(global.to_str().unwrap()));
        assert!(lib.find(SkillTier::Global, "alpha").unwrap().symlink_target.is_none());

        let old = lib.find(SkillTier::PersonalArchive, "old-2026-08-24").unwrap();
        assert_eq!(old.restore_name.as_deref(), Some("old"));
        assert!(old.also_in.is_empty() && old.shadowed_by.is_none());
        let g = lib.find(SkillTier::PersonalArchive, "gamma-2026-01-02-3").unwrap();
        assert_eq!(g.restore_name.as_deref(), Some("gamma"));
        assert_eq!(g.also_in, vec![SkillTier::Global, SkillTier::Repo]);
        assert!(lib.find(SkillTier::Personal, "alpha").unwrap().restore_name.is_none());
        assert_eq!(
            lib.find(SkillTier::GlobalArchive, "retired").unwrap().tier.to_string(),
            "global-archive"
        );
    }

    #[test]
    fn dir_name_validation_rejects_traversal() {
        for ok in ["a", "my-skill", "x.y_z-1", "A9"] {
            assert!(validate_dir_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".", "..", ".hidden", "a/b", "../x", "a..b", "a b", "a\\b", "é", "x\0"] {
            assert!(validate_dir_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_dir_name(&"a".repeat(129)).is_err());
    }

    #[test]
    fn restore_name_strips_archive_suffixes() {
        assert_eq!(strip_archive_suffix("foo-2026-08-24"), "foo");
        assert_eq!(strip_archive_suffix("foo-bar-2026-08-24-3"), "foo-bar");
        assert_eq!(strip_archive_suffix("foo-20260824T101112"), "foo");
        assert_eq!(strip_archive_suffix("foo-2026"), "foo-2026");
        assert_eq!(strip_archive_suffix("plain"), "plain");
        assert_eq!(derive_restore_name(Some("real"), "x-2026-01-01"), "real");
        assert_eq!(derive_restore_name(Some("has space"), "x-2026-01-01"), "x");
        assert_eq!(derive_restore_name(None, "x-2026-01-01-2"), "x");
    }

    const GOOD: &str = "---\nname: x\ndescription: does a thing\nflavor: learned\ncreated_by: agent\n---\n\n# When to invoke\nwhen y\n\n# Steps\n1. a\n\n# Failure modes\n- boom\n";

    #[test]
    fn validate_accepts_a_well_formed_skill() {
        assert!(validate(GOOD).is_empty(), "{:?}", validate(GOOD));
    }

    #[test]
    fn validate_flags_missing_failure_modes() {
        let no_fail = "---\ndescription: d\nflavor: learned\n---\n\n# When to invoke\nx\n\n# Steps\n1\n";
        let issues = validate(no_fail);
        assert!(issues.iter().any(|i| i.contains("failure modes")), "{issues:?}");
    }

    #[test]
    fn validate_flags_missing_frontmatter_and_description() {
        assert!(validate("# Steps\nno frontmatter").iter().any(|i| i.contains("frontmatter")));
        let no_desc = "---\nflavor: learned\n---\n\n# When to invoke\nx\n# Steps\n1\n# Failure modes\n-z\n";
        assert!(validate(no_desc).iter().any(|i| i.contains("description")));
    }

    #[test]
    fn parses_created_by_and_pinned() {
        let fm = parse_frontmatter(
            "---\ndescription: d\nflavor: learned\ncreated_by: agent\npinned: true\n---\nbody\n",
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(fm.created_by.as_deref(), Some("agent"));
        assert!(fm.pinned);
    }

    #[test]
    fn lenient_parse_recovers_description_with_colon() {
        // strict YAML trips on the bare `: ` in the value; lenient recovers it.
        let fm = parse_frontmatter(
            "---\nname: x\ndescription: Workspace arg: A or B\nflavor: learned\n---\nbody\n",
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(fm.flavor.as_deref(), Some("learned"));
    }
}
