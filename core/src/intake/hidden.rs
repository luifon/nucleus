//! Hidden content in issue text (ADR-036, "The hidden-content hold").
//!
//! The operator reads an issue on its GitHub page before adding the gate
//! label. The page is GitHub's rendering of the Markdown, and the pipeline
//! reads the raw text. Everything the rendering drops, collapses, shows only
//! on hover or keeps out of view is text the operator did not see but an
//! agent reads. This module finds such content in the raw issue title, the
//! raw issue body and the raw collaborator comments an item uses, so the
//! pipeline can hold the item before any agent runs and show the operator
//! the complete hidden content.
//!
//! **How the Markdown is read.** The body and each comment are parsed with
//! `comrak` (a port of GitHub's cmark-gfm) with the extensions GitHub
//! enables for issues (tables, strikethrough, autolinks, task lists,
//! footnotes, `$` and `` $` `` math, alerts) and source positions on. The
//! syntax tree decides what is code (fenced and indented code blocks and
//! code spans, inside block quotes and list items too), and gives the
//! fences, tables, links, images, footnotes and math. Line endings are
//! normalized to LF first (a bare CR ends a line for GitHub); an offset map
//! takes every finding back to the raw text. Some checks also read the
//! source text around the code the tree found (HTML comments, tags,
//! `<details>`, link reference definitions, entities, hiding math macros):
//! the tree has no node for a reference definition, and comrak reports
//! wrong inline positions after one, so a code span is masked only when the
//! source at its reported position is really that code span. Where comrak
//! and GitHub could disagree, both readings run and the stricter result
//! stays.
//!
//! **What is flagged** is listed by [`Kind`]. Invisible characters are
//! flagged everywhere, code included (they are invisible in code too).
//!
//! **What cannot be detected:** content GitHub renders visibly but a person
//! misses (text far down a long body, look-alike characters, an instruction
//! in plain sight), and rendering rules GitHub adds after this list.

use super::event::Comment;
use comrak::nodes::{AstNode, NodeValue};
use serde::{Deserialize, Serialize};

/// One piece of hidden content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export, rename = "IntakeHiddenFinding")]
pub struct Finding {
    /// Where it is: `title`, `body` or `comment <id>`.
    pub location: String,
    /// [`Kind::as_str`].
    pub kind: String,
    /// 1-based line and column (in characters) of its start in the raw
    /// text; CR, LF and CRLF each end a line.
    #[ts(type = "number")]
    pub line: u32,
    #[ts(type = "number")]
    pub column: u32,
    /// Its range in the raw text of the location, in characters (Unicode
    /// scalar values), end exclusive.
    #[ts(type = "number")]
    pub start: u32,
    #[ts(type = "number")]
    pub end: u32,
    /// The complete hidden content made visible, never shortened:
    /// invisible characters as code points, everything else as its literal
    /// source (with any invisible character in it shown as `[U+XXXX]`).
    pub text: String,
}

/// The raw text of a location that has findings, stored with the hold so
/// the operator can review the whole source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export, rename = "IntakeHiddenSource")]
pub struct Source {
    pub location: String,
    pub text: String,
}

/// What an item is held for (`items.hold_json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub findings: Vec<Finding>,
    pub sources: Vec<Source>,
}

/// The kinds of hidden content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// `<!-- … -->`, closed or not: not rendered.
    HtmlComment,
    /// Invisible characters ([`INVISIBLE`]).
    InvisibleCharacters,
    /// An HTML entity that decodes to an invisible character: GitHub
    /// decodes it, so the page shows nothing.
    InvisibleEntity,
    /// A `<details>` block without `open`: collapsed until clicked.
    Details,
    /// A tag GitHub's sanitizer removes, or an attribute that can hide or
    /// restyle content ([`VISIBLE_TAGS`], [`attribute_allowed`]).
    HtmlTag,
    /// `[label]: url "title"`: renders as nothing.
    LinkDefinition,
    /// `[^label]: text`: shown only at the page bottom, only when referenced.
    FootnoteDefinition,
    /// Image alt text: not shown while the image loads.
    ImageAlt,
    /// A link or image title: shown only on hover.
    LinkTitle,
    /// A link whose destination differs from its visible text: the
    /// destination shows only on hover.
    LinkDestination,
    /// An image URL outside [`EXEMPT_IMAGE_HOSTS`]: the page shows the
    /// picture, not the address (which can carry text, or load content that
    /// differs from what the operator saw).
    ImageSource,
    /// Table cells beyond the header's column count: GFM drops them.
    TableExtraCells,
    /// Math with a macro outside [`MATH_VISIBLE`], or one of
    /// [`MATH_ALWAYS_FLAG`].
    MathStyling,
    /// A fence GitHub renders as a picture ([`RENDERED_FENCES`]).
    RenderedBlock,
    /// A fence info string with text after its first word, or a first word
    /// that is not a language identifier: GitHub shows neither.
    FenceInfo,
}

impl Kind {
    pub const ALL: [Kind; 15] = [
        Kind::HtmlComment,
        Kind::InvisibleCharacters,
        Kind::InvisibleEntity,
        Kind::Details,
        Kind::HtmlTag,
        Kind::LinkDefinition,
        Kind::FootnoteDefinition,
        Kind::ImageAlt,
        Kind::LinkTitle,
        Kind::LinkDestination,
        Kind::ImageSource,
        Kind::TableExtraCells,
        Kind::MathStyling,
        Kind::RenderedBlock,
        Kind::FenceInfo,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::HtmlComment => "html_comment",
            Kind::InvisibleCharacters => "invisible_characters",
            Kind::InvisibleEntity => "invisible_entity",
            Kind::Details => "details",
            Kind::HtmlTag => "html_tag",
            Kind::LinkDefinition => "link_definition",
            Kind::FootnoteDefinition => "footnote_definition",
            Kind::ImageAlt => "image_alt",
            Kind::LinkTitle => "link_title",
            Kind::LinkDestination => "link_destination",
            Kind::ImageSource => "image_source",
            Kind::TableExtraCells => "table_extra_cells",
            Kind::MathStyling => "math_styling",
            Kind::RenderedBlock => "rendered_block",
            Kind::FenceInfo => "fence_info",
        }
    }

    /// Words for the operator.
    pub fn label(self) -> &'static str {
        match self {
            Kind::HtmlComment => "HTML comment",
            Kind::InvisibleCharacters => "invisible characters",
            Kind::InvisibleEntity => "entity for an invisible character",
            Kind::Details => "collapsed <details> block",
            Kind::HtmlTag => "raw HTML",
            Kind::LinkDefinition => "link reference definition",
            Kind::FootnoteDefinition => "footnote definition",
            Kind::ImageAlt => "image alt text",
            Kind::LinkTitle => "link title",
            Kind::LinkDestination => "link destination",
            Kind::ImageSource => "image address",
            Kind::TableExtraCells => "table cells beyond the header",
            Kind::MathStyling => "math macro outside the visible-only list",
            Kind::RenderedBlock => "diagram or map block",
            Kind::FenceInfo => "code fence info string",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

// ── lists ────────────────────────────────────────────────────────────────

/// Tags GitHub's sanitizer keeps and renders with their content visible.
///
/// Source: the element allowlist of GitHub's HTML sanitizer as published in
/// `html-pipeline` (`SanitizationFilter`, `lib/html/pipeline/sanitization_filter.rb`),
/// minus the kept elements that hide or shrink content: `small` (smaller
/// text), `ruby`/`rt`/`rp` (`rp` is hidden where ruby is supported), `bdo`
/// (reorders the text), `time`, `wbr`, `h7`/`h8` (not headings), `samp` and
/// `var` stay. `details` is listed but a `<details>` without `open` is
/// reported as collapsed. Every other tag is removed by the sanitizer
/// (with or without its content) and is flagged.
pub const VISIBLE_TAGS: &[&str] = &[
    "a", "abbr", "b", "blockquote", "br", "caption", "cite", "code", "dd", "del", "details", "dfn", "div", "dl",
    "dt", "em", "figcaption", "figure", "h1", "h2", "h3", "h4", "h5", "h6", "hr", "i", "img", "ins", "kbd", "li",
    "mark", "ol", "p", "pre", "q", "s", "samp", "span", "strike", "strong", "sub", "summary", "sup", "table",
    "tbody", "td", "tfoot", "th", "thead", "tr", "tt", "ul", "var",
];

/// Attributes that neither hide nor restyle content, per tag. Every other
/// attribute is flagged: the sanitizer keeps some that change what is shown
/// (`title` shows only on hover, `width`/`height` can shrink an image to
/// nothing, `dir` reverses text, `hidden`, `style` and `class` are the usual
/// hiding tools even where GitHub drops them). `a href` and `img src`/`alt`
/// are checked by the link and image rules instead.
pub fn attribute_allowed(tag: &str, attr: &str) -> bool {
    matches!(
        (tag, attr),
        ("a", "href") | ("img", "src") | ("img", "alt") | ("td" | "th", "align") | ("ol", "start") | ("details", "open")
    )
}

/// Image hosts whose pictures are the normal pasted attachments of GitHub
/// issues: `https://github.com/user-attachments/…` (current uploads) and
/// `https://user-images.githubusercontent.com/…` (older uploads). An image
/// from anywhere else is flagged with its address.
pub const EXEMPT_IMAGE_HOSTS: &[(&str, &str)] =
    &[("github.com", "/user-attachments/"), ("user-images.githubusercontent.com", "/")];

/// Fence info words GitHub renders as a picture instead of showing the
/// source: Mermaid diagrams, GeoJSON/TopoJSON maps, STL 3D models.
pub const RENDERED_FENCES: &[&str] = &["mermaid", "geojson", "topojson", "stl"];

/// Macros that are flagged in math even if [`MATH_VISIBLE`] listed them by
/// mistake: they can hide, move, restyle or link text.
pub const MATH_ALWAYS_FLAG: &[&str] = &[
    "bbox", "enclose", "style", "class", "cssId", "href", "color", "phantom", "hphantom", "vphantom", "smash",
    "textcolor", "colorbox", "fcolorbox", "pagecolor", "htmlStyle", "htmlClass", "htmlId", "htmlData", "mathrlap",
    "mathllap", "rlap", "llap", "clap", "raise", "lower", "kern", "hspace", "vspace", "mkern", "mskip", "hskip",
    "unicode", "require", "def", "newcommand", "renewcommand", "let",
];

/// Math macros that only draw visible symbols or structure: Greek letters,
/// operators and relations, arrows, fractions and roots, big operators,
/// accents, delimiters, font switches for visible text, spacing of at most
/// two em, and a few environments. In `$…$`, `$$…$$`, `` $`…`$ `` and
/// ```` ```math ```` every macro outside this list is flagged, and so is
/// an optional `[…]` argument that contains `:`, `;` or `=` (CSS-like).
/// Symbol macros (`\,`, `\;`, `\{`, `\\`, …) are allowed.
pub const MATH_VISIBLE: &[&str] = &[
    // Greek
    "alpha", "beta", "gamma", "delta", "epsilon", "varepsilon", "zeta", "eta", "theta", "vartheta", "iota", "kappa",
    "lambda", "mu", "nu", "xi", "pi", "varpi", "rho", "varrho", "sigma", "varsigma", "tau", "upsilon", "phi",
    "varphi", "chi", "psi", "omega", "Gamma", "Delta", "Theta", "Lambda", "Xi", "Pi", "Sigma", "Upsilon", "Phi",
    "Psi", "Omega",
    // big operators and functions
    "sum", "prod", "coprod", "int", "iint", "iiint", "oint", "bigcup", "bigcap", "bigoplus", "bigotimes", "lim",
    "limsup", "liminf", "sup", "inf", "max", "min", "arg", "det", "dim", "exp", "ln", "log", "lg", "sin", "cos",
    "tan", "cot", "sec", "csc", "arcsin", "arccos", "arctan", "sinh", "cosh", "tanh", "gcd", "deg", "ker", "Pr",
    "mod", "bmod", "pmod", "limits", "nolimits",
    // relations and operators
    "le", "leq", "ge", "geq", "ne", "neq", "approx", "equiv", "sim", "simeq", "cong", "propto", "ll", "gg", "in",
    "notin", "ni", "subset", "subseteq", "supset", "supseteq", "cup", "cap", "setminus", "emptyset", "varnothing",
    "forall", "exists", "nexists", "neg", "lnot", "land", "lor", "wedge", "vee", "implies", "iff", "to", "gets",
    "mapsto", "rightarrow", "leftarrow", "Rightarrow", "Leftarrow", "leftrightarrow", "Leftrightarrow", "uparrow",
    "downarrow", "longrightarrow", "longleftarrow", "Longrightarrow", "mid", "parallel", "perp", "pm", "mp",
    "times", "div", "cdot", "cdots", "ldots", "dots", "vdots", "ddots", "circ", "bullet", "star", "ast", "oplus",
    "otimes", "odot", "infty", "partial", "nabla", "hbar", "ell", "Re", "Im", "aleph", "prime", "angle",
    "triangle", "square", "top", "bot", "vdash", "models", "not", "therefore", "because", "colon",
    // structure and accents
    "frac", "dfrac", "tfrac", "binom", "sqrt", "left", "right", "big", "Big", "bigg", "Bigg", "middle", "overline",
    "underline", "hat", "widehat", "bar", "tilde", "widetilde", "vec", "dot", "ddot", "acute", "grave", "breve",
    "check", "overbrace", "underbrace", "overrightarrow", "overleftarrow", "stackrel", "overset", "underset",
    // delimiters
    "langle", "rangle", "lfloor", "rfloor", "lceil", "rceil", "lvert", "rvert", "lVert", "rVert", "vert", "Vert",
    // fonts for visible text
    "text", "textbf", "textit", "textrm", "texttt", "mathrm", "mathbf", "mathit", "mathsf", "mathtt", "mathcal",
    "mathbb", "mathfrak", "boldsymbol", "operatorname", "displaystyle", "textstyle",
    // spacing and environments
    "quad", "qquad", "begin", "end",
];

/// Characters a fence info string's first word may use: a language
/// identifier (`rust`, `c++`, `objective-c`, `f#`, `shell-session`).
fn is_language_word(w: &str) -> bool {
    (1..=32).contains(&w.chars().count())
        && w.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'#' | b'.' | b'-'))
}

// ── invisible characters ─────────────────────────────────────────────────

/// Named invisible code points and ranges.
///
/// Source: the Unicode Character Database, version 16.0:
/// `DerivedGeneralCategory.txt` (every code point of category Cf),
/// `DerivedCoreProperties.txt` (`Default_Ignorable_Code_Point`) and
/// `UnicodeData.txt` (names). Added to those: characters that are not
/// default-ignorable but render as a blank (the Hangul fillers U+115F,
/// U+1160, U+3164, U+FFA0 are default-ignorable; U+2800 BRAILLE PATTERN
/// BLANK is not), the line and paragraph separators (rendered as nothing or
/// as a line break, depending on the browser), control characters other
/// than tab, line feed and carriage return (category Cc), and private-use
/// characters (no standard glyph: a box or nothing, never readable text).
/// Ranges whose members share a description have one entry.
const INVISIBLE: &[(u32, u32, &str)] = &[
    (0x0000, 0x0008, "CONTROL CHARACTER"),
    (0x000B, 0x000C, "CONTROL CHARACTER"),
    (0x000E, 0x001F, "CONTROL CHARACTER"),
    (0x007F, 0x009F, "CONTROL CHARACTER"),
    (0x00AD, 0x00AD, "SOFT HYPHEN"),
    (0x034F, 0x034F, "COMBINING GRAPHEME JOINER"),
    (0x0600, 0x0600, "ARABIC NUMBER SIGN"),
    (0x0601, 0x0601, "ARABIC SIGN SANAH"),
    (0x0602, 0x0602, "ARABIC FOOTNOTE MARKER"),
    (0x0603, 0x0603, "ARABIC SIGN SAFHA"),
    (0x0604, 0x0604, "ARABIC SIGN SAMVAT"),
    (0x0605, 0x0605, "ARABIC NUMBER MARK ABOVE"),
    (0x061C, 0x061C, "ARABIC LETTER MARK"),
    (0x06DD, 0x06DD, "ARABIC END OF AYAH"),
    (0x070F, 0x070F, "SYRIAC ABBREVIATION MARK"),
    (0x0890, 0x0890, "ARABIC POUND MARK ABOVE"),
    (0x0891, 0x0891, "ARABIC PIASTRE MARK ABOVE"),
    (0x08E2, 0x08E2, "ARABIC DISPUTED END OF AYAH"),
    (0x115F, 0x115F, "HANGUL CHOSEONG FILLER"),
    (0x1160, 0x1160, "HANGUL JUNGSEONG FILLER"),
    (0x17B4, 0x17B4, "KHMER VOWEL INHERENT AQ"),
    (0x17B5, 0x17B5, "KHMER VOWEL INHERENT AA"),
    (0x180B, 0x180D, "MONGOLIAN FREE VARIATION SELECTOR"),
    (0x180E, 0x180E, "MONGOLIAN VOWEL SEPARATOR"),
    (0x180F, 0x180F, "MONGOLIAN FREE VARIATION SELECTOR FOUR"),
    (0x200B, 0x200B, "ZERO WIDTH SPACE"),
    (0x200C, 0x200C, "ZERO WIDTH NON-JOINER"),
    (0x200D, 0x200D, "ZERO WIDTH JOINER"),
    (0x200E, 0x200E, "LEFT-TO-RIGHT MARK"),
    (0x200F, 0x200F, "RIGHT-TO-LEFT MARK"),
    (0x2028, 0x2028, "LINE SEPARATOR"),
    (0x2029, 0x2029, "PARAGRAPH SEPARATOR"),
    (0x202A, 0x202A, "LEFT-TO-RIGHT EMBEDDING"),
    (0x202B, 0x202B, "RIGHT-TO-LEFT EMBEDDING"),
    (0x202C, 0x202C, "POP DIRECTIONAL FORMATTING"),
    (0x202D, 0x202D, "LEFT-TO-RIGHT OVERRIDE"),
    (0x202E, 0x202E, "RIGHT-TO-LEFT OVERRIDE"),
    (0x2060, 0x2060, "WORD JOINER"),
    (0x2061, 0x2061, "FUNCTION APPLICATION"),
    (0x2062, 0x2062, "INVISIBLE TIMES"),
    (0x2063, 0x2063, "INVISIBLE SEPARATOR"),
    (0x2064, 0x2064, "INVISIBLE PLUS"),
    (0x2065, 0x2065, "UNASSIGNED DEFAULT-IGNORABLE CODE POINT"),
    (0x2066, 0x2066, "LEFT-TO-RIGHT ISOLATE"),
    (0x2067, 0x2067, "RIGHT-TO-LEFT ISOLATE"),
    (0x2068, 0x2068, "FIRST STRONG ISOLATE"),
    (0x2069, 0x2069, "POP DIRECTIONAL ISOLATE"),
    (0x206A, 0x206A, "INHIBIT SYMMETRIC SWAPPING"),
    (0x206B, 0x206B, "ACTIVATE SYMMETRIC SWAPPING"),
    (0x206C, 0x206C, "INHIBIT ARABIC FORM SHAPING"),
    (0x206D, 0x206D, "ACTIVATE ARABIC FORM SHAPING"),
    (0x206E, 0x206E, "NATIONAL DIGIT SHAPES"),
    (0x206F, 0x206F, "NOMINAL DIGIT SHAPES"),
    (0x2800, 0x2800, "BRAILLE PATTERN BLANK"),
    (0x3164, 0x3164, "HANGUL FILLER"),
    (0xE000, 0xF8FF, "PRIVATE USE CHARACTER"),
    (0xFE00, 0xFE0F, "VARIATION SELECTOR"),
    (0xFEFF, 0xFEFF, "ZERO WIDTH NO-BREAK SPACE"),
    (0xFFA0, 0xFFA0, "HALFWIDTH HANGUL FILLER"),
    (0xFFF0, 0xFFF8, "UNASSIGNED DEFAULT-IGNORABLE CODE POINT"),
    (0xFFF9, 0xFFF9, "INTERLINEAR ANNOTATION ANCHOR"),
    (0xFFFA, 0xFFFA, "INTERLINEAR ANNOTATION SEPARATOR"),
    (0xFFFB, 0xFFFB, "INTERLINEAR ANNOTATION TERMINATOR"),
    (0x110BD, 0x110BD, "KAITHI NUMBER SIGN"),
    (0x110CD, 0x110CD, "KAITHI NUMBER SIGN ABOVE"),
    (0x13430, 0x1343F, "EGYPTIAN HIEROGLYPH FORMAT CONTROL"),
    (0x1BCA0, 0x1BCA0, "SHORTHAND FORMAT LETTER OVERLAP"),
    (0x1BCA1, 0x1BCA1, "SHORTHAND FORMAT CONTINUING OVERLAP"),
    (0x1BCA2, 0x1BCA2, "SHORTHAND FORMAT DOWN STEP"),
    (0x1BCA3, 0x1BCA3, "SHORTHAND FORMAT UP STEP"),
    (0x1D173, 0x1D17A, "MUSICAL SYMBOL FORMAT CONTROL"),
    (0xE0000, 0xE0000, "UNASSIGNED TAG-BLOCK CODE POINT"),
    (0xE0001, 0xE0001, "LANGUAGE TAG"),
    (0xE0002, 0xE001F, "UNASSIGNED TAG-BLOCK CODE POINT"),
    (0xE0020, 0xE007E, "TAG CHARACTER"),
    (0xE007F, 0xE007F, "CANCEL TAG"),
    (0xE0080, 0xE00FF, "UNASSIGNED DEFAULT-IGNORABLE CODE POINT"),
    (0xE0100, 0xE01EF, "VARIATION SELECTOR"),
    (0xE01F0, 0xE0FFF, "UNASSIGNED DEFAULT-IGNORABLE CODE POINT"),
    (0xF0000, 0xFFFFD, "PRIVATE USE CHARACTER"),
    (0x100000, 0x10FFFD, "PRIVATE USE CHARACTER"),
];

/// The name of `c` when it is one of the [`INVISIBLE`] characters.
pub fn invisible_name(c: char) -> Option<String> {
    let cp = c as u32;
    let i = INVISIBLE.partition_point(|(_, end, _)| *end < cp);
    let (start, end, name) = *INVISIBLE.get(i)?;
    if cp < start || cp > end {
        return None;
    }
    Some(match cp {
        0xFE00..=0xFE0F => format!("VARIATION SELECTOR-{}", cp - 0xFE00 + 1),
        0xE0100..=0xE01EF => format!("VARIATION SELECTOR-{}", cp - 0xE0100 + 17),
        0xE0020..=0xE007E => format!("TAG {}", tag_ascii(cp).map(|a| format!("{a:?}")).unwrap_or_default()),
        _ => name.to_string(),
    })
}

/// The ASCII character a tag character (U+E0020–U+E007E) stands for.
fn tag_ascii(cp: u32) -> Option<char> {
    (0xE0020..=0xE007E).contains(&cp).then(|| char::from_u32(cp - 0xE0000)).flatten()
}

/// `U+200B ZERO WIDTH SPACE`.
pub fn code_point(c: char) -> String {
    match invisible_name(c) {
        Some(n) => format!("U+{:04X} {n}", c as u32),
        None => format!("U+{:04X}", c as u32),
    }
}

const ZWNJ: char = '\u{200C}';
const ZWJ: char = '\u{200D}';

/// An emoji character that can start or continue a ZWJ sequence element.
fn is_pictographic(c: char) -> bool {
    let cp = c as u32;
    matches!(cp, 0xA9 | 0xAE | 0x203C..=0x3299 | 0x1F000..=0x1FAFF) && !(0x1F3FB..=0x1F3FF).contains(&cp)
}

/// True when the ZWJ at `i` is inside a sequence Unicode lists as an RGI
/// emoji ZWJ sequence (`emojis` crate, Unicode Emoji 17.0 data from
/// `emoji-test.txt`, which contains every sequence of
/// `emoji-zwj-sequences.txt`).
fn zwj_in_emoji(chars: &[char], i: usize) -> bool {
    let part = |c: char| c == ZWJ || c == '\u{FE0F}' || (0x1F3FB..=0x1F3FF).contains(&(c as u32)) || is_pictographic(c);
    let mut l = i;
    while l > 0 && part(chars[l - 1]) {
        l -= 1;
    }
    let mut r = i + 1;
    while r < chars.len() && part(chars[r]) {
        r += 1;
    }
    // One sequence: a new emoji starts at a pictographic character that
    // does not follow a ZWJ.
    let mut start = l;
    let mut end = r;
    for k in l + 1..r {
        if is_pictographic(chars[k]) && chars[k - 1] != ZWJ {
            if k <= i {
                start = k;
            } else {
                end = k;
                break;
            }
        }
    }
    let seq: String = chars[start..end].iter().collect();
    emojis::get(&seq).is_some()
}

/// True when the ZWNJ or ZWJ at `i` sits between two letters where Unicode
/// defines its effect (RFC 5892, Appendix A.1 and A.2, which states the
/// Unicode joining rules as tests): after a virama (canonical combining
/// class 9) and before a letter, as in Indic scripts; or between a
/// character that joins to the following one (Joining_Type L or D) and one
/// that joins to the preceding one (R or D), with only transparent marks
/// (T) between, as in Arabic, Persian and Syriac. Joining types come from
/// Unicode 16.0 (`unicode-joining-type`).
fn joiner_in_script(chars: &[char], i: usize) -> bool {
    use unicode_joining_type::{get_joining_type, JoiningType as J};
    if i == 0 || i + 1 >= chars.len() {
        return false;
    }
    let virama = unicode_normalization::char::canonical_combining_class(chars[i - 1]) == 9;
    if virama && chars[i + 1].is_alphabetic() {
        return true;
    }
    let mut l = i;
    let before = loop {
        if l == 0 {
            break None;
        }
        l -= 1;
        match get_joining_type(chars[l]) {
            J::Transparent => continue,
            t => break Some(t),
        }
    };
    let mut r = i;
    let after = loop {
        r += 1;
        if r >= chars.len() {
            break None;
        }
        match get_joining_type(chars[r]) {
            J::Transparent => continue,
            t => break Some(t),
        }
    };
    matches!(before, Some(J::LeftJoining | J::DualJoining)) && matches!(after, Some(J::RightJoining | J::DualJoining))
}

/// Runs of invisible characters in `text`. Not flagged: a single U+FE0E or
/// U+FE0F right after a visible character (emoji presentation), a ZWJ inside
/// an RGI emoji ZWJ sequence, and a ZWNJ or ZWJ where [`joiner_in_script`]
/// holds.
fn invisible_runs(text: &str, out: &mut Vec<Raw>) {
    let idx: Vec<(usize, char)> = text.char_indices().collect();
    let chars: Vec<char> = idx.iter().map(|(_, c)| *c).collect();
    let hidden = |k: usize| -> bool {
        let c = chars[k];
        if invisible_name(c).is_none() {
            return false;
        }
        if c == '\u{FE0E}' || c == '\u{FE0F}' {
            let prev_visible = k > 0 && !chars[k - 1].is_whitespace() && invisible_name(chars[k - 1]).is_none();
            let prev_zwj_emoji = k > 0 && chars[k - 1] == ZWJ && zwj_in_emoji(&chars, k - 1);
            if prev_visible || prev_zwj_emoji {
                return false;
            }
        }
        if c == ZWJ && zwj_in_emoji(&chars, k) {
            return false;
        }
        if (c == ZWJ || c == ZWNJ) && joiner_in_script(&chars, k) {
            return false;
        }
        true
    };
    let mut k = 0;
    while k < chars.len() {
        if !hidden(k) {
            k += 1;
            continue;
        }
        let s = k;
        while k < chars.len() && hidden(k) {
            k += 1;
        }
        let at = idx[s].0;
        let end = idx.get(k).map(|(b, _)| *b).unwrap_or(text.len());
        out.push(Raw { kind: Kind::InvisibleCharacters, at, end, text: describe_run(&chars[s..k]) });
    }
}

/// `U+200B ZERO WIDTH SPACE ×3, U+2060 WORD JOINER`, complete; for tag
/// characters also the ASCII text they spell.
fn describe_run(run: &[char]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < run.len() {
        let mut j = i;
        while j < run.len() && run[j] == run[i] {
            j += 1;
        }
        let n = j - i;
        parts.push(if n > 1 { format!("{} ×{n}", code_point(run[i])) } else { code_point(run[i]) });
        i = j;
    }
    let tags: String = run.iter().filter_map(|c| tag_ascii(*c as u32)).collect();
    let list = parts.join(", ");
    if tags.is_empty() {
        list
    } else {
        format!("tag characters that spell {tags:?}: {list}")
    }
}

// ── scanning ─────────────────────────────────────────────────────────────

/// A finding in the normalized text, before its location and raw position.
#[derive(Debug, Clone)]
struct Raw {
    kind: Kind,
    /// Byte range in the normalized text.
    at: usize,
    end: usize,
    text: String,
}

fn raw(kind: Kind, at: usize, end: usize, text: String) -> Raw {
    Raw { kind, at, end: end.max(at), text }
}

/// Source text made safe to show, complete: invisible characters as
/// `[U+XXXX]`, everything else as written.
fn shown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            c if invisible_name(c).is_some() => out.push_str(&format!("[U+{:04X}]", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `text[a..b]`, with both ends moved back to character boundaries.
fn slice(text: &str, a: usize, b: usize) -> &str {
    let fix = |mut i: usize| {
        i = i.min(text.len());
        while !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let (a, b) = (fix(a), fix(b));
    if a >= b {
        ""
    } else {
        &text[a..b]
    }
}

/// The text with CRLF and bare CR turned into LF, and for every byte of it
/// the byte offset in the raw text (one more entry for the end).
struct Normalized {
    text: String,
    to_raw: Vec<usize>,
}

fn normalize(raw_text: &str) -> Normalized {
    let b = raw_text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut to_raw = Vec::with_capacity(b.len() + 1);
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\r' {
            out.push(b'\n');
            to_raw.push(i);
            i += if b.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
        } else {
            out.push(b[i]);
            to_raw.push(i);
            i += 1;
        }
    }
    to_raw.push(b.len());
    // Only ASCII bytes changed.
    Normalized { text: String::from_utf8(out).expect("normalizing line breaks keeps UTF-8"), to_raw }
}

/// Byte offsets of the line starts of `text`.
fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(text.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));
    v
}

/// True when the byte at `i` is preceded by an odd number of backslashes.
fn escaped(b: &[u8], i: usize) -> bool {
    let mut n = 0;
    let mut j = i;
    while j > 0 && b[j - 1] == b'\\' {
        n += 1;
        j -= 1;
    }
    n % 2 == 1
}

fn inside(ranges: &[(usize, usize)], i: usize) -> bool {
    ranges.iter().any(|(s, e)| *s <= i && i < *e)
}

// ── the syntax tree ──────────────────────────────────────────────────────

/// What the tree gives the source-text checks.
#[derive(Default)]
struct Tree {
    /// Shown literally (code), or not shown at all but reported by the tree
    /// (rendered and math fences): masked for the source-text checks.
    literal: Vec<(usize, usize)>,
    /// Inline and block math, already checked from the tree.
    math: Vec<(usize, usize)>,
}

fn comrak_options() -> comrak::Options<'static> {
    let mut o = comrak::Options::default();
    // GitHub's issue Markdown: GFM plus footnotes, math and alerts. The
    // tag filter is a rendering step (it escapes a few tags); the tags it
    // touches are flagged here anyway.
    o.extension.table = true;
    o.extension.strikethrough = true;
    o.extension.autolink = true;
    o.extension.tasklist = true;
    o.extension.footnotes = true;
    o.extension.math_dollars = true;
    o.extension.math_code = true;
    o.extension.alerts = true;
    o.render.sourcepos = true;
    o
}

/// The visible text under a node (text, code, line breaks).
fn node_text<'a>(n: &'a AstNode<'a>) -> String {
    let mut s = String::new();
    for d in n.descendants() {
        match &d.data().value {
            NodeValue::Text(t) => s.push_str(t),
            NodeValue::Code(c) => s.push_str(&c.literal),
            NodeValue::SoftBreak | NodeValue::LineBreak => s.push(' '),
            _ => {}
        }
    }
    s
}

/// Walk the tree of `text` (normalized): findings from the nodes, and the
/// ranges for the source-text checks.
fn tree_scan(text: &str, out: &mut Vec<Raw>) -> Tree {
    let arena = comrak::Arena::new();
    let options = comrak_options();
    let root = comrak::parse_document(&arena, text, &options);
    let starts = line_starts(text);
    let off = |line: usize, col: usize| -> usize {
        let ls = starts.get(line.saturating_sub(1)).copied().unwrap_or(text.len());
        (ls + col.saturating_sub(1)).min(text.len())
    };
    let range = |n: &AstNode<'_>| -> (usize, usize) {
        let sp = n.data().sourcepos;
        let a = off(sp.start.line, sp.start.column.max(1));
        let b = if sp.end.column == 0 { off(sp.end.line, 1) } else { off(sp.end.line, sp.end.column) + 1 };
        (a, b.min(text.len()).max(a))
    };
    let mut t = Tree::default();
    for n in root.descendants() {
        let (a, b) = range(n);
        let value = n.data().value.clone();
        match value {
            NodeValue::CodeBlock(cb) => {
                t.literal.push((a, b));
                if cb.fenced {
                    let info = cb.info.trim();
                    let word = info.split_whitespace().next().unwrap_or("");
                    let tail = info[word.len()..].trim();
                    let info_at = slice(text, a, b).find(info).map(|x| a + x).unwrap_or(a);
                    if !tail.is_empty() {
                        out.push(raw(Kind::FenceInfo, info_at, info_at + info.len(), format!("text after the language word: {}", shown(tail))));
                    }
                    if !word.is_empty() && !is_language_word(word) {
                        out.push(raw(Kind::FenceInfo, info_at, info_at + word.len(), format!("not a language word: {}", shown(word))));
                    }
                    let lower = word.to_ascii_lowercase();
                    if RENDERED_FENCES.contains(&lower.as_str()) {
                        out.push(raw(Kind::RenderedBlock, a, b, shown(slice(text, a, b))));
                    } else if lower == "math" {
                        if let Some(why) = math_problems(&cb.literal) {
                            out.push(raw(Kind::MathStyling, a, b, format!("{why}: {}", shown(&cb.literal))));
                        }
                    }
                }
            }
            NodeValue::Code(c) => {
                // Masked only when the source there is this code span (comrak
                // reports shifted inline positions after a removed reference
                // definition; unmasked code only adds findings).
                let s = slice(text, a, b);
                let ticks = "`".repeat(c.num_backticks);
                if c.num_backticks > 0 && s.len() >= 2 * c.num_backticks && s.starts_with(&ticks) && s.ends_with(&ticks) {
                    t.literal.push((a, b));
                }
            }
            NodeValue::Math(m) => {
                t.math.push((a, b));
                if let Some(why) = math_problems(&m.literal) {
                    out.push(raw(Kind::MathStyling, a, b, format!("{why}: {}", shown(&m.literal))));
                }
            }
            NodeValue::Link(l) => {
                if !l.title.is_empty() {
                    out.push(raw(Kind::LinkTitle, a, b, shown(&l.title)));
                }
                let label = node_text(n);
                if !same_destination(&label, &l.url) {
                    out.push(raw(Kind::LinkDestination, a, b, format!("{} (shown as {:?})", shown(&l.url), shown(label.trim()))));
                }
            }
            NodeValue::Image(l) => {
                let alt = node_text(n);
                if !alt.trim().is_empty() {
                    out.push(raw(Kind::ImageAlt, a, b, shown(&alt)));
                }
                if !l.title.is_empty() {
                    out.push(raw(Kind::LinkTitle, a, b, shown(&l.title)));
                }
                if !image_exempt(&l.url) {
                    out.push(raw(Kind::ImageSource, a, b, shown(&l.url)));
                }
            }
            NodeValue::FootnoteDefinition(_) => {
                out.push(raw(Kind::FootnoteDefinition, a, b, shown(slice(text, a, b))));
            }
            NodeValue::TableRow(_) => {
                let last_end = n.children().last().map(|c| range(c).1).unwrap_or(a);
                let line_end = text[a..].find('\n').map(|x| a + x).unwrap_or(text.len());
                let rest = slice(text, last_end.max(a), b.max(line_end));
                let extra = rest.trim().trim_start_matches('|').trim_end_matches('|').trim();
                if !extra.is_empty() {
                    let at = last_end + rest.find(extra).unwrap_or(0);
                    out.push(raw(Kind::TableExtraCells, at, at + extra.len(), shown(extra)));
                }
            }
            _ => {}
        }
    }
    t
}

/// A link's destination is visible when its text, normalized, is the
/// destination (autolinks, bare URLs, `[https://x](https://x)`).
fn same_destination(label: &str, url: &str) -> bool {
    fn norm(s: &str) -> String {
        let s = percent_decode(s.trim()).to_lowercase();
        let s = s.strip_prefix("mailto:").unwrap_or(&s);
        let s = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(s);
        s.trim_end_matches('/').to_string()
    }
    !url.trim().is_empty() && norm(label) == norm(url)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An image address on [`EXEMPT_IMAGE_HOSTS`] over HTTPS.
fn image_exempt(url: &str) -> bool {
    let Some(rest) = url.trim().strip_prefix("https://") else { return false };
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = host.to_ascii_lowercase();
    !path.contains("..")
        && !path.contains('\\')
        && EXEMPT_IMAGE_HOSTS.iter().any(|(h, p)| host == *h && path.starts_with(p) && path.len() > p.len())
}

/// Why a math source is flagged: the macros outside [`MATH_VISIBLE`] (or in
/// [`MATH_ALWAYS_FLAG`]) and CSS-like optional arguments; `None` when it
/// only draws visible symbols.
fn math_problems(src: &str) -> Option<String> {
    let b = src.as_bytes();
    let mut bad: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        let n = b[i + 1..].iter().take_while(|c| c.is_ascii_alphabetic()).count();
        if n == 0 {
            i += 2; // a symbol macro (`\,`, `\{`, `\\`)
            continue;
        }
        let name = &src[i + 1..i + 1 + n];
        let mut j = i + 1 + n;
        let flagged = MATH_ALWAYS_FLAG.contains(&name) || !MATH_VISIBLE.contains(&name);
        while j < b.len() && b[j] == b' ' {
            j += 1;
        }
        let css = b.get(j) == Some(&b'[')
            && src[j..].find(']').is_some_and(|e| src[j..j + e].contains([':', ';', '=']));
        if flagged || css {
            let item = format!("\\{name}");
            if !bad.contains(&item) {
                bad.push(item);
            }
        }
        i += 1 + n;
    }
    (!bad.is_empty()).then(|| format!("macros {}", bad.join(", ")))
}

// ── source-text checks (around the code the tree found) ──────────────────

/// `text` with every masked byte replaced by a space (line breaks kept), so
/// offsets stay the same.
fn masked(text: &str, ranges: &[(usize, usize)]) -> String {
    let mut b = text.as_bytes().to_vec();
    for &(s, e) in ranges {
        for x in &mut b[s.min(text.len())..e.min(text.len())] {
            if *x != b'\n' {
                *x = b' ';
            }
        }
    }
    // A range can end inside a multibyte character only if comrak's
    // positions are off; then nothing is masked (more findings, never fewer).
    String::from_utf8(b).unwrap_or_else(|_| text.to_string())
}

/// HTML comments; returns their ranges.
fn html_comments(m: &str, text: &str, out: &mut Vec<Raw>) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(rel) = m[from..].find("<!--") {
        let p = from + rel;
        let rest = &m[p + 4..];
        let end = if rest.starts_with('>') {
            p + 5
        } else if rest.starts_with("->") {
            p + 6
        } else {
            match rest.find("-->") {
                Some(e) => p + 4 + e + 3,
                None => m.len(),
            }
        };
        out.push(raw(Kind::HtmlComment, p, end, shown(slice(text, p, end))));
        ranges.push((p, end));
        from = end;
    }
    ranges
}

/// One parsed tag: name (lower case), whether it closes, attributes (name
/// in lower case, value), and its end.
struct Tag {
    name: String,
    closing: bool,
    attrs: Vec<(String, String)>,
    end: usize,
}

/// Parse the tag at `i` (`m[i] == '<'`); `None` when it is not a tag.
/// Without a closing `>` a tag counts only at the start of a line, where
/// CommonMark can start an HTML block with it.
fn parse_tag(m: &str, i: usize) -> Option<Tag> {
    let b = m.as_bytes();
    let closing = b.get(i + 1) == Some(&b'/');
    let n0 = i + 1 + closing as usize;
    if !b.get(n0).is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let nl = b[n0..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'-').count();
    let name = m[n0..n0 + nl].to_ascii_lowercase();
    let mut j = n0 + nl;
    if !matches!(b.get(j), None | Some(b' ' | b'\t' | b'\n' | b'/' | b'>')) {
        return None;
    }
    let mut attrs = Vec::new();
    loop {
        while j < b.len() && matches!(b[j], b' ' | b'\t' | b'\n') {
            j += 1;
        }
        match b.get(j) {
            None => break,
            Some(b'>') => return Some(Tag { name, closing, attrs, end: j + 1 }),
            Some(b'/') if b.get(j + 1) == Some(&b'>') => return Some(Tag { name, closing, attrs, end: j + 2 }),
            Some(b'<') => break,
            _ => {}
        }
        let a0 = j;
        while j < b.len() && !matches!(b[j], b' ' | b'\t' | b'\n' | b'=' | b'>' | b'<') && !(b[j] == b'/' && b.get(j + 1) == Some(&b'>')) {
            j += 1;
        }
        if j == a0 {
            j += 1; // a stray character: skip it
            continue;
        }
        let an = m[a0..j].to_ascii_lowercase();
        while j < b.len() && matches!(b[j], b' ' | b'\t') {
            j += 1;
        }
        let mut value = String::new();
        if b.get(j) == Some(&b'=') {
            j += 1;
            while j < b.len() && matches!(b[j], b' ' | b'\t' | b'\n') {
                j += 1;
            }
            match b.get(j) {
                Some(&q @ (b'"' | b'\'')) => {
                    let v0 = j + 1;
                    let v1 = b[v0..].iter().position(|c| *c == q).map(|x| v0 + x);
                    let Some(v1) = v1 else { break };
                    value = m[v0..v1].to_string();
                    j = v1 + 1;
                }
                _ => {
                    let v0 = j;
                    while j < b.len() && !matches!(b[j], b' ' | b'\t' | b'\n' | b'>') {
                        j += 1;
                    }
                    value = m[v0..j].to_string();
                }
            }
        }
        attrs.push((an, value));
    }
    // No `>`: only at the start of a line (an HTML block start).
    let ls = m[..i].rfind('\n').map(|x| x + 1).unwrap_or(0);
    let lead = &m[ls..i];
    (lead.len() <= 3 && lead.bytes().all(|c| c == b' ')).then(|| Tag {
        name,
        closing,
        attrs,
        end: m[i..].find('\n').map(|x| i + x).unwrap_or(m.len()),
    })
}

/// `<details>` blocks without `open`, nested ones included in the outer.
fn details(m: &str, text: &str, comments: &[(usize, usize)], out: &mut Vec<Raw>) {
    let lower = m.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("<details") {
        let p = from + rel;
        from = p + 8;
        if inside(comments, p) {
            continue;
        }
        let Some(tag) = parse_tag(m, p) else { continue };
        if tag.name != "details" || tag.closing || tag.attrs.iter().any(|(a, _)| a == "open") {
            continue;
        }
        let mut depth = 1;
        let mut i = tag.end;
        let mut end = m.len();
        while i < lower.len() {
            let open = lower[i..].find("<details").map(|x| i + x);
            let close = lower[i..].find("</details").map(|x| i + x);
            match (open, close) {
                (Some(o), Some(c)) if o < c => {
                    depth += 1;
                    i = o + 8;
                }
                (_, Some(c)) => {
                    depth -= 1;
                    i = c + 9;
                    if depth == 0 {
                        end = lower[c..].find('>').map(|x| c + x + 1).unwrap_or(m.len());
                        break;
                    }
                }
                _ => break,
            }
        }
        out.push(raw(Kind::Details, p, end, shown(slice(text, p, end))));
        from = end.max(p + 8);
    }
}

/// Visible text between `from` and the closing `</name>` (tags removed).
fn element_text(m: &str, from: usize, name: &str) -> Option<String> {
    let lower = m[from..].to_ascii_lowercase();
    let close = lower.find(&format!("</{name}>")).or_else(|| {
        lower.match_indices(&format!("</{name}")).map(|(i, _)| i).find(|&i| {
            lower.as_bytes().get(i + 2 + name.len()).is_some_and(|c| c.is_ascii_whitespace())
        })
    })?;
    let inner = &m[from..from + close];
    let mut s = String::new();
    let mut in_tag = false;
    for c in inner.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => s.push(c),
            _ => {}
        }
    }
    Some(s)
}

/// Raw HTML: tags outside [`VISIBLE_TAGS`], attributes outside
/// [`attribute_allowed`], `<a href>` whose text is not its address, `<img>`
/// alt text and addresses, nested `<sub>`/`<sup>`, and declarations,
/// processing instructions and CDATA (removed by the sanitizer).
fn html_tags(m: &str, text: &str, comments: &[(usize, usize)], out: &mut Vec<Raw>) {
    let b = m.as_bytes();
    let lower = m.to_ascii_lowercase();
    let mut small_depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' || inside(comments, i) {
            i += 1;
            continue;
        }
        let rest = &lower[i..];
        if rest.starts_with("<!--") {
            i += 4;
            continue;
        }
        let special_end = if rest.starts_with("<?") {
            Some(rest.find("?>").map(|x| i + x + 2))
        } else if rest.starts_with("<![cdata[") {
            Some(rest.find("]]>").map(|x| i + x + 3))
        } else if rest.len() > 2 && rest.as_bytes()[1] == b'!' && rest.as_bytes()[2].is_ascii_alphabetic() {
            Some(rest.find('>').map(|x| i + x + 1))
        } else {
            None
        };
        if let Some(end) = special_end {
            let end = end.unwrap_or_else(|| m[i..].find('\n').map(|x| i + x).unwrap_or(m.len()));
            out.push(raw(Kind::HtmlTag, i, end, shown(slice(text, i, end))));
            i = end.max(i + 1);
            continue;
        }
        let Some(tag) = parse_tag(m, i) else {
            i += 1;
            continue;
        };
        let src = shown(slice(text, i, tag.end));
        let name = tag.name.as_str();
        if !VISIBLE_TAGS.contains(&name) {
            out.push(raw(Kind::HtmlTag, i, tag.end, src));
            i = tag.end;
            continue;
        }
        let mut bad_attr = false;
        for (a, v) in &tag.attrs {
            match (name, a.as_str()) {
                ("a", "href") => {
                    let label = element_text(m, tag.end, "a").unwrap_or_default();
                    if !same_destination(&label, v) {
                        out.push(raw(Kind::LinkDestination, i, tag.end, format!("{} (shown as {:?})", shown(v), shown(label.trim()))));
                    }
                }
                ("img", "src") => {
                    if !image_exempt(v) {
                        out.push(raw(Kind::ImageSource, i, tag.end, shown(v)));
                    }
                }
                ("img", "alt") => {
                    if !v.trim().is_empty() {
                        out.push(raw(Kind::ImageAlt, i, tag.end, shown(v)));
                    }
                }
                _ if attribute_allowed(name, a) => {}
                _ => bad_attr = true,
            }
        }
        let mut flag = bad_attr;
        if name == "sub" || name == "sup" {
            if tag.closing {
                small_depth = (small_depth - 1).max(0);
            } else {
                if small_depth > 0 {
                    flag = true; // nested: shrinks text until unreadable
                }
                small_depth += 1;
            }
        }
        if flag {
            out.push(raw(Kind::HtmlTag, i, tag.end, src));
        }
        i = tag.end;
    }
}

/// Strip block-container prefixes (blockquote `>`, list markers) from the
/// start of a line; returns the offset of what follows.
fn after_containers(line: &str) -> usize {
    let b = line.as_bytes();
    let mut i = 0;
    loop {
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
        if i < b.len() && b[i] == b'>' {
            i += 1;
            continue;
        }
        if i + 1 < b.len() && matches!(b[i], b'-' | b'*' | b'+') && (b[i + 1] == b' ' || b[i + 1] == b'\t') {
            i += 2;
            continue;
        }
        let digits = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        if (1..=9).contains(&digits)
            && i + digits + 1 < b.len()
            && matches!(b[i + digits], b'.' | b')')
            && (b[i + digits + 1] == b' ' || b[i + digits + 1] == b'\t')
        {
            i += digits + 2;
            continue;
        }
        return i;
    }
}

/// Link reference definitions and footnote definitions, read from the
/// source lines (the tree keeps no node for a reference definition). A line
/// that only looks like one (inside a paragraph) is flagged too.
fn definitions(m: &str, text: &str, out: &mut Vec<Raw>) {
    let starts = line_starts(m);
    for (k, &s) in starts.iter().enumerate() {
        let e = m[s..].find('\n').map(|x| s + x).unwrap_or(m.len());
        let line = &m[s..e];
        let at = after_containers(line);
        let rest = &line[at..];
        if !rest.starts_with('[') {
            continue;
        }
        let rb = rest.as_bytes();
        let mut j = 1;
        let mut close = None;
        while j < rb.len() {
            match rb[j] {
                b'\\' => j += 1,
                b'[' => break,
                b']' => {
                    close = Some(j);
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        let Some(c) = close else { continue };
        if rb.get(c + 1) != Some(&b':') || rest[1..c].trim().is_empty() {
            continue;
        }
        let kind = if rest[1..].starts_with('^') { Kind::FootnoteDefinition } else { Kind::LinkDefinition };
        let mut end = e;
        if rest[c + 2..].trim().is_empty() {
            if let Some(&ns) = starts.get(k + 1) {
                end = m[ns..].find('\n').map(|x| ns + x).unwrap_or(m.len());
            }
        }
        out.push(raw(kind, s + at, end, shown(slice(text, s + at, end))));
    }
}

/// [`MATH_ALWAYS_FLAG`] macros anywhere outside code and outside the math
/// the tree found (in case GitHub reads math where comrak does not).
fn hiding_macros(m: &str, text: &str, math: &[(usize, usize)], out: &mut Vec<Raw>) {
    let b = m.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || inside(math, i) {
            i += 1;
            continue;
        }
        let n = b[i + 1..].iter().take_while(|c| c.is_ascii_alphabetic()).count();
        let name = &m[i + 1..i + 1 + n];
        if n > 0 && MATH_ALWAYS_FLAG.contains(&name) {
            let end = m[i..].find(char::is_whitespace).map(|x| i + x).unwrap_or(m.len());
            out.push(raw(Kind::MathStyling, i, end, shown(slice(text, i, end))));
        }
        i += 1 + n;
    }
}

/// Named HTML entities that decode to an invisible character.
const INVISIBLE_ENTITIES: &[(&str, char)] = &[
    ("shy", '\u{AD}'),
    ("zwnj", '\u{200C}'),
    ("zwj", '\u{200D}'),
    ("lrm", '\u{200E}'),
    ("rlm", '\u{200F}'),
    ("ZeroWidthSpace", '\u{200B}'),
    ("NegativeVeryThinSpace", '\u{200B}'),
    ("NegativeThinSpace", '\u{200B}'),
    ("NegativeMediumSpace", '\u{200B}'),
    ("NegativeThickSpace", '\u{200B}'),
    ("NoBreak", '\u{2060}'),
    ("ApplyFunction", '\u{2061}'),
    ("af", '\u{2061}'),
    ("InvisibleTimes", '\u{2062}'),
    ("it", '\u{2062}'),
    ("InvisibleComma", '\u{2063}'),
    ("ic", '\u{2063}'),
];

/// HTML entities (decimal, hexadecimal, named) that decode to an invisible
/// character, outside code (inside code GitHub shows them literally).
fn entities(m: &str, out: &mut Vec<Raw>) {
    let b = m.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'&' || escaped(b, i) {
            i += 1;
            continue;
        }
        let Some(rel) = b[i + 1..b.len().min(i + 40)].iter().position(|c| *c == b';') else {
            i += 1;
            continue;
        };
        let body = &m[i + 1..i + 1 + rel];
        let decoded = if let Some(num) = body.strip_prefix('#') {
            let v = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                (!hex.is_empty() && hex.len() <= 6 && hex.bytes().all(|c| c.is_ascii_hexdigit()))
                    .then(|| u32::from_str_radix(hex, 16).ok())
                    .flatten()
            } else {
                (!num.is_empty() && num.len() <= 7 && num.bytes().all(|c| c.is_ascii_digit()))
                    .then(|| num.parse::<u32>().ok())
                    .flatten()
            };
            v.and_then(char::from_u32)
        } else {
            INVISIBLE_ENTITIES.iter().find(|(n, _)| *n == body).map(|(_, c)| *c)
        };
        match decoded {
            Some(c) if c != '\0' && invisible_name(c).is_some() => {
                let end = i + 2 + rel;
                out.push(raw(Kind::InvisibleEntity, i, end, format!("{} → {}", &m[i..end], code_point(c))));
                i = end;
            }
            _ => i += 1,
        }
    }
}

/// Every finding of one Markdown text (normalized).
fn scan_markdown_raw(text: &str) -> Vec<Raw> {
    let mut out = Vec::new();
    invisible_runs(text, &mut out);
    let tree = tree_scan(text, &mut out);
    let m = masked(text, &tree.literal);
    let comments = html_comments(&m, text, &mut out);
    details(&m, text, &comments, &mut out);
    html_tags(&m, text, &comments, &mut out);
    definitions(&m, text, &mut out);
    hiding_macros(&m, text, &tree.math, &mut out);
    entities(&m, &mut out);
    out.sort_by_key(|r| (r.at, r.kind, r.end));
    out.dedup_by(|x, y| x.kind == y.kind && x.at == y.at);
    out
}

// ── public API ───────────────────────────────────────────────────────────

/// Turn findings in the normalized text into findings on the raw text.
fn locate(location: &str, raw_text: &str, norm: &Normalized, found: Vec<Raw>) -> Vec<Finding> {
    let pos = |nb: usize| -> (u32, u32, u32) {
        let rb = norm.to_raw[nb.min(norm.to_raw.len() - 1)];
        let before = &raw_text[..rb];
        let chars = before.chars().count() as u32;
        let mut line = 1u32;
        let mut col = 1u32;
        let mut prev_cr = false;
        for c in before.chars() {
            match c {
                '\n' if prev_cr => {}
                '\n' | '\r' => {
                    line += 1;
                    col = 1;
                }
                _ => col += 1,
            }
            prev_cr = c == '\r';
        }
        (line, col, chars)
    };
    found
        .into_iter()
        .map(|r| {
            let (line, column, start) = pos(r.at);
            let (_, _, end) = pos(r.end);
            Finding {
                location: location.to_string(),
                kind: r.kind.as_str().to_string(),
                line,
                column,
                start,
                end: end.max(start),
                text: r.text,
            }
        })
        .collect()
}

/// Findings in an issue title. GitHub shows the title as plain text (HTML
/// escaped, entities not decoded), so only invisible characters count.
pub fn scan_title(title: &str) -> Vec<Finding> {
    let norm = normalize(title);
    let mut found = Vec::new();
    invisible_runs(&norm.text, &mut found);
    locate("title", title, &norm, found)
}

/// Findings in a Markdown text (an issue body or a comment) at `location`.
pub fn scan_markdown(location: &str, text: &str) -> Vec<Finding> {
    let norm = normalize(text);
    let found = scan_markdown_raw(&norm.text);
    locate(location, text, &norm, found)
}

/// Location name of a comment.
pub fn comment_location(c: &Comment) -> String {
    format!("comment {}", c.id)
}

/// Every finding of an item's revision (the title, the body and each
/// comment the item uses), with the raw text of each location that has
/// findings.
pub fn scan_revision(title: &str, body: &str, comments: &[Comment]) -> Hold {
    let mut hold = Hold::default();
    let mut add = |loc: String, text: &str, found: Vec<Finding>| {
        if !found.is_empty() {
            hold.sources.push(Source { location: loc, text: text.to_string() });
            hold.findings.extend(found);
        }
    };
    add("title".into(), title, scan_title(title));
    add("body".into(), body, scan_markdown("body", body));
    for c in comments {
        let loc = comment_location(c);
        let found = scan_markdown(&loc, &c.body);
        add(loc, &c.body, found);
    }
    hold
}

/// What a release is bound to: the SHA-256 of every location that has
/// findings with its complete raw text, and of every finding with its
/// complete hidden text. A new comment without findings leaves it
/// unchanged; any change of a location with findings, or a new location
/// with findings, changes it.
pub fn fingerprint(hold: &Hold) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for s in &hold.sources {
        for part in [s.location.as_str(), &super::event::comment_hash(&s.text)] {
            h.update(part.as_bytes());
            h.update([0u8]);
        }
    }
    for f in &hold.findings {
        let head = format!("{}\0{}\0{}\0{}\0", f.location, f.kind, f.start, f.end);
        h.update(head.as_bytes());
        h.update(super::event::comment_hash(&f.text).as_bytes());
        h.update([0u8]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Length of the hold code the operator types (`#12 release a1b2c3`).
pub const HOLD_CODE_LEN: usize = 6;

/// The short code of a hold fingerprint.
pub fn hold_code(fingerprint: &str) -> &str {
    &fingerprint[..fingerprint.len().min(HOLD_CODE_LEN)]
}

/// True when `given` names the hold `fingerprint`: a hexadecimal prefix of
/// it with at least [`HOLD_CODE_LEN`] characters (letter case ignored).
pub fn names_hold(given: &str, fingerprint: &str) -> bool {
    let g = given.trim().to_ascii_lowercase();
    g.len() >= HOLD_CODE_LEN && g.bytes().all(|c| c.is_ascii_hexdigit()) && fingerprint.starts_with(&g)
}

/// `2 × HTML comment, 1 × invisible characters`, in [`Kind`] order.
pub fn summary(findings: &[Finding]) -> String {
    let mut parts = Vec::new();
    for k in Kind::ALL {
        let n = findings.iter().filter(|f| f.kind == k.as_str()).count();
        if n > 0 {
            parts.push(format!("{n} × {}", k.label()));
        }
    }
    parts.join(", ")
}

fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// One finding on one line, for a short message: `body 3:5 HTML comment:
/// <!-- … -->`, the text on one line and cut at `max` characters. Storage
/// and the full views keep the complete text.
pub fn describe(f: &Finding, max: usize) -> String {
    let label = Kind::parse(&f.kind).map(Kind::label).unwrap_or("hidden content");
    let one_line = f.text.replace('\n', " ⏎ ");
    format!("{} {}:{} {label}: {}", f.location, f.line, f.column, clip_chars(&one_line, max))
}

#[cfg(test)]
mod tests;
