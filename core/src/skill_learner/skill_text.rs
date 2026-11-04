//! The text of a SKILL.md as a patch sees it (ADR-038): its heading outline,
//! the text of one section, and edits applied to it.
//!
//! A patch suggestion is written as a list of edits, not as a whole new
//! file, so a skill of any size can be patched within the review prompt's
//! size limit. Code applies the edits to the skill's current text; the
//! result is the full draft that [`super::suggestions::prepare_draft`]
//! checks and the store keeps.
//!
//! All functions work on [`normalize`]d text: no byte order mark, `\n` line
//! breaks, a final line break.

use serde::{Deserialize, Serialize};

/// The pseudo-heading of the text before the first heading (the frontmatter
/// block and anything after it up to the first heading).
pub const FRONTMATTER_SECTION: &str = "(frontmatter)";
/// Deepest heading level that starts a section in the outline (`###`).
pub const MAX_OUTLINE_LEVEL: usize = 3;
/// Most edits in one patch.
pub const MAX_EDITS: usize = 40;

/// `text` without a byte order mark, with `\n` line breaks and a final line
/// break (an empty text stays empty).
pub fn normalize(text: &str) -> String {
    let t = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    if t.is_empty() || t.ends_with('\n') { t } else { format!("{t}\n") }
}

/// One edit of a patch. Exactly one of `old` and `append_to_section` is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEdit {
    /// Text of the skill to replace. Must occur exactly once in the text as
    /// it stands after the earlier edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// A heading line of the skill: `new` is added at the end of that
    /// section, after its last non-blank line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_to_section: Option<String>,
    /// The replacement, or the lines to add. Empty with `old` deletes it.
    pub new: String,
}

/// One section of the outline: a heading line (or [`FRONTMATTER_SECTION`])
/// and the byte range of its text, from the heading line through the line
/// before the next heading of the same or a higher level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// 0 for the frontmatter section, else the number of `#`.
    pub level: usize,
    /// The heading line without its line break and trailing spaces.
    pub heading: String,
    pub start: usize,
    pub end: usize,
}

impl Section {
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// The heading level of `line` (1 to [`MAX_OUTLINE_LEVEL`]), when it is an
/// ATX heading: up to three spaces, `#`s, then a space or the line end.
fn heading_level(line: &str) -> Option<usize> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    if hashes == 0 || hashes > MAX_OUTLINE_LEVEL {
        return None;
    }
    let after = &rest[hashes..];
    (after.is_empty() || after.starts_with(' ') || after.starts_with('\t')).then_some(hashes)
}

/// A code fence opening or closing line: its character and length.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    let c = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = t.len() - t.trim_start_matches(c).len();
    (n >= 3).then_some((c, n))
}

/// The byte offset where the frontmatter block ends (after its closing
/// `---` line), or 0 when the text has none.
fn frontmatter_end(text: &str) -> usize {
    let Some(rest) = text.strip_prefix("---\n") else { return 0 };
    match rest.find("\n---\n") {
        Some(i) => 4 + i + 5,
        None if rest.ends_with("\n---") => text.len(),
        None => 0,
    }
}

/// Every heading of `text` up to [`MAX_OUTLINE_LEVEL`] outside the
/// frontmatter and fenced code blocks, as sections, in text order. The
/// first entry is [`FRONTMATTER_SECTION`] when there is text before the
/// first heading.
pub fn outline(text: &str) -> Vec<Section> {
    let mut heads: Vec<(usize, usize, String)> = Vec::new();
    let mut pos = frontmatter_end(text);
    let mut fence: Option<(char, usize)> = None;
    for line in text[pos..].split_inclusive('\n') {
        let bare = line.trim_end_matches('\n');
        match (fence, fence_marker(bare)) {
            (None, Some(m)) => fence = Some(m),
            (Some((c, n)), Some((c2, n2))) if c == c2 && n2 >= n && bare.trim().trim_start_matches(c).is_empty() => {
                fence = None
            }
            (None, None) => {
                if let Some(level) = heading_level(bare) {
                    heads.push((level, pos, bare.trim_end().to_string()));
                }
            }
            _ => {}
        }
        pos += line.len();
    }
    let mut out = Vec::new();
    let first = heads.first().map(|h| h.1).unwrap_or(text.len());
    if first > 0 {
        out.push(Section { level: 0, heading: FRONTMATTER_SECTION.to_string(), start: 0, end: first });
    }
    for (i, (level, start, heading)) in heads.iter().enumerate() {
        let end = heads[i + 1..].iter().find(|h| h.0 <= *level).map(|h| h.1).unwrap_or(text.len());
        out.push(Section { level: *level, heading: heading.clone(), start: *start, end });
    }
    out
}

fn same_heading(a: &str, b: &str) -> bool {
    a.trim() == b.trim()
}

/// The sections of `text` whose heading line is `heading` (spaces at either
/// end aside). Usually one; a heading line can repeat.
pub fn sections<'a>(text: &'a str, heading: &str) -> Vec<(&'a str, Section)> {
    outline(text).into_iter().filter(|s| same_heading(&s.heading, heading)).map(|s| (&text[s.start..s.end], s)).collect()
}

/// The deepest section that contains byte `offset`, for showing the
/// context of a quote the reviewer was not shown.
pub fn section_at(text: &str, offset: usize) -> Option<Section> {
    outline(text).into_iter().filter(|s| s.start <= offset && offset < s.end).max_by_key(|s| s.level)
}

/// Where `needle` occurs in `hay`: the first offset and whether it occurs
/// again (overlapping occurrences count).
fn occurrences(hay: &str, needle: &str) -> (Option<usize>, bool) {
    let Some(first) = hay.find(needle) else { return (None, false) };
    let step = needle.chars().next().map(char::len_utf8).unwrap_or(1);
    (Some(first), hay[first + step..].contains(needle))
}

/// Apply `edits` in order to `text` (normalized first). Each `old` must
/// occur exactly once in the text as it stands after the earlier edits;
/// `append_to_section` must name exactly one heading line. Returns the new
/// text, or every problem found: an edit that does not apply refuses the
/// whole patch.
pub fn apply_edits(text: &str, edits: &[SkillEdit]) -> Result<String, Vec<String>> {
    let mut problems = Vec::new();
    if edits.is_empty() {
        return Err(vec!["the patch has no edits".into()]);
    }
    if edits.len() > MAX_EDITS {
        return Err(vec![format!("the patch has {} edits; the limit is {MAX_EDITS}", edits.len())]);
    }
    let mut cur = normalize(text);
    for (i, e) in edits.iter().enumerate() {
        let n = i + 1;
        let new = e.new.replace("\r\n", "\n");
        match (&e.old, &e.append_to_section) {
            (Some(_), Some(_)) => problems.push(format!("edit {n}: set either `old` or `append_to_section`, not both")),
            (None, None) => problems.push(format!("edit {n}: set `old` (text to replace) or `append_to_section` (a heading line)")),
            (Some(old), None) => {
                let old = old.replace("\r\n", "\n");
                if old.is_empty() {
                    problems.push(format!(
                        "edit {n}: `old` is empty; quote the text to replace, or use `append_to_section` to add lines at the end of a section"
                    ));
                    continue;
                }
                if old == new {
                    problems.push(format!("edit {n}: `old` and `new` are the same text; the edit changes nothing"));
                    continue;
                }
                match occurrences(&cur, &old) {
                    (None, _) => problems.push(format!(
                        "edit {n}: `old` does not occur in the skill text{}; copy it exactly (same characters, spaces and line breaks)",
                        if i > 0 { " as it stands after the earlier edits" } else { "" }
                    )),
                    (Some(_), true) => problems.push(format!(
                        "edit {n}: `old` occurs more than once; quote more of the surrounding text so it occurs exactly once"
                    )),
                    (Some(at), false) => cur.replace_range(at..at + old.len(), &new),
                }
            }
            (None, Some(heading)) => {
                if new.trim().is_empty() {
                    problems.push(format!("edit {n}: `new` is empty; there is nothing to add"));
                    continue;
                }
                if same_heading(heading, FRONTMATTER_SECTION) {
                    problems.push(format!(
                        "edit {n}: `append_to_section` takes a heading line, not {FRONTMATTER_SECTION}; edit the frontmatter with `old`"
                    ));
                    continue;
                }
                let found = sections(&cur, heading);
                match found.as_slice() {
                    [] => problems.push(format!(
                        "edit {n}: there is no heading line {heading:?}; copy a heading line from the outline exactly"
                    )),
                    [(body, s)] => {
                        // After the section's last non-blank line, so the
                        // blank lines before the next heading stay after it.
                        let kept = body.trim_end_matches(['\n', ' ', '\t']).len();
                        let at = s.start + body[kept..].find('\n').map(|j| kept + j + 1).unwrap_or(body.len());
                        let mut add = new.trim_matches('\n').to_string();
                        add.push('\n');
                        if !cur[..at].ends_with('\n') {
                            add.insert(0, '\n');
                        }
                        cur.insert_str(at, &add);
                    }
                    _ => problems.push(format!(
                        "edit {n}: the heading line {heading:?} occurs more than once; use `old` with text of the right section"
                    )),
                }
            }
        }
    }
    if problems.is_empty() { Ok(cur) } else { Err(problems) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKILL: &str = "---\nname: demo\ndescription: \"Demo steps\"\n---\n\n# When to invoke\nWhen a demo runs.\n\n# Steps\n1. Build.\n2. Push.\n\n## Push details\nPush with the flag.\n\n```bash\n# not a heading\ngit push\n```\n\n### Deep\nDeep text.\n\n# Failure modes\n- Build fails: stop.\n";

    fn old(o: &str, n: &str) -> SkillEdit {
        SkillEdit { old: Some(o.into()), append_to_section: None, new: n.into() }
    }
    fn append(h: &str, n: &str) -> SkillEdit {
        SkillEdit { old: None, append_to_section: Some(h.into()), new: n.into() }
    }

    #[test]
    fn a_unique_quote_is_replaced() {
        let out = apply_edits(SKILL, &[old("2. Push.\n", "2. Run the tests.\n3. Push.\n")]).unwrap();
        assert!(out.contains("1. Build.\n2. Run the tests.\n3. Push.\n\n## Push details"));
        assert_eq!(out.len(), SKILL.len() + "2. Run the tests.\n".len());
    }

    #[test]
    fn a_missing_or_repeated_quote_refuses_the_whole_patch() {
        let missing = apply_edits(SKILL, &[old("1. Build.", "1. Compile."), old("3. Deploy.", "3. Ship.")]).unwrap_err();
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert!(missing[0].starts_with("edit 2: `old` does not occur"), "{missing:?}");
        let twice = apply_edits(SKILL, &[old("Push", "Send")]).unwrap_err();
        assert!(twice[0].contains("more than once"), "{twice:?}");
        // Overlapping occurrences count as two.
        assert!(apply_edits("---\nname: a\n---\naaa\n", &[old("aa", "b")]).is_err());
        let empty = apply_edits(SKILL, &[old("", "x")]).unwrap_err();
        assert!(empty[0].contains("`old` is empty"), "{empty:?}");
        assert!(apply_edits(SKILL, &[]).is_err());
        assert!(apply_edits(SKILL, &[old("1. Build.", "1. Build.")]).is_err());
        let both = SkillEdit { old: Some("x".into()), append_to_section: Some("# Steps".into()), new: "y".into() };
        assert!(apply_edits(SKILL, &[both]).unwrap_err()[0].contains("not both"));
    }

    #[test]
    fn edits_apply_in_order_to_the_text_left_by_the_earlier_ones() {
        let out = apply_edits(SKILL, &[old("1. Build.", "1. Build it."), old("1. Build it.\n2. Push.", "1. Build it.\n2. Push it.")])
            .unwrap();
        assert!(out.contains("1. Build it.\n2. Push it.\n"));
        // The second quote matches only after the first edit.
        assert!(apply_edits(SKILL, &[old("1. Build it.", "x")]).is_err());
        // An edit can make a later quote ambiguous.
        let err = apply_edits(SKILL, &[old("Deep text.", "Push with the flag."), old("Push with the flag.", "y")]).unwrap_err();
        assert!(err[0].starts_with("edit 2:") && err[0].contains("more than once"), "{err:?}");
    }

    #[test]
    fn crlf_text_and_quotes_and_a_missing_final_newline_are_normalized() {
        let crlf = format!("\u{feff}{}", SKILL.trim_end().replace('\n', "\r\n"));
        let out = apply_edits(&crlf, &[old("- Build fails: stop.\r\n", "- Build fails: read the log, then stop.\n")]).unwrap();
        assert!(!out.contains('\r') && !out.starts_with('\u{feff}'));
        assert!(out.ends_with("- Build fails: read the log, then stop.\n"), "{out}");
        assert_eq!(normalize("a"), "a\n");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn append_adds_lines_at_the_end_of_a_section() {
        let out = apply_edits(SKILL, &[append("# Failure modes", "- Push fails: retry once.")]).unwrap();
        assert!(out.ends_with("- Build fails: stop.\n- Push fails: retry once.\n"), "{out}");
        // A section with subsections: the end is before the next heading of
        // the same or a higher level, after the last non-blank line.
        let out = apply_edits(SKILL, &[append("# Steps", "Note: steps run in order.\n")]).unwrap();
        assert!(out.contains("Deep text.\nNote: steps run in order.\n\n# Failure modes"), "{out}");
        assert!(apply_edits(SKILL, &[append("# Nowhere", "x")]).unwrap_err()[0].contains("no heading line"));
        assert!(apply_edits(SKILL, &[append(FRONTMATTER_SECTION, "x")]).is_err());
        assert!(apply_edits(SKILL, &[append("# Steps", " \n")]).is_err());
        let dup = format!("{SKILL}\n# Steps\nagain\n");
        assert!(apply_edits(&dup, &[append("# Steps", "x")]).unwrap_err()[0].contains("more than once"));
    }

    #[test]
    fn the_outline_skips_frontmatter_and_code_and_sizes_sections() {
        let o = outline(SKILL);
        let heads: Vec<(&str, usize)> = o.iter().map(|s| (s.heading.as_str(), s.level)).collect();
        assert_eq!(
            heads,
            vec![
                (FRONTMATTER_SECTION, 0),
                ("# When to invoke", 1),
                ("# Steps", 1),
                ("## Push details", 2),
                ("### Deep", 3),
                ("# Failure modes", 1),
            ]
        );
        let (steps, s) = &sections(SKILL, "# Steps")[0];
        assert!(steps.starts_with("# Steps\n1. Build.") && steps.ends_with("Deep text.\n\n"), "{steps:?}");
        assert_eq!(s.len(), steps.len());
        let (push, _) = &sections(SKILL, "  ## Push details ")[0];
        assert!(push.contains("# not a heading") && push.ends_with("Deep text.\n\n"), "a ### section belongs to its ##");
        let (fm, _) = &sections(SKILL, FRONTMATTER_SECTION)[0];
        assert!(fm.starts_with("---\nname: demo") && fm.ends_with("---\n\n"));
        let total: usize = o.iter().filter(|s| s.level <= 1).map(Section::len).sum();
        assert_eq!(total, SKILL.len(), "top-level sections cover the text");
        let at = SKILL.find("git push").unwrap();
        assert_eq!(section_at(SKILL, at).unwrap().heading, "## Push details");
        assert!(sections(SKILL, "# Missing").is_empty());
    }
}
