//! Port of gateway/platforms/signal_format.py.
//!
// Public API is ahead of its callers (the Signal send path wires it).
#![allow(dead_code)]
//!
//! One public function: [`markdown_to_signal`]. Signal does not render
//! markdown, it uses `bodyRanges` (exposed by signal-cli as `textStyle` /
//! `textStyles` params) of the form `start:length:STYLE`. This converts a
//! markdown string into plain text plus that list of style strings.
//!
//! Positions are measured in UTF-16 code units, because that is what the
//! Signal protocol uses. Supported styles: BOLD, ITALIC, STRIKETHROUGH,
//! MONOSPACE.
//!
//! Faithfulness notes (this mirrors the Python step by step):
//!
//!  * Python indexes and slices strings by Unicode code point, and `len()` is
//!    a code-point count. All the intermediate `styles` offsets are therefore
//!    code-point offsets, so this port carries positions as code-point indices
//!    (operating over `Vec<char>` where slicing is needed) and only converts to
//!    UTF-16 code units at the very end via `char::len_utf16`.
//!  * The inline patterns run in a fixed priority order with overlap
//!    suppression (an `occupied` list), then the surrounding delimiters are
//!    removed and every remaining position is shifted left by the removed
//!    spans. The code-block and heading styles collected earlier get shifted by
//!    the same `_adjust` logic.
//!
//! This crate uses `fancy-regex` (std `regex` has no lookaround) for the italic
//! patterns with lookbehind/lookahead. See media.rs for the same usage.

use std::sync::OnceLock;

use fancy_regex::Regex;

/// Convert markdown to plain text plus a Signal `textStyles` list.
///
/// Returns `(plain_text, ["start:length:STYLE", ...])` where `start`/`length`
/// are UTF-16 code-unit offsets into `plain_text`.
pub fn markdown_to_signal(text: &str) -> (String, Vec<String>) {
    // Collapse 3+ newlines to 2, strip, normalize bullet markers.
    let text = collapse_newlines(text);
    let text = text.trim().to_string();
    let mut text = normalize_bullet_markers(&text);

    // styles carries (cp_start, cp_len, style) with code-point offsets.
    let mut styles: Vec<(usize, usize, &'static str)> = Vec::new();

    // Extract fenced code blocks to MONOSPACE, mutating `text` in place. This
    // mirrors Python's `while match := code_block.search(text)` loop: each pass
    // pulls the first code block, replaces it with its (newline-rstripped)
    // inner text, and records the span. Removing the surrounding ``` fences
    // means the loop always terminates.
    let code_block = code_block_re();
    // A plain `loop` with a `let ... else break`, not `while let`: the capture
    // borrows `text`, and we reassign `text` inside the body. The `let else`
    // ends the borrow before that reassignment; `while let` would hold it across
    // the whole body and fail the borrow check.
    #[allow(clippy::while_let_loop)]
    loop {
        let Ok(Some(cap)) = code_block.captures(&text) else {
            break;
        };
        let whole = cap.get(0).unwrap();
        let g1 = cap.get(1).unwrap();
        let start_byte = whole.start();
        let end_byte = whole.end();
        let inner = g1.as_str().trim_end_matches('\n').to_string();
        // start is a code-point offset into the current text.
        let start_cp = byte_to_cp(&text, start_byte);
        let inner_cp_len = inner.chars().count();
        let new_text = format!("{}{}{}", &text[..start_byte], inner, &text[end_byte..]);
        styles.push((start_cp, inner_cp_len, "MONOSPACE"));
        text = new_text;
    }

    // Headings (^#{1,6}\s+) become BOLD; the marker is stripped. Everything
    // here is done in code-point space, so operate over a char vector.
    let heading = heading_re();
    let tc: Vec<char> = text.chars().collect();
    let mut new_text: Vec<char> = Vec::new();
    let mut last_end_cp = 0usize;
    for (ms_byte, me_byte) in finditer(heading, &text) {
        let ms_cp = byte_to_cp(&text, ms_byte);
        let me_cp = byte_to_cp(&text, me_byte);
        new_text.extend_from_slice(&tc[last_end_cp..ms_cp]);
        // eol = text.find("\n", match.end()); -1 -> len(text).
        let eol_cp = tc[me_cp..]
            .iter()
            .position(|&c| c == '\n')
            .map(|p| me_cp + p)
            .unwrap_or(tc.len());
        let heading_text = &tc[me_cp..eol_cp];
        let start = new_text.len();
        new_text.extend_from_slice(heading_text);
        styles.push((start, heading_text.len(), "BOLD"));
        last_end_cp = eol_cp;
    }
    new_text.extend_from_slice(&tc[last_end_cp..]);
    let text: String = new_text.into_iter().collect();

    // Inline patterns, in priority order, with overlap suppression.
    let patterns: [(&Regex, &'static str); 6] = [
        (bold_star_re(), "BOLD"),
        (bold_underscore_re(), "BOLD"),
        (strike_re(), "STRIKETHROUGH"),
        (mono_re(), "MONOSPACE"),
        (italic_star_re(), "ITALIC"),
        (italic_underscore_re(), "ITALIC"),
    ];

    // all_matches holds (ms, me, g1s, g1e, style) in code-point offsets.
    let mut all_matches: Vec<(usize, usize, usize, usize, &'static str)> = Vec::new();
    let mut occupied: Vec<(usize, usize)> = Vec::new();
    for (pattern, style) in patterns {
        for (ms_b, me_b, g1s_b, g1e_b) in finditer_caps(pattern, &text) {
            let ms = byte_to_cp(&text, ms_b);
            let me = byte_to_cp(&text, me_b);
            // Overlap check works in any consistent index space.
            let overlaps = occupied.iter().any(|&(os, oe)| ms < oe && me > os);
            if !overlaps {
                let g1s = byte_to_cp(&text, g1s_b);
                let g1e = byte_to_cp(&text, g1e_b);
                all_matches.push((ms, me, g1s, g1e, style));
                occupied.push((ms, me));
            }
        }
    }
    all_matches.sort();

    // Removed delimiter spans: the run before group 1 and the run after it.
    let mut removals: Vec<(usize, usize)> = Vec::new();
    for &(ms, me, g1s, g1e, _) in &all_matches {
        if g1s > ms {
            removals.push((ms, g1s - ms));
        }
        if me > g1e {
            removals.push((g1e, me - g1e));
        }
    }
    removals.sort();

    let adjust = |pos: usize| -> usize {
        let mut shift = 0usize;
        for &(remove_pos, remove_len) in &removals {
            if remove_pos < pos {
                shift += remove_len.min(pos - remove_pos);
            } else {
                break;
            }
        }
        pos - shift
    };

    // Shift the code-block / heading styles by the removed spans.
    let mut adjusted_prior: Vec<(usize, usize, &'static str)> = Vec::new();
    for &(start, length, style) in &styles {
        let new_start = adjust(start);
        let new_end = adjust(start + length);
        if new_end > new_start {
            adjusted_prior.push((new_start, new_end - new_start, style));
        }
    }

    // Rebuild text with delimiters stripped, recording inline style spans.
    let tc: Vec<char> = text.chars().collect();
    let mut result: Vec<char> = Vec::new();
    let mut last_end = 0usize;
    let mut inline_styles: Vec<(usize, usize, &'static str)> = Vec::new();
    for &(ms, me, g1s, g1e, style) in &all_matches {
        result.extend_from_slice(&tc[last_end..ms]);
        let pos = result.len();
        let inner = &tc[g1s..g1e];
        result.extend_from_slice(inner);
        inline_styles.push((pos, inner.len(), style));
        last_end = me;
    }
    result.extend_from_slice(&tc[last_end..]);
    let rc = result;
    let text: String = rc.iter().collect();

    let mut styles = adjusted_prior;
    styles.extend(inline_styles);
    styles.sort();

    // Final UTF-16 offset computation.
    let total_cp = rc.len();
    let mut style_strings: Vec<String> = Vec::new();
    for (cp_start, cp_len, style_type) in styles {
        if cp_start + cp_len > total_cp {
            continue;
        }
        let u16_start = utf16_len(&rc[..cp_start]);
        let u16_len = utf16_len(&rc[cp_start..cp_start + cp_len]);
        style_strings.push(format!("{u16_start}:{u16_len}:{style_type}"));
    }

    (text, style_strings)
}

/// Length of a code-point slice in UTF-16 code units (matches Python
/// `len(s.encode("utf-16-le")) // 2`).
fn utf16_len(chars: &[char]) -> usize {
    chars.iter().map(|c| c.len_utf16()).sum()
}

/// Code-point index of a byte offset (Python indexes strings by code point).
fn byte_to_cp(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

/// re.sub(r"\n{3,}", "\n\n", text).
fn collapse_newlines(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    re.replace_all(text, "\n\n").into_owned()
}

/// Replace markdown bullet markers with plain Unicode bullets, preserving
/// fenced code blocks verbatim (list-looking lines inside code are code).
///
/// Mirrors Python `re.split(r"(```.*?```)", source, flags=re.DOTALL)`: the
/// capturing split keeps the code fences as the odd-index parts, so only the
/// prose parts (even indices) get the bullet substitution.
fn normalize_bullet_markers(source: &str) -> String {
    static SPLIT_RE: OnceLock<Regex> = OnceLock::new();
    static BULLET_RE: OnceLock<Regex> = OnceLock::new();
    let split_re = SPLIT_RE.get_or_init(|| Regex::new(r"(?s)```.*?```").unwrap());
    let bullet_re = BULLET_RE.get_or_init(|| Regex::new(r"(?m)^([ \t]{0,3})[-*+]\s+").unwrap());

    let mut out = String::new();
    let mut last = 0usize;
    for (ms, me) in finditer(split_re, source) {
        // Prose segment before this code fence.
        let prose = &source[last..ms];
        out.push_str(&bullet_re.replace_all(prose, "${1}\u{2022} "));
        // Code fence, kept byte-for-byte.
        out.push_str(&source[ms..me]);
        last = me;
    }
    let prose = &source[last..];
    out.push_str(&bullet_re.replace_all(prose, "${1}\u{2022} "));
    out
}

// ── regex definitions ────────────────────────────────────────────────────────

fn code_block_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)```[a-zA-Z0-9_+-]*\n?(.*?)```").unwrap())
}
fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^#{1,6}\s+").unwrap())
}
fn bold_star_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)\*\*(.+?)\*\*").unwrap())
}
fn bold_underscore_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)__(.+?)__").unwrap())
}
fn strike_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)~~(.+?)~~").unwrap())
}
fn mono_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`(.+?)`").unwrap())
}
fn italic_star_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?<!\*)\*(?!\*| )(.+?)(?<!\*)\*(?!\*)").unwrap())
}
fn italic_underscore_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?<!\w)_(?!_)(.+?)(?<!_)_(?!\w)").unwrap())
}

// ── finditer helpers (non-overlapping, leftmost, like Python re.finditer) ─────

/// Non-overlapping (start_byte, end_byte) matches over `text`.
fn finditer(re: &Regex, text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Ok(Some(m)) = re.find_from_pos(text, pos) {
        out.push((m.start(), m.end()));
        pos = m.end().max(m.start() + 1);
    }
    out
}

/// Non-overlapping matches with the whole span and group-1 span, as byte
/// offsets `(ms, me, g1s, g1e)`. All patterns here have a mandatory group 1.
fn finditer_caps(re: &Regex, text: &str) -> Vec<(usize, usize, usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Ok(Some(c)) = re.captures_from_pos(text, pos) {
        let whole = c.get(0).unwrap();
        let g1 = c.get(1).unwrap();
        out.push((whole.start(), whole.end(), g1.start(), g1.end()));
        pos = whole.end().max(whole.start() + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styles(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Golden vectors captured from the real Python markdown_to_signal.
    #[test]
    fn golden_vectors() {
        let cases: Vec<(&str, &str, Vec<&str>)> = vec![
            // bold **, italic *, strikethrough ~~, inline code `
            (
                "**bold** and *italic* and ~~strike~~ and `code`",
                "bold and italic and strike and code",
                vec![
                    "0:4:BOLD",
                    "9:6:ITALIC",
                    "20:6:STRIKETHROUGH",
                    "31:4:MONOSPACE",
                ],
            ),
            // bold __ and italic _
            (
                "__also bold__ and _also italic_",
                "also bold and also italic",
                vec!["0:9:BOLD", "14:11:ITALIC"],
            ),
            // no markup at all
            ("no markup here at all", "no markup here at all", vec![]),
            // single heading
            (
                "# Heading one\nbody text",
                "Heading one\nbody text",
                vec!["0:11:BOLD"],
            ),
            // two headings
            (
                "## Sub\ntext\n### Deep heading",
                "Sub\ntext\nDeep heading",
                vec!["0:3:BOLD", "9:12:BOLD"],
            ),
            // bullet normalization for -, *, +
            (
                "- first\n- second\n* third\n+ fourth",
                "\u{2022} first\n\u{2022} second\n\u{2022} third\n\u{2022} fourth",
                vec![],
            ),
            // fenced code block -> MONOSPACE (inner newline-rstripped)
            (
                "```python\nprint('hi')\n```",
                "print('hi')",
                vec!["0:11:MONOSPACE"],
            ),
            // 3+ newlines collapse to 2
            ("text\n\n\n\n\nmore", "text\n\nmore", vec![]),
            // multibyte emoji inside bold proves UTF-16 offset math
            (
                "**bold with emoji \u{1f389} inside** tail",
                "bold with emoji \u{1f389} inside tail",
                vec!["0:25:BOLD"],
            ),
            // accented (BMP) multibyte text
            (
                "caf\u{e9} **cr\u{e8}me** br\u{fb}l\u{e9}e",
                "caf\u{e9} cr\u{e8}me br\u{fb}l\u{e9}e",
                vec!["5:5:BOLD"],
            ),
            // adjacent bold + italic
            ("**a** *b*", "a b", vec!["0:1:BOLD", "2:1:ITALIC"]),
            // triple markers: bold wins, keeps inner asterisks
            ("***both***", "*both*", vec!["0:5:BOLD"]),
            // inline code and a fenced block side by side
            (
                "a `mono` b ```py\ncode\n``` c",
                "a mono b code c",
                vec!["2:4:MONOSPACE", "9:4:MONOSPACE"],
            ),
            // indented / tab-led bullets
            (
                "  - indented bullet\n\t* tab bullet",
                "\u{2022} indented bullet\n\t\u{2022} tab bullet",
                vec![],
            ),
            // underscores inside a word are not italic
            (
                "regular _no_match_word here",
                "regular _no_match_word here",
                vec![],
            ),
            // two bold spans of different flavors
            (
                "start **b1** mid __b2__ end",
                "start b1 mid b2 end",
                vec!["6:2:BOLD", "13:2:BOLD"],
            ),
            // mixed document: heading, bold, bullets, inline code
            (
                "# H\n\nsome **bold**\n\n- item one\n- item two\n\n`inline`",
                "H\n\nsome bold\n\n\u{2022} item one\n\u{2022} item two\n\ninline",
                vec!["0:1:BOLD", "8:4:BOLD", "37:6:MONOSPACE"],
            ),
            // leading emoji shifts the UTF-16 start offset
            (
                "\u{1f389}\u{1f389} **x** \u{1f389}",
                "\u{1f389}\u{1f389} x \u{1f389}",
                vec!["5:1:BOLD"],
            ),
        ];

        for (input, want_text, want_styles) in cases {
            let (got_text, got_styles) = markdown_to_signal(input);
            assert_eq!(got_text, want_text, "text mismatch for input {input:?}");
            assert_eq!(
                got_styles,
                styles(&want_styles),
                "styles mismatch for input {input:?}"
            );
        }
    }
}
