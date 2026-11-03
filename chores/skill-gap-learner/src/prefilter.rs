//! The pre-filter (ADR-038): code, no model, decides which parts of which
//! sessions get a review session.
//!
//! Signals of a part (see [`Signals`]): operator turns, tool calls, failed
//! tool calls followed later by a successful call of the same tool, operator
//! turns that read as a correction or frustration, and a loaded skill
//! followed by such a correction. Sessions read from the session index carry
//! no tool calls, so only the text signals count there.

use nucleus_core::config::SkillLearnerConfig;
use nucleus_core::skill_learner::ledger::Source;
use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

/// What the pre-filter measured in one part of a session.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Signals {
    /// Operator turns (system-injected and Claude Code-written turns left out).
    pub user_turns: u32,
    pub tool_calls: u32,
    pub tool_errors: u32,
    /// Failed tool calls followed later in the part by a successful call of
    /// the same tool.
    pub error_recoveries: u32,
    /// Operator turns that read as a correction or frustration, plus tool
    /// calls the operator refused and interruptions.
    pub corrections: u32,
    /// Corrections that came after a skill was loaded in the session.
    pub skill_corrections: u32,
    /// Skills loaded in this part (Skill tool or a slash command), in order.
    pub skills_loaded: Vec<String>,
    /// Size of the rendered part in bytes.
    pub bytes: usize,
}

/// Correction and frustration phrases, English and Portuguese. Each needs
/// enough words to be specific: "no"/"não" alone is ordinary speech, and
/// "instead of", "wrong" or a swear word alone appear as often in neutral
/// sentences (measured on this workspace's transcripts, 2026-11).
fn correction_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let patterns = [
            // English
            r"^\s*(no|nope|wrong)\s*[,.!]",
            r"\b(that'?s|this is|it'?s) not (what|how|right|correct|it)\b",
            r"\b(that'?s|this is|it'?s|you'?re|is|are|was|looks|still) (wrong|incorrect)\b",
            r"\bwrong (file|path|place|branch|repo|folder|command|one|way|order|host|tool|skill|format|language|answer|account|channel|chat|target)\b",
            r"\bnot what i (asked|meant|wanted|said)\b",
            r"\bi (already )?(said|told you|asked you)\b",
            r"\b(stop|quit) (\w+ing|that|it)\b",
            r"\b(don'?t|do not|never) (do|use|add|write|call|run|touch|change|create|make|put|send|ask|guess) (that|this|it|again)\b",
            r"\bwhy (did|do|are|would) you\b",
            r"\byou (forgot|missed|ignored|broke|keep|always)\b",
            r"\bfrom now on\b",
            r"\bremember (this|that)\b",
            r"\b(wtf|what the (fuck|hell)|good fucking job|for fuck'?s sake)\b",
            // Portuguese
            r"^\s*(n[ãa]o|errado)\s*[,.!]",
            r"\bn[ãa]o (é|e|era|foi) (isso|assim|o que)\b",
            r"\b(t[áa]|est[áa]|ficou|isso [ée]) errad[oa]\b",
            r"\bpar[ae] de\b",
            r"\bj[áa] (te )?(falei|disse|pedi|expliquei)\b",
            r"\beu (te )?(falei|disse|pedi)\b",
            r"\bpor ?qu[eê] (voc[eê]|vc)\b",
            r"\b(voc[eê]|vc) (esqueceu|ignorou|errou|quebrou|sempre|n[ãa]o (fez|leu|seguiu|usou))\b",
            r"\ba partir de agora\b",
            r"\blembr[ae][- ]se\b",
            r"\b(n[ãa]o|nunca) (mais )?(fa[çc]a|use|coloque|mande|crie|mexa|toque|invente|chute)\b",
            r"\b(na|que) porra\b|\bpqp\b",
        ];
        Regex::new(&format!("(?i)(?:{})", patterns.join("|"))).expect("valid correction regex")
    })
}

/// True when an operator turn reads as a correction or frustration.
pub fn is_correction(text: &str) -> bool {
    text.starts_with("[Request interrupted by user") || correction_re().is_match(text)
}

/// The reason a part is worth a review, or `None` when it is not.
/// `continuation` is true for a part that does not start at the beginning
/// of the session: the earlier turns exist, so one operator turn is enough.
pub fn decide(s: &Signals, source: Source, cfg: &SkillLearnerConfig, continuation: bool) -> Option<String> {
    let min_turns = if continuation { cfg.min_user_turns.min(1) } else { cfg.min_user_turns };
    if s.user_turns < min_turns {
        return None;
    }
    if s.skill_corrections >= 1 {
        return Some(format!("a loaded skill was followed by {} correction(s)", s.skill_corrections));
    }
    if cfg.min_corrections > 0 && s.corrections >= cfg.min_corrections {
        return Some(format!("{} correction(s) by the operator", s.corrections));
    }
    match source {
        Source::Transcript => {
            if cfg.min_error_recoveries > 0 && s.error_recoveries >= cfg.min_error_recoveries {
                return Some(format!("{} failed tool call(s) retried successfully", s.error_recoveries));
            }
            if cfg.min_tool_calls > 0 && s.tool_calls >= cfg.min_tool_calls {
                return Some(format!("{} tool calls", s.tool_calls));
            }
        }
        Source::Index => {
            if s.user_turns >= cfg.index_min_user_turns {
                return Some(format!("{} operator turns", s.user_turns));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrections_need_specific_phrasing() {
        for yes in [
            "no, use the release build",
            "That's not what I asked",
            "I already told you to run the tests first",
            "stop doing that",
            "don't use that flag",
            "why did you delete the file?",
            "you forgot the migration",
            "that's the wrong file",
            "from now on, answer in one line",
            "wtf is this",
            "Não, é o outro arquivo",
            "não é isso",
            "está errado",
            "para de inventar",
            "já falei que é no main",
            "por que você mudou isso?",
            "você esqueceu o teste",
            "a partir de agora responde curto",
            "não faça commit",
            "[Request interrupted by user for tool use]",
        ] {
            assert!(is_correction(yes), "{yes}");
        }
        for no in [
            "now add the dashboard route",
            "can you look at the logs?",
            "não sei se precisa, o que você acha?",
            "nothing else, thanks",
            "o arquivo não existe ainda, cria ele",
            "notice the second column",
            "why this printer instead of a thermal one?",
            "Am I wrong? it looks fancy",
            "This is not in the scope of what we're doing",
            "ads on netflix and other shit",
            "no thanks needed",
        ] {
            assert!(!is_correction(no), "{no}");
        }
    }

    #[test]
    fn decide_uses_the_configured_thresholds() {
        let cfg = SkillLearnerConfig::default();
        let base = Signals { user_turns: 3, ..Signals::default() };
        assert_eq!(decide(&base, Source::Transcript, &cfg, false), None);
        let few_turns = Signals { user_turns: 1, corrections: 5, ..Signals::default() };
        assert_eq!(decide(&few_turns, Source::Transcript, &cfg, false), None);
        assert!(decide(&few_turns, Source::Transcript, &cfg, true).is_some(), "a continuation needs one turn");
        let corrected = Signals { corrections: 1, ..base.clone() };
        assert!(decide(&corrected, Source::Transcript, &cfg, false).is_some());
        let skill = Signals { skill_corrections: 1, ..base.clone() };
        assert!(decide(&skill, Source::Index, &cfg, false).unwrap().contains("loaded skill"));
        let tools = Signals { tool_calls: cfg.min_tool_calls, ..base.clone() };
        assert!(decide(&tools, Source::Transcript, &cfg, false).is_some());
        let recovered = Signals { error_recoveries: cfg.min_error_recoveries, ..base.clone() };
        assert!(decide(&recovered, Source::Transcript, &cfg, false).is_some());
        // The index has no tool data: only text signals and length count.
        assert_eq!(decide(&tools, Source::Index, &cfg, false), None);
        let long = Signals { user_turns: cfg.index_min_user_turns, ..Signals::default() };
        assert!(decide(&long, Source::Index, &cfg, false).is_some());
        assert_eq!(decide(&long, Source::Transcript, &cfg, false), None);
    }
}
