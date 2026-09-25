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
//! code spans, inside block quotes and list items too). Line endings are
//! normalized to LF first (a bare CR ends a line for GitHub); an offset map
//! takes every finding back to the raw text.
//!
//! Which checks read the tree, and which read the text:
//!
//! - **The tree** (node kind, literal content, source position): fences
//!   and their info strings, tables, links, images, footnote definitions,
//!   math (inline, display, `` $`…`$ ``, math fences), and all raw HTML —
//!   every `HtmlBlock` and `HtmlInline` node, wherever it sits (quotes,
//!   lists, tables, any depth), is read from its literal: comments,
//!   forbidden elements and attributes, incomplete or unterminated tags,
//!   `<details>`.
//! - **The text**, with the code the tree found masked, only where the tree
//!   gives nothing to read: link reference definitions (comrak removes them
//!   and keeps no node), character references (the tree hands back decoded
//!   text; the source keeps the reference and its position), `<!--` outside
//!   every HTML node (a backstop in case GitHub starts a comment where
//!   comrak reads text), and the always-flagged math macros outside the math
//!   nodes (in case GitHub reads math where comrak does not).
//! - **Invisible characters** are read from the raw text, code included,
//!   with every character reference outside code decoded into the same
//!   character stream by a WHATWG decoder ([`charref`]: numeric references
//!   with or without `;`, all 2,231 named ones, the legacy names without
//!   `;`, the attribute-value rule inside tags), so the emoji and joining
//!   rules see `&zwj;` as a ZWJ and `&#8203` as a zero-width space.
//!
//! **Positions.** comrak reports wrong inline positions after a removed
//! reference definition. Every code span and inline HTML node is checked
//! against the source at its reported position; one that does not match is
//! placed by matching the nodes of its paragraph (or cell, or heading) in
//! document order against the source, skipping code, escaped `<` and
//! reference definitions. When that is not a one-to-one match, nothing is
//! guessed: a code span stays unmasked, and an HTML node's findings say
//! "position unknown" and cover the whole location. Where comrak and GitHub
//! could disagree, the stricter reading stays.
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

/// The values an allowed attribute may have (checked after WHATWG
/// attribute decoding): `open` with no value, an empty one or `open`;
/// `align` one of left, right, center, justify; `start` an optional `-` and
/// 1–9 ASCII digits. Letter case is ignored. Anything else is a value the
/// page never shows, so it is flagged with the decoded value.
pub fn attribute_value_allowed(attr: &str, value: &str) -> bool {
    let v = value.to_ascii_lowercase();
    match attr {
        "open" => v.is_empty() || v == "open",
        "align" => matches!(v.as_str(), "left" | "right" | "center" | "justify"),
        "start" => {
            let d = v.strip_prefix('-').unwrap_or(&v);
            (1..=9).contains(&d.len()) && d.bytes().all(|c| c.is_ascii_digit())
        }
        _ => false,
    }
}

/// An attribute value with its character references decoded by the WHATWG
/// attribute-value rules.
fn decode_attribute(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut i = 0;
    while i < v.len() {
        if v.as_bytes()[i] == b'&' {
            if let Some((cs, len)) = charref::decode_at(v, i, true) {
                out.extend(cs);
                i += len;
                continue;
            }
        }
        let c = v[i..].chars().next().expect("a char boundary");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// The only image addresses that are not flagged: GitHub's own attachment
/// URLs for pictures pasted into an issue, in their exact shapes.
///
/// - `https://github.com/user-attachments/assets/<uuid>`
/// - `https://user-images.githubusercontent.com/<digits>/<digits>-<uuid>.<ext>`
///   with `<ext>` in [`ATTACHMENT_EXTENSIONS`]
///
/// `<uuid>` is canonical lower-case 8-4-4-4-12 hex. Anything else is
/// flagged: credentials, a port, a query, a fragment, percent-encoding or
/// dot segments in the path, a trailing slash or trailing text.
pub const EXEMPT_IMAGE_HOSTS: &[&str] = &["github.com", "user-images.githubusercontent.com"];

/// File extensions of the older attachment host.
pub const ATTACHMENT_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "mp4", "mov"];

/// Base characters of the emoji variation sequences: each one followed by
/// U+FE0E (text style) or U+FE0F (emoji style) is a sequence Unicode
/// defines. Source: `emoji-variation-sequences.txt`, Unicode 16.0 (the same
/// 371 bases for both selectors). `StandardizedVariants.txt` 16.0 defines
/// no sequence with U+FE0E or U+FE0F, so it adds none.
const VARIATION_BASES: &[u32] = &[
    0x0023, 0x002A, 0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035, 0x0036, 0x0037, 0x0038, 0x0039,
    0x00A9, 0x00AE, 0x203C, 0x2049, 0x2122, 0x2139, 0x2194, 0x2195, 0x2196, 0x2197, 0x2198, 0x2199,
    0x21A9, 0x21AA, 0x231A, 0x231B, 0x2328, 0x23CF, 0x23E9, 0x23EA, 0x23EB, 0x23EC, 0x23ED, 0x23EE,
    0x23EF, 0x23F0, 0x23F1, 0x23F2, 0x23F3, 0x23F8, 0x23F9, 0x23FA, 0x24C2, 0x25AA, 0x25AB, 0x25B6,
    0x25C0, 0x25FB, 0x25FC, 0x25FD, 0x25FE, 0x2600, 0x2601, 0x2602, 0x2603, 0x2604, 0x260E, 0x2611,
    0x2614, 0x2615, 0x2618, 0x261D, 0x2620, 0x2622, 0x2623, 0x2626, 0x262A, 0x262E, 0x262F, 0x2638,
    0x2639, 0x263A, 0x2640, 0x2642, 0x2648, 0x2649, 0x264A, 0x264B, 0x264C, 0x264D, 0x264E, 0x264F,
    0x2650, 0x2651, 0x2652, 0x2653, 0x265F, 0x2660, 0x2663, 0x2665, 0x2666, 0x2668, 0x267B, 0x267E,
    0x267F, 0x2692, 0x2693, 0x2694, 0x2695, 0x2696, 0x2697, 0x2699, 0x269B, 0x269C, 0x26A0, 0x26A1,
    0x26A7, 0x26AA, 0x26AB, 0x26B0, 0x26B1, 0x26BD, 0x26BE, 0x26C4, 0x26C5, 0x26C8, 0x26CE, 0x26CF,
    0x26D1, 0x26D3, 0x26D4, 0x26E9, 0x26EA, 0x26F0, 0x26F1, 0x26F2, 0x26F3, 0x26F4, 0x26F5, 0x26F7,
    0x26F8, 0x26F9, 0x26FA, 0x26FD, 0x2702, 0x2705, 0x2708, 0x2709, 0x270A, 0x270B, 0x270C, 0x270D,
    0x270F, 0x2712, 0x2714, 0x2716, 0x271D, 0x2721, 0x2728, 0x2733, 0x2734, 0x2744, 0x2747, 0x274C,
    0x274E, 0x2753, 0x2754, 0x2755, 0x2757, 0x2763, 0x2764, 0x2795, 0x2796, 0x2797, 0x27A1, 0x27B0,
    0x27BF, 0x2934, 0x2935, 0x2B05, 0x2B06, 0x2B07, 0x2B1B, 0x2B1C, 0x2B50, 0x2B55, 0x3030, 0x303D,
    0x3297, 0x3299, 0x1F004, 0x1F170, 0x1F171, 0x1F17E, 0x1F17F, 0x1F202, 0x1F21A, 0x1F22F,
    0x1F237, 0x1F30D, 0x1F30E, 0x1F30F, 0x1F315, 0x1F31C, 0x1F321, 0x1F324, 0x1F325, 0x1F326,
    0x1F327, 0x1F328, 0x1F329, 0x1F32A, 0x1F32B, 0x1F32C, 0x1F336, 0x1F378, 0x1F37D, 0x1F393,
    0x1F396, 0x1F397, 0x1F399, 0x1F39A, 0x1F39B, 0x1F39E, 0x1F39F, 0x1F3A7, 0x1F3AC, 0x1F3AD,
    0x1F3AE, 0x1F3C2, 0x1F3C4, 0x1F3C6, 0x1F3CA, 0x1F3CB, 0x1F3CC, 0x1F3CD, 0x1F3CE, 0x1F3D4,
    0x1F3D5, 0x1F3D6, 0x1F3D7, 0x1F3D8, 0x1F3D9, 0x1F3DA, 0x1F3DB, 0x1F3DC, 0x1F3DD, 0x1F3DE,
    0x1F3DF, 0x1F3E0, 0x1F3ED, 0x1F3F3, 0x1F3F5, 0x1F3F7, 0x1F408, 0x1F415, 0x1F41F, 0x1F426,
    0x1F43F, 0x1F441, 0x1F442, 0x1F446, 0x1F447, 0x1F448, 0x1F449, 0x1F44D, 0x1F44E, 0x1F453,
    0x1F46A, 0x1F47D, 0x1F4A3, 0x1F4B0, 0x1F4B3, 0x1F4BB, 0x1F4BF, 0x1F4CB, 0x1F4DA, 0x1F4DF,
    0x1F4E4, 0x1F4E5, 0x1F4E6, 0x1F4EA, 0x1F4EB, 0x1F4EC, 0x1F4ED, 0x1F4F7, 0x1F4F9, 0x1F4FA,
    0x1F4FB, 0x1F4FD, 0x1F508, 0x1F50D, 0x1F512, 0x1F513, 0x1F549, 0x1F54A, 0x1F550, 0x1F551,
    0x1F552, 0x1F553, 0x1F554, 0x1F555, 0x1F556, 0x1F557, 0x1F558, 0x1F559, 0x1F55A, 0x1F55B,
    0x1F55C, 0x1F55D, 0x1F55E, 0x1F55F, 0x1F560, 0x1F561, 0x1F562, 0x1F563, 0x1F564, 0x1F565,
    0x1F566, 0x1F567, 0x1F56F, 0x1F570, 0x1F573, 0x1F574, 0x1F575, 0x1F576, 0x1F577, 0x1F578,
    0x1F579, 0x1F587, 0x1F58A, 0x1F58B, 0x1F58C, 0x1F58D, 0x1F590, 0x1F5A5, 0x1F5A8, 0x1F5B1,
    0x1F5B2, 0x1F5BC, 0x1F5C2, 0x1F5C3, 0x1F5C4, 0x1F5D1, 0x1F5D2, 0x1F5D3, 0x1F5DC, 0x1F5DD,
    0x1F5DE, 0x1F5E1, 0x1F5E3, 0x1F5E8, 0x1F5EF, 0x1F5F3, 0x1F5FA, 0x1F610, 0x1F687, 0x1F68D,
    0x1F691, 0x1F694, 0x1F698, 0x1F6AD, 0x1F6B2, 0x1F6B9, 0x1F6BA, 0x1F6BC, 0x1F6CB, 0x1F6CD,
    0x1F6CE, 0x1F6CF, 0x1F6E0, 0x1F6E1, 0x1F6E2, 0x1F6E3, 0x1F6E4, 0x1F6E5, 0x1F6E9, 0x1F6F0,
    0x1F6F3,
];

fn is_variation_base(c: char) -> bool {
    VARIATION_BASES.binary_search(&(c as u32)).is_ok()
}

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

/// True when the character at `k` is hidden content. Not hidden: a U+FE0E
/// or U+FE0F right after one of the [`VARIATION_BASES`] (a variation
/// sequence Unicode defines) or inside an RGI emoji ZWJ sequence; a ZWJ
/// inside an RGI emoji ZWJ sequence; a ZWNJ or ZWJ where
/// [`joiner_in_script`] holds. `chars` is the text as GitHub renders it:
/// entities outside code already decoded.
fn hidden_at(chars: &[char], k: usize) -> bool {
    let c = chars[k];
    if invisible_name(c).is_none() {
        return false;
    }
    if c == '\u{FE0E}' || c == '\u{FE0F}' {
        if k > 0 && is_variation_base(chars[k - 1]) {
            return false;
        }
        let in_sequence = (k + 1 < chars.len() && chars[k + 1] == ZWJ && zwj_in_emoji(chars, k + 1))
            || (k >= 2 && chars[k - 2] == ZWJ && zwj_in_emoji(chars, k - 2));
        if c == '\u{FE0F}' && in_sequence {
            return false;
        }
        return true;
    }
    if c == ZWJ && zwj_in_emoji(chars, k) {
        return false;
    }
    if (c == ZWJ || c == ZWNJ) && joiner_in_script(chars, k) {
        return false;
    }
    true
}

/// One character of the text as GitHub renders it, with its source bytes:
/// every character a reference outside code produces carries the
/// reference's range.
#[derive(Debug, Clone, Copy)]
struct Unit {
    c: char,
    at: usize,
    end: usize,
    entity: bool,
    /// Inside a tag's source (from `<` to its `>`), which is never
    /// rendered: an invisible character here is flagged whatever surrounds
    /// it, and nothing here is a neighbour an exemption may lean on.
    in_tag: bool,
}

/// The characters of `text` as GitHub renders them, each with its source
/// bytes. With `decode = false` (the title) every character is literal.
/// Otherwise each range is decoded exactly as its renderer decodes it:
///
/// - code (`code` ranges): literal;
/// - raw HTML (`html` ranges, the placed HTML nodes): the browser's WHATWG
///   rules ([`charref::decode_at`]), with the attribute-value rule inside
///   `attribute` ranges (the tags);
/// - Markdown text (everything else): CommonMark's rules
///   ([`charref::decode_commonmark_at`]: only references that end in `;`),
///   a backslash-escaped `&` stays literal, and an unescaped `*` is not a
///   character GitHub is sure to show (it may be an emphasis delimiter), so
///   it stands in the stream as U+FFFC, which no exemption accepts as a
///   neighbour.
fn units(text: &str, code: &[(usize, usize)], decode: bool, attribute: &[(usize, usize)], html: &[(usize, usize)]) -> Vec<Unit> {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if decode && !inside(code, i) {
            let in_html = inside(html, i);
            if b[i] == b'&' {
                let decoded = if in_html {
                    charref::decode_at(text, i, inside(attribute, i))
                } else if !escaped(b, i) {
                    charref::decode_commonmark_at(text, i)
                } else {
                    None
                };
                if let Some((cs, len)) = decoded {
                    for c in cs {
                        out.push(Unit { c, at: i, end: i + len, entity: true, in_tag: inside(attribute, i) });
                    }
                    i += len;
                    continue;
                }
            }
            if b[i] == b'*' && !in_html && !escaped(b, i) {
                out.push(Unit { c: '\u{FFFC}', at: i, end: i + 1, entity: false, in_tag: false });
                i += 1;
                continue;
            }
        }
        let c = text[i..].chars().next().expect("a char boundary");
        out.push(Unit { c, at: i, end: i + c.len_utf8(), entity: false, in_tag: decode && inside(attribute, i) });
        i += c.len_utf8();
    }
    out
}

/// Invisible characters in a character stream: runs of literal ones, and
/// each reference that produces one (one finding per reference, listing
/// the hidden characters it produces).
fn invisible_findings(text: &str, us: &[Unit], out: &mut Vec<Raw>) {
    let chars: Vec<char> = us.iter().map(|u| u.c).collect();
    // What the exemptions see: tag syntax is not rendered, so a visible
    // character inside a tag stands as U+FFFC, which no exemption accepts.
    let context: Vec<char> =
        us.iter().map(|u| if u.in_tag && invisible_name(u.c).is_none() { '\u{FFFC}' } else { u.c }).collect();
    let hidden: Vec<bool> = (0..chars.len())
        .map(|k| if us[k].in_tag { invisible_name(chars[k]).is_some() } else { hidden_at(&context, k) })
        .collect();
    let mut k = 0;
    while k < us.len() {
        if us[k].entity {
            let (at, end) = (us[k].at, us[k].end);
            let s = k;
            while k < us.len() && us[k].entity && us[k].at == at {
                k += 1;
            }
            let bad: Vec<String> = (s..k).filter(|&j| hidden[j]).map(|j| code_point(chars[j])).collect();
            if !bad.is_empty() {
                out.push(raw(Kind::InvisibleEntity, at, end, format!("{} → {}", &text[at..end], bad.join(", "))));
            }
            continue;
        }
        if !hidden[k] {
            k += 1;
            continue;
        }
        let s = k;
        while k < us.len() && hidden[k] && !us[k].entity {
            k += 1;
        }
        out.push(raw(Kind::InvisibleCharacters, us[s].at, us[k - 1].end, describe_run(&chars[s..k])));
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
    /// Every raw HTML node, in document order.
    html: Vec<HtmlNode>,
}

/// A raw HTML node: its literal (container prefixes removed) and, for each
/// literal byte and one past the end, the source byte it came from.
struct HtmlNode {
    literal: String,
    map: Vec<usize>,
    range: (usize, usize),
    /// False when the node could not be placed in the source: its findings
    /// cover the whole location and say so.
    known: bool,
}

impl HtmlNode {
    fn src(&self, i: usize) -> usize {
        self.map[i.min(self.map.len() - 1)]
    }
}

/// An HTML node placed at source byte `at`.
fn html_node(text: &str, starts: &[usize], at: usize, literal: String) -> HtmlNode {
    let line = starts.partition_point(|s| *s <= at);
    let map = html_map(text, starts, at, line, &literal);
    let end = map.last().copied().unwrap_or(at);
    HtmlNode { map, literal, range: (at, end.max(at)), known: true }
}

/// An HTML node that could not be placed.
fn html_node_unknown(literal: String) -> HtmlNode {
    HtmlNode { map: vec![0], literal, range: (0, 0), known: false }
}

/// True when `literal` (possibly several lines) is the source at `at`: its
/// first line starts there, and every later line ends its source line (the
/// container prefix is what comrak removed).
fn literal_at(text: &str, starts: &[usize], at: usize, literal: &str) -> bool {
    let lines: Vec<&str> = literal.strip_suffix('\n').unwrap_or(literal).split('\n').collect();
    if at > text.len() || !text[at..].starts_with(lines[0]) {
        return false;
    }
    let first = starts.partition_point(|s| *s <= at);
    lines.iter().enumerate().skip(1).all(|(k, l)| {
        let Some(&ls) = starts.get(first - 1 + k) else { return false };
        let le = text[ls..].find('\n').map(|x| ls + x).unwrap_or(text.len());
        text[ls..le].ends_with(l)
    })
}

/// The code span source for a Code node with `n` backticks and content
/// `literal`, when `text[p..]` starts with one: its end.
fn code_span_at(text: &str, p: usize, n: usize, literal: &str) -> Option<usize> {
    let b = text.as_bytes();
    let run = |i: usize| b[i.min(b.len())..].iter().take_while(|c| **c == b'`').count();
    if run(p) != n || (p > 0 && b[p - 1] == b'`') {
        return None;
    }
    let mut j = p + n;
    while j < b.len() {
        if b[j] == b'`' {
            let m = run(j);
            if m == n {
                let inner = text[p + n..j].replace('\n', " ");
                let inner = if inner.len() >= 2 && inner.starts_with(' ') && inner.ends_with(' ') && inner.trim() != "" {
                    inner[1..inner.len() - 1].to_string()
                } else {
                    inner
                };
                return (inner == literal).then_some(j + n);
            }
            j += m;
        } else {
            j += 1;
        }
    }
    None
}

/// Map each byte of an HTML node's `literal` back to the source: literal
/// line k comes from source line `first_line + k`, as a suffix of it (the
/// container prefix is what was removed), and the first line starts at
/// `start`. A line that is not a suffix of its source line (a tab expanded
/// in the prefix) maps to its source line's start.
fn html_map(text: &str, starts: &[usize], start: usize, first_line: usize, literal: &str) -> Vec<usize> {
    let mut map = Vec::with_capacity(literal.len() + 1);
    let mut first = true;
    for (line_no, piece) in (first_line..).zip(literal.split_inclusive('\n')) {
        let body = piece.strip_suffix('\n').unwrap_or(piece);
        let ls = starts.get(line_no.saturating_sub(1)).copied().unwrap_or(text.len());
        let le = text[ls.min(text.len())..].find('\n').map(|x| ls + x).unwrap_or(text.len());
        let base = if first {
            start
        } else if text[ls.min(le)..le].ends_with(body) {
            le - body.len()
        } else {
            ls
        };
        for j in 0..piece.len() {
            map.push((base + j).min(text.len()));
        }
        first = false;
    }
    map.push(map.last().map(|x| (x + 1).min(text.len())).unwrap_or(start));
    map
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
fn tree_scan(text: &str, defs: &[(usize, usize)], out: &mut Vec<Raw>) -> Tree {
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
    let mut pending: Vec<Pending> = Vec::new();
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
                let (block, block_range) = inline_block(n, &range);
                let ok = code_span_at(text, a, c.num_backticks, &c.literal) == Some(b);
                pending.push(Pending { code: Some(c.num_backticks), literal: c.literal, at: a, end: b, ok, block, block_range });
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
            // Block positions are reliable: the node is placed where comrak says.
            NodeValue::HtmlBlock(hb) => t.html.push(html_node(text, &starts, a, hb.literal)),
            NodeValue::HtmlInline(lit) => {
                let (block, block_range) = inline_block(n, &range);
                let ok = literal_at(text, &starts, a, &lit);
                pending.push(Pending { code: None, literal: lit, at: a, end: b, ok, block, block_range });
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
    place_inline(text, &starts, defs, pending, &mut t);
    t
}

/// A code span or inline HTML node whose position is checked before use.
struct Pending {
    /// `Some(backticks)` for a code span, `None` for inline HTML.
    code: Option<usize>,
    literal: String,
    at: usize,
    end: usize,
    /// The source at the reported position is this node.
    ok: bool,
    /// The leaf block that holds it (paragraph, heading, table cell…) and
    /// that block's range (block positions are reliable).
    block: usize,
    block_range: (usize, usize),
}

/// The nearest ancestor of an inline node that is not inline, as an id and
/// its source range.
fn inline_block<'a>(n: &'a AstNode<'a>, range: &dyn Fn(&AstNode<'_>) -> (usize, usize)) -> (usize, (usize, usize)) {
    let mut cur = n;
    while let Some(p) = cur.parent() {
        cur = p;
        let inline = matches!(
            cur.data().value,
            NodeValue::Emph
                | NodeValue::Strong
                | NodeValue::Strikethrough
                | NodeValue::Link(_)
                | NodeValue::Image(_)
                | NodeValue::Superscript
                | NodeValue::Subscript
                | NodeValue::Underline
                | NodeValue::Highlight
                | NodeValue::Insert
                | NodeValue::SpoileredText
                | NodeValue::WikiLink(_)
                | NodeValue::Escaped
        );
        if !inline {
            break;
        }
    }
    (cur as *const AstNode<'_> as usize, range(cur))
}

/// Place the pending inline nodes. A node whose reported position holds it
/// keeps it. Otherwise the nodes of one block with the same kind and
/// literal are matched, in document order, against their occurrences in the
/// block's source (outside placed code, escaped `<` and reference
/// definitions); only a one-to-one match places them. An unplaced code span
/// stays unmasked; an unplaced HTML node is reported with its position
/// unknown.
fn place_inline(text: &str, starts: &[usize], defs: &[(usize, usize)], mut pending: Vec<Pending>, t: &mut Tree) {
    let b = text.as_bytes();
    // A reported position counts only outside reference definitions and,
    // for HTML, outside code that itself checked out. A block with any node
    // that fails is shifted: all its nodes go through the matcher.
    let code_ok: Vec<(usize, usize)> =
        pending.iter().filter(|p| p.code.is_some() && p.ok && !inside(defs, p.at)).map(|p| (p.at, p.end)).collect();
    let shifted: std::collections::HashSet<usize> = pending
        .iter()
        .filter(|p| !p.ok || inside(defs, p.at) || (p.code.is_none() && inside(&code_ok, p.at)))
        .map(|p| p.block)
        .collect();
    for p in &mut pending {
        if shifted.contains(&p.block) {
            p.ok = false;
        }
    }
    let mut placed: Vec<Option<(usize, usize)>> = pending.iter().map(|p| p.ok.then_some((p.at, p.end))).collect();
    // Code first: placed code is excluded when HTML is placed.
    for pass_code in [true, false] {
        let mut done = vec![false; pending.len()];
        for i in 0..pending.len() {
            if done[i] || pending[i].code.is_some() != pass_code {
                continue;
            }
            let group: Vec<usize> = (i..pending.len())
                .filter(|&j| {
                    pending[j].block == pending[i].block && pending[j].code == pending[i].code && pending[j].literal == pending[i].literal
                })
                .collect();
            for &j in &group {
                done[j] = true;
            }
            if group.iter().all(|&j| pending[j].ok) {
                continue;
            }
            let (bs, be) = pending[i].block_range;
            let code_now: Vec<(usize, usize)> = t.literal.clone();
            let mut cands: Vec<(usize, usize)> = Vec::new();
            let mut p = bs;
            while p < be.min(text.len()) {
                let hit = match pending[i].code {
                    Some(n) => (b[p] == b'`' && !escaped(b, p) && !inside(defs, p))
                        .then(|| code_span_at(text, p, n, &pending[i].literal))
                        .flatten(),
                    None => (b[p] == b'<'
                        && !escaped(b, p)
                        && !inside(&code_now, p)
                        && !inside(defs, p)
                        && literal_at(text, starts, p, &pending[i].literal))
                    .then(|| p + pending[i].literal.len()),
                };
                match hit {
                    Some(e) if e <= be => {
                        cands.push((p, e));
                        p = e.max(p + 1);
                    }
                    _ => p += 1,
                }
            }
            if cands.len() == group.len() {
                for (k, &j) in group.iter().enumerate() {
                    placed[j] = Some(cands[k]);
                }
            } else {
                for &j in &group {
                    if !pending[j].ok {
                        placed[j] = None;
                    }
                }
            }
        }
        if pass_code {
            for (j, p) in pending.iter().enumerate() {
                if p.code.is_some() {
                    if let Some(r) = placed[j] {
                        t.literal.push(r);
                    }
                }
            }
        }
    }
    for (j, p) in pending.into_iter().enumerate() {
        if p.code.is_none() {
            t.html.push(match placed[j] {
                Some((at, _)) => html_node(text, starts, at, p.literal),
                None => html_node_unknown(p.literal),
            });
        }
    }
    t.html.sort_by_key(|h| if h.known { h.range.0 } else { usize::MAX });
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

/// Canonical lower-case 8-4-4-4-12 hexadecimal UUID.
fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| {
            p.len() == n && p.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
}

/// An image address in one of the exact attachment shapes of
/// [`EXEMPT_IMAGE_HOSTS`]. Parsed with the `url` crate; the address must be
/// exactly the parser's serialization (so nothing was normalized away: dot
/// segments, letter case, a default port) and carry no credentials, port,
/// query, fragment or percent-encoding.
fn image_exempt(raw_url: &str) -> bool {
    if raw_url.contains(['%', '\\']) || raw_url.chars().any(char::is_whitespace) {
        return false;
    }
    let Ok(u) = url::Url::parse(raw_url) else { return false };
    if u.as_str() != raw_url
        || u.scheme() != "https"
        || !u.username().is_empty()
        || u.password().is_some()
        || u.port().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return false;
    }
    let segs: Vec<&str> = u.path().split('/').skip(1).collect();
    if segs.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return false;
    }
    match (u.host_str(), segs.as_slice()) {
        (Some("github.com"), ["user-attachments", "assets", id]) => is_uuid(id),
        (Some("user-images.githubusercontent.com"), [user, file]) => {
            let Some((stem, ext)) = file.rsplit_once('.') else { return false };
            let Some((num, id)) = stem.split_once('-') else { return false };
            let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
            digits(user) && digits(num) && is_uuid(id) && ATTACHMENT_EXTENSIONS.contains(&ext)
        }
        _ => false,
    }
}

/// Why a math source is flagged: the macros outside [`MATH_VISIBLE`] (or in
/// [`MATH_ALWAYS_FLAG`]), CSS-like optional arguments, and comments (an
/// unescaped `%` to the end of its line; `\%` is a percent sign); `None`
/// when it only draws visible symbols.
fn math_problems(src: &str) -> Option<String> {
    let b = src.as_bytes();
    let mut bad: Vec<String> = Vec::new();
    let mut comments: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        // MathJax skips everything from an unescaped `%` to the line end.
        if b[i] == b'%' {
            let end = src[i..].find('\n').map(|x| i + x).unwrap_or(src.len());
            comments.push(src[i..end].to_string());
            i = end;
            continue;
        }
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
    let mut why = Vec::new();
    if !bad.is_empty() {
        why.push(format!("macros {}", bad.join(", ")));
    }
    for c in comments {
        why.push(format!("comment {:?}", shown(&c)));
    }
    (!why.is_empty()).then(|| why.join("; "))
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

/// `<!--` outside every HTML node the tree found (a backstop: GitHub could
/// start a comment where comrak reads text).
fn text_comments(m: &str, text: &str, html: &[(usize, usize)], out: &mut Vec<Raw>) {
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
        if !inside(html, p) {
            out.push(raw(Kind::HtmlComment, p, end, shown(slice(text, p, end))));
        }
        from = end;
    }
}

/// One parsed tag: name (lower case), whether it closes, attributes (name
/// in lower case, value), and its end.
struct Tag {
    name: String,
    closing: bool,
    attrs: Vec<(String, String)>,
    end: usize,
    unterminated: bool,
}

/// Parse the tag at `i` (`m[i] == '<'`, `m` an HTML node's literal); `None`
/// when it is not a tag. A tag without its closing `>` (or with an
/// unterminated quoted value) is returned up to the end of its line and
/// marked `unterminated`: in an HTML node the sanitizer drops what follows.
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
            Some(b'>') => return Some(Tag { name, closing, attrs, end: j + 1, unterminated: false }),
            Some(b'/') if b.get(j + 1) == Some(&b'>') => {
                return Some(Tag { name, closing, attrs, end: j + 2, unterminated: false })
            }
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
    Some(Tag { name, closing, attrs, end: m[i..].find('\n').map(|x| i + x).unwrap_or(m.len()), unterminated: true })
}

/// The end of the `<details>` element whose open tag starts at source byte
/// `p`: after its matching `</details>` (nested ones counted), or the end.
fn details_end(m: &str, p: usize) -> usize {
    let lower = m.to_ascii_lowercase();
    let mut depth = 1;
    let mut i = p + 8;
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
                    return lower[c..].find('>').map(|x| c + x + 1).unwrap_or(m.len());
                }
            }
            _ => break,
        }
    }
    m.len()
}

/// Visible text between source byte `from` and the closing `</name>`
/// (tags removed), read from the masked source (a label follows an inline
/// `<a>` node as text).
fn element_text(m: &str, from: usize, name: &str) -> Option<String> {
    let lower = m[from.min(m.len())..].to_ascii_lowercase();
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

/// Raw HTML, read from every HTML node of the tree: comments; tags outside
/// [`VISIBLE_TAGS`]; attributes outside [`attribute_allowed`]; incomplete or
/// unterminated tags; `<details>` without `open`; `<a href>` whose text is
/// not its address; `<img>` alt text and addresses; nested `<sub>`/`<sup>`;
/// declarations, processing instructions and CDATA (removed by the
/// sanitizer). `m` is the masked source (for labels and `<details>` ends),
/// `text` the unmasked source.
fn tree_html(nodes: &[HtmlNode], m: &str, text: &str, out: &mut Vec<Raw>) -> Vec<(usize, usize)> {
    let mut small_depth = 0i32;
    let mut tags: Vec<(usize, usize)> = Vec::new();
    for node in nodes {
        let lit = node.literal.as_str();
        let from = out.len();
        let b = lit.as_bytes();
        let lower = lit.to_ascii_lowercase();
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'<' {
                i += 1;
                continue;
            }
            let rest = &lower[i..];
            if rest.starts_with("<!--") {
                let r = &lit[i + 4..];
                let end = if r.starts_with('>') {
                    i + 5
                } else if r.starts_with("->") {
                    i + 6
                } else {
                    r.find("-->").map(|e| i + 4 + e + 3).unwrap_or(lit.len())
                };
                out.push(raw(Kind::HtmlComment, node.src(i), node.src(end), shown(&lit[i..end])));
                i = end;
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
                let end = end.unwrap_or(lit.len());
                out.push(raw(Kind::HtmlTag, node.src(i), node.src(end), shown(&lit[i..end])));
                i = end.max(i + 1);
                continue;
            }
            let Some(tag) = parse_tag(lit, i) else {
                i += 1;
                continue;
            };
            let (sa, se) = (node.src(i), node.src(tag.end));
            if node.known {
                tags.push((sa, se));
            }
            let src = shown(&lit[i..tag.end]);
            let name = tag.name.as_str();
            if tag.unterminated {
                out.push(raw(Kind::HtmlTag, sa, se, format!("unterminated tag: {src}")));
                i = tag.end.max(i + 1);
                continue;
            }
            if !VISIBLE_TAGS.contains(&name) {
                out.push(raw(Kind::HtmlTag, sa, se, src));
                i = tag.end;
                continue;
            }
            if name == "details" && !tag.closing && !tag.attrs.iter().any(|(a, _)| a == "open") {
                let end = details_end(m, sa);
                out.push(raw(Kind::Details, sa, end, shown(slice(text, sa, end))));
            }
            let mut bad_attr = false;
            for (a, v) in &tag.attrs {
                match (name, a.as_str()) {
                    ("a", "href") => {
                        let label = element_text(m, se, "a").unwrap_or_default();
                        if !same_destination(&label, v) {
                            out.push(raw(Kind::LinkDestination, sa, se, format!("{} (shown as {:?})", shown(v), shown(label.trim()))));
                        }
                    }
                    ("img", "src") => {
                        if !image_exempt(v) {
                            out.push(raw(Kind::ImageSource, sa, se, shown(v)));
                        }
                    }
                    ("img", "alt") => {
                        if !v.trim().is_empty() {
                            out.push(raw(Kind::ImageAlt, sa, se, shown(v)));
                        }
                    }
                    _ if attribute_allowed(name, a) => {
                        let value = decode_attribute(v);
                        if !attribute_value_allowed(a, &value) {
                            out.push(raw(
                                Kind::HtmlTag,
                                sa,
                                se,
                                format!("attribute {a}={:?} (decoded) is not a value that only changes layout: {src}", shown(&value)),
                            ));
                        }
                    }
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
                out.push(raw(Kind::HtmlTag, sa, se, src));
            }
            i = tag.end.max(i + 1);
        }
        if !node.known {
            // Not placed in the source: the whole location, said plainly.
            for f in &mut out[from..] {
                f.at = 0;
                f.end = text.len();
                f.text = format!("position unknown (the source has this more than once, or comrak's position could not be matched): {}", f.text);
            }
        }
    }
    tags
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
    for (kind, s, e) in definition_spans(m) {
        out.push(raw(kind, s, e, shown(slice(text, s, e))));
    }
}

/// The reference and footnote definitions of `m`: (kind, start, end).
fn definition_spans(m: &str) -> Vec<(Kind, usize, usize)> {
    let mut spans = Vec::new();
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
        spans.push((kind, s + at, end));
    }
    spans
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

/// Every finding of one Markdown text (normalized).
fn scan_markdown_raw(text: &str) -> Vec<Raw> {
    let mut out = Vec::new();
    let defs: Vec<(usize, usize)> = definition_spans(text).into_iter().map(|(_, s, e)| (s, e)).collect();
    let tree = tree_scan(text, &defs, &mut out);
    let m = masked(text, &tree.literal);
    let tags = tree_html(&tree.html, &m, text, &mut out);
    let html_ranges: Vec<(usize, usize)> = tree.html.iter().filter(|h| h.known).map(|h| h.range).collect();
    invisible_findings(text, &units(text, &tree.literal, true, &tags, &html_ranges), &mut out);
    // An HTML node that could not be placed: its source is read as Markdown
    // above; its literal is also read as HTML here, since the context is
    // unknown. A finding from either counts, over the whole location.
    for node in tree.html.iter().filter(|h| !h.known) {
        let lit = node.literal.as_str();
        let tag_spans: Vec<(usize, usize)> = (0..lit.len())
            .filter(|&i| lit.as_bytes()[i] == b'<')
            .filter_map(|i| parse_tag(lit, i).map(|t| (i, t.end)))
            .collect();
        let mut found = Vec::new();
        invisible_findings(lit, &units(lit, &[], true, &tag_spans, &[(0, lit.len())]), &mut found);
        for mut f in found.into_iter().filter(|f| f.kind == Kind::InvisibleEntity) {
            f.at = 0;
            f.end = text.len();
            f.text = format!("position unknown (read as HTML): {}", f.text);
            out.push(f);
        }
    }
    text_comments(&m, text, &html_ranges, &mut out);
    definitions(&m, text, &mut out);
    hiding_macros(&m, text, &tree.math, &mut out);
    out.sort_by_key(|r| (r.at, r.kind, r.end));
    // The tree and a text check can report one thing twice; a finding whose
    // position is unknown is kept apart from the others.
    let unknown = |r: &Raw| r.text.starts_with("position unknown");
    out.dedup_by(|x, y| x.kind == y.kind && x.at == y.at && unknown(x) == unknown(y) && (!unknown(x) || x.text == y.text));
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
    invisible_findings(&norm.text, &units(&norm.text, &[], false, &[], &[]), &mut found);
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

mod charref;
mod entities;

#[cfg(test)]
mod tests;
