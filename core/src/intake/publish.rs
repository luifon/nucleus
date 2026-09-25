//! What Nucleus publishes on GitHub for an item (ADR-036): the draft pull
//! request's title and body, the issue comment, and the secret scan every
//! published text and every pushed diff passes first.
//!
//! The pull request body is built from code-owned fields only: the issue
//! link, the branch, the changed files, the test command and its result,
//! and the footer. No model text and no raw test output go into it. The
//! one model-written text Nucleus publishes, the summary in the proposed
//! issue comment, is cut to a fixed length and escaped so it cannot mention
//! anyone, link, embed an image, carry HTML or break out of its paragraph.
//!
//! Before a push, the diff to be pushed and the pull request's title and
//! body go through [`SecretGuard`]; before the comment is posted, the
//! comment does. The production guard runs the repository's own
//! `tools/check-secrets.sh` from the Nucleus workspace root (`.env` values,
//! the `.claude/secret-strings` denylist, personal-information patterns,
//! home paths, private skill names) and the credential shapes of
//! `secret_filter`. A hit, or a guard that cannot run, blocks the step:
//! the item moves to `blocked` with the finding categories, never the
//! matched values.

use super::store::Item;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The result of a secret scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    /// Finding categories (for example `pii-email`, `env-value`); never the
    /// matched text.
    Hit(Vec<String>),
}

/// Scans text that is about to be published.
#[async_trait::async_trait]
pub trait SecretGuard: Send + Sync {
    async fn scan(&self, text: &str) -> Verdict;
}

/// `tools/check-secrets.sh` of the Nucleus workspace, plus the credential
/// shapes of [`crate::secret_filter::CredentialRules`].
pub struct ScriptGuard {
    pub workspace_root: PathBuf,
}

/// The category of one finding line of `check-secrets.sh` (`    - value:…`,
/// `    - denylist/skill:…`, `    - pii-email`); the matched text after
/// the colon is dropped.
fn category(line: &str) -> Option<String> {
    let item = line.trim_start().strip_prefix("- ")?.trim();
    let head = item.split(':').next().unwrap_or(item).trim();
    Some(match head {
        "value" => "env-value".to_string(),
        "denylist/skill" => "denylist-or-private-skill".to_string(),
        other if other.starts_with("pii-") => other.to_string(),
        _ => "other".to_string(),
    })
}

#[async_trait::async_trait]
impl SecretGuard for ScriptGuard {
    async fn scan(&self, text: &str) -> Verdict {
        let mut found: Vec<String> = crate::secret_filter::CredentialRules::from_workspace(&self.workspace_root)
            .redact(text)
            .hits
            .into_iter()
            .map(|k| format!("credential-{k}"))
            .collect();
        match run_script(&self.workspace_root, text).await {
            Ok(mut cats) => found.append(&mut cats),
            Err(e) => {
                tracing::warn!(err = %format!("{e:#}"), "intake: the secret guard could not run");
                found.push("guard-unavailable".into());
            }
        }
        found.sort();
        found.dedup();
        if found.is_empty() {
            Verdict::Clean
        } else {
            Verdict::Hit(found)
        }
    }
}

/// Run the guard script with `text` on stdin. Exit 0: no findings; exit 2:
/// the categories it printed; anything else is an error.
async fn run_script(ws: &Path, text: &str) -> Result<Vec<String>> {
    use tokio::io::AsyncWriteExt;
    let script = ws.join("tools/check-secrets.sh");
    if !script.is_file() {
        anyhow::bail!("tools/check-secrets.sh is missing");
    }
    let mut child = tokio::process::Command::new("bash")
        .arg(&script)
        .current_dir(ws)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting the secret guard")?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let input = text.to_string();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(input.as_bytes()).await;
        drop(stdin);
    });
    let out = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
        .await
        .context("the secret guard did not finish within 120 s")??;
    let _ = writer.await;
    match out.status.code() {
        Some(0) => Ok(vec![]),
        Some(2) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let mut cats: Vec<String> = stderr.lines().filter_map(category).collect();
            if cats.is_empty() {
                cats.push("other".into());
            }
            Ok(cats)
        }
        code => anyhow::bail!("the secret guard exited with {code:?}"),
    }
}

/// Text on one line without control characters, `@` replaced by the
/// full-width `＠` (no mention), at most `max` characters.
pub fn plain_line(s: &str, max: usize) -> String {
    let one: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('@', "＠");
    super::clip(&one, max)
}

/// A file path as a Markdown code span: no backticks, no control
/// characters, at most 200 characters. Inside a code span GitHub renders
/// no mention, link, image or HTML.
fn code_span(path: &str) -> String {
    let p: String = path.chars().map(|c| if c.is_control() || c == '`' { '?' } else { c }).collect();
    format!("`{}`", super::clip(&p, 200))
}

/// A model-written summary for public text: one paragraph, at most `max`
/// characters, Markdown and HTML characters escaped, no mention, no URL.
pub fn escape_summary(s: &str, max: usize) -> String {
    let one = plain_line(s, max);
    let one = one.replace("://", "[:]//").replace("www.", "www[.]");
    let mut out = String::with_capacity(one.len());
    for c in one.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '!' | '#' | '|' | '~' | '{' | '}' | '+' | '-' | '=' | ':' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Most changed files listed in the pull request body.
const MAX_FILES: usize = 100;

/// The draft pull request's title: `Nucleus #<item>: <issue title>`.
pub fn pr_title(item: &Item) -> String {
    format!("Nucleus #{}: {}", item.id, plain_line(item.rev_title.as_deref().unwrap_or(&item.title), 150))
}

/// Everything the pull request body is built from.
pub struct PrFacts<'a> {
    pub item: &'a Item,
    /// `Closes #12`, `Refs owner/name#12` or `Source: …`.
    pub link: String,
    pub branch: &'a str,
    pub files: &'a [String],
    pub test_command: Option<&'a str>,
}

/// The draft pull request's body, from code-owned fields only.
pub fn pr_body(f: &PrFacts<'_>) -> String {
    let mut files = String::new();
    for p in f.files.iter().take(MAX_FILES) {
        files.push_str(&format!("- {}\n", code_span(p)));
    }
    if f.files.len() > MAX_FILES {
        files.push_str(&format!("- and {} more\n", f.files.len() - MAX_FILES));
    }
    if f.files.is_empty() {
        files.push_str("- none\n");
    }
    let tests = match (f.item.tests_status.as_deref(), f.test_command) {
        (Some(s), Some(cmd)) if s != "not_run" => {
            format!("{} — {s} (run by Nucleus after the agent finished)", code_span(cmd))
        }
        _ => "not run (no test command is configured)".to_string(),
    };
    format!(
        "{link}\n\n**Branch:** {branch}\n\n**Changed files ({count}):**\n{files}\n**Tests:** {tests}\n\n---\n\
         Draft opened by the Nucleus issue pipeline (item #{n}). Review before merging; Nucleus never merges.\n\n\
         🤖 Generated with [Claude Code](https://claude.com/claude-code)\n\n<!-- {marker}item-{n} -->",
        link = f.link,
        branch = code_span(f.branch),
        count = f.files.len(),
        n = f.item.id,
        marker = super::github::COMMENT_MARKER_PREFIX,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_categories_drop_the_matched_text() {
        assert_eq!(category("    - value:s3cr3t-token").as_deref(), Some("env-value"));
        assert_eq!(category("    - denylist/skill:somename").as_deref(), Some("denylist-or-private-skill"));
        assert_eq!(category("    - pii-email").as_deref(), Some("pii-email"));
        assert_eq!(category("✖ possible personal information"), None);
    }

    #[test]
    fn summaries_cannot_mention_link_or_break_out() {
        let s = escape_summary("Hi @octocat see [x](https://evil.example/a) ![i](u) <img src=x> ```\n# Title\nwww.example.org", 500);
        assert!(!s.contains('@') && !s.contains("https://") && !s.contains("www.e"), "{s}");
        assert!(!s.contains("<img") && s.contains("&lt;img"), "{s}");
        assert!(!s.contains("](") && !s.contains("```") && !s.contains('\n'), "{s}");
        assert!(s.contains("\\[x\\]") && s.contains("\\#"), "{s}");
        assert!(escape_summary(&"a".repeat(2000), 600).chars().count() <= 601);
    }

    #[test]
    fn pr_text_is_code_owned() {
        let mut item = crate::intake::briefs::tests::test_item();
        item.rev_title = Some("Fix @someone's typo\nsecond line".into());
        item.impl_summary = Some("MODEL-SUMMARY".into());
        item.tests_status = Some("passed".into());
        item.tests_output = Some("RAW-TEST-OUTPUT".into());
        let title = pr_title(&item);
        assert_eq!(title, "Nucleus #1: Fix ＠someone's typo second line");
        let files = vec!["src/a.rs".to_string(), "we`ird\n.md".to_string()];
        let body = pr_body(&PrFacts { item: &item, link: "Closes #3".into(), branch: "nucleus/item-1-fix", files: &files, test_command: Some("cargo test") });
        assert!(body.starts_with("Closes #3\n"), "{body}");
        assert!(body.contains("- `src/a.rs`") && body.contains("- `we?ird?.md`"), "{body}");
        assert!(body.contains("`cargo test` — passed"), "{body}");
        assert!(body.contains("Generated with [Claude Code](https://claude.com/claude-code)"));
        assert!(!body.contains("MODEL-SUMMARY") && !body.contains("RAW-TEST-OUTPUT"), "{body}");
    }
}
