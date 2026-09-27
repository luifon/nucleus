//! HTML character references as the WHATWG HTML standard decodes them
//! (section 13.2.5.72 onward, "character reference state"): numeric
//! references with or without `;` and their replacement rules, and every
//! named reference of [`super::entities::ENTITIES`], including the legacy
//! names that are valid without `;`.
//!
//! GitHub decodes references in two places, with different rules, and the
//! hold decodes each range with its own renderer's rules (decoding more
//! than the renderer is not stricter: it changes the characters the
//! exemptions look at):
//!
//! - Markdown text: CommonMark ([`decode_commonmark_at`]) decodes only
//!   references that end in `;`: `&#` 1–7 digits `;`, `&#x` 1–6 hex digits
//!   `;`, or a name from the table followed by `;`. Code point 0 and
//!   invalid ones become U+FFFD; there is no Windows-1252 remapping.
//! - Raw HTML: the browser ([`decode_at`]), where `;` is optional for
//!   numeric references and the legacy names.

use super::entities::{ENTITIES, MAX_NAME_LEN};

/// Numeric references 0x80–0x9F that the standard replaces (the Windows-1252
/// mapping); the others in that range stay C1 control characters.
const C1_REPLACEMENTS: &[(u32, u32)] = &[
    (0x80, 0x20AC),
    (0x82, 0x201A),
    (0x83, 0x0192),
    (0x84, 0x201E),
    (0x85, 0x2026),
    (0x86, 0x2020),
    (0x87, 0x2021),
    (0x88, 0x02C6),
    (0x89, 0x2030),
    (0x8A, 0x0160),
    (0x8B, 0x2039),
    (0x8C, 0x0152),
    (0x8E, 0x017D),
    (0x91, 0x2018),
    (0x92, 0x2019),
    (0x93, 0x201C),
    (0x94, 0x201D),
    (0x95, 0x2022),
    (0x96, 0x2013),
    (0x97, 0x2014),
    (0x98, 0x02DC),
    (0x99, 0x2122),
    (0x9A, 0x0161),
    (0x9B, 0x203A),
    (0x9C, 0x0153),
    (0x9E, 0x017E),
    (0x9F, 0x0178),
];

/// The character a numeric reference with value `v` produces: U+FFFD for
/// 0, surrogates and values past U+10FFFF; the Windows-1252 mapping for
/// 0x80–0x9F; the code point itself otherwise (noncharacters and controls
/// are parse errors but stay).
pub fn numeric_char(v: u32) -> char {
    if v == 0 || v > 0x10FFFF || (0xD800..=0xDFFF).contains(&v) {
        return '\u{FFFD}';
    }
    if let Some((_, r)) = C1_REPLACEMENTS.iter().find(|(from, _)| *from == v) {
        return char::from_u32(*r).unwrap_or('\u{FFFD}');
    }
    char::from_u32(v).unwrap_or('\u{FFFD}')
}

/// Decode a CommonMark character reference at `i` (`text[i] == '&'`): the
/// characters it produces and its length, or `None` when CommonMark leaves
/// the `&` as text.
pub fn decode_commonmark_at(text: &str, i: usize) -> Option<(Vec<char>, usize)> {
    let b = text.as_bytes();
    let semi = b[i + 1..b.len().min(i + 2 + MAX_NAME_LEN)].iter().position(|c| *c == b';')? + i + 1;
    let body = &text[i + 1..semi];
    let len = semi + 1 - i;
    if let Some(num) = body.strip_prefix('#') {
        let v = if let Some(hex) = num.strip_prefix(['x', 'X']) {
            (!hex.is_empty() && hex.len() <= 6 && hex.bytes().all(|c| c.is_ascii_hexdigit()))
                .then(|| u32::from_str_radix(hex, 16).ok())
                .flatten()?
        } else {
            (!num.is_empty() && num.len() <= 7 && num.bytes().all(|c| c.is_ascii_digit()))
                .then(|| num.parse::<u32>().ok())
                .flatten()?
        };
        let c = if v == 0 { '\u{FFFD}' } else { char::from_u32(v).unwrap_or('\u{FFFD}') };
        return Some((vec![c], len));
    }
    let name = &text[i + 1..semi + 1];
    let k = ENTITIES.binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes())).ok()?;
    Some((ENTITIES[k].1.iter().filter_map(|c| char::from_u32(*c)).collect(), len))
}

/// Decode the character reference that starts at `i` (`text[i] == '&'`) by
/// the WHATWG rules (raw HTML):
/// the characters it produces (one named reference can produce two) and
/// its length in bytes, or `None` when the standard leaves the `&` as text.
/// `in_attribute`: the attribute-value rule, where a named reference
/// without `;` followed by `=` or an ASCII letter or digit stays text.
pub fn decode_at(text: &str, i: usize, in_attribute: bool) -> Option<(Vec<char>, usize)> {
    let b = text.as_bytes();
    let next = *b.get(i + 1)?;
    if next == b'#' {
        let mut j = i + 2;
        let hex = matches!(b.get(j), Some(b'x' | b'X'));
        if hex {
            j += 1;
        }
        let digits_from = j;
        let mut v: u32 = 0;
        while j < b.len() {
            let d = match (hex, b[j]) {
                (_, c @ b'0'..=b'9') => (c - b'0') as u32,
                (true, c @ b'a'..=b'f') => (c - b'a' + 10) as u32,
                (true, c @ b'A'..=b'F') => (c - b'A' + 10) as u32,
                _ => break,
            };
            v = v.saturating_mul(if hex { 16 } else { 10 }).saturating_add(d).min(0x11_0000);
            j += 1;
        }
        if j == digits_from {
            return None; // `&#` or `&#x` without digits: text
        }
        if b.get(j) == Some(&b';') {
            j += 1;
        }
        return Some((vec![numeric_char(v)], j - i));
    }
    if !next.is_ascii_alphanumeric() {
        return None;
    }
    // The longest name in the table that the text starts with.
    let avail = b.len() - i - 1;
    for len in (1..=avail.min(MAX_NAME_LEN)).rev() {
        let cand = &b[i + 1..i + 1 + len];
        let Ok(k) = ENTITIES.binary_search_by(|(n, _)| n.as_bytes().cmp(cand)) else { continue };
        let (name, cps) = ENTITIES[k];
        if in_attribute && !name.ends_with(';') {
            if let Some(&c) = b.get(i + 1 + len) {
                if c == b'=' || c.is_ascii_alphanumeric() {
                    return None;
                }
            }
        }
        return Some((cps.iter().filter_map(|c| char::from_u32(*c)).collect(), 1 + len));
    }
    None
}
