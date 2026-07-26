//! The deterministic corpus. **This module is the whole point of the app.**
//!
//! A test target is only useful if the expected answer is known exactly, so the
//! text in each field is a constant here and the app prints its measurements at
//! launch. A harness can then assert on numbers rather than eyeball a window.
//!
//! Three counts, and they are all different, which is exactly why they are all
//! printed:
//!
//! - **bytes** — what Rust's `str::len` gives you.
//! - **UTF-16 code units** — what `kAXSelectedTextRangeAttribute` and
//!   Windows `SendInput` use. §8 names the AX range as a `CFRange`, and `CFRange`
//!   on a `CFString` is UTF-16. Every off-by-one in insert-at-caret lives here.
//! - **grapheme clusters** — what a user calls "characters", and what §5's
//!   middle-out truncation must not split.
//!
//! Nothing here depends on AppKit, so the counts are testable in CI on any
//! platform (§18 tier 1) even though the window is macOS-only.

/// One field's seed text and why it is that text.
pub struct Sample {
    /// Accessibility identifier set on the control. Stable join key for a harness.
    pub id: &'static str,
    /// Human label, also set as the accessibility label.
    pub label: &'static str,
    /// The exact seed text.
    pub text: &'static str,
    /// What this sample is for.
    pub purpose: &'static str,
}

/// Single-line ASCII. The baseline: if this is not readable, nothing is.
pub const SINGLE_LINE: Sample = Sample {
    id: "aibo.single-line",
    label: "Single line ASCII",
    text: "The quick brown fox jumps over the lazy dog.",
    purpose: "baseline NSTextField; 44 bytes == 44 UTF-16 units == 44 graphemes",
};

/// Japanese in a single-line field.
pub const SINGLE_LINE_JA: Sample = Sample {
    id: "aibo.single-line-ja",
    label: "Single line Japanese",
    text: "明日の会議は三時からです。",
    purpose: "BMP non-ASCII: bytes != UTF-16 units, UTF-16 units == graphemes",
};

/// The nasty one. Every count differs.
///
/// - `👨‍👩‍👧‍👦` — ZWJ family: 4 code points + 3 ZWJ, **11 UTF-16 units, 1 grapheme**.
/// - `🇯🇵` — regional indicator pair: **4 UTF-16 units, 1 grapheme**.
/// - `é` written as `e` + U+0301 — **2 UTF-16 units, 1 grapheme**.
/// - `𠀋` — a CJK extension-B ideograph: **2 UTF-16 units** (a surrogate pair).
///
/// A range arithmetic bug that survives ASCII and survives Japanese will not
/// survive this line.
pub const UNICODE_TRAPS: Sample = Sample {
    id: "aibo.unicode-traps",
    label: "Unicode traps",
    text: "family 👨‍👩‍👧‍👦 flag 🇯🇵 combining e\u{0301} astral 𠀋 end",
    purpose: "surrogate pairs, ZWJ sequences, regional indicators, combining marks",
};

/// Multi-line, mixed script, with leading and trailing whitespace that matters.
///
/// §5: *"Preserve leading and trailing whitespace exactly — the result is pasted
/// back over a selection and a stripped space is a visible bug."* The leading
/// four spaces and the ideographic space `U+3000` on line 3 are there to be
/// round-tripped, not to look nice.
pub const MULTI_LINE: Sample = Sample {
    id: "aibo.multi-line",
    label: "Multi line body",
    text: "    Indented first line, four leading spaces.\n\
           Second line, plain ASCII.\n\
           \u{3000}三行目は全角スペースで始まります。\n\
           Fourth line ends with two trailing spaces.  \n\
           \n\
           Sixth line after a blank one.",
    purpose: "newlines, leading spaces, U+3000, trailing spaces, a blank line",
};

/// A password field. It exists to make **secure input mode** reproducible.
///
/// §8: *"`IsSecureEventInputEnabled()` — password fields, Terminal, and password
/// managers block keystroke synthesis and AX reads. Other apps can leave it
/// stuck globally."* and *"Paste-based insert fails silently with no diagnosable
/// cause unless you detect and explain it."*
///
/// Click into this field and every S2 read and every S4 insert should fail in a
/// specific, recognisable way. That is the test.
pub const SECURE: Sample = Sample {
    id: "aibo.secure",
    label: "Secure field (enables secure input mode)",
    text: "",
    purpose: "reproduce EnableSecureEventInput; S2 reads and S4 inserts must fail visibly",
};

/// Every sample, in window order.
pub const ALL: &[Sample] = &[
    SINGLE_LINE,
    SINGLE_LINE_JA,
    UNICODE_TRAPS,
    MULTI_LINE,
    SECURE,
];

/// The three counts a harness asserts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// UTF-8 bytes.
    pub bytes: usize,
    /// UTF-16 code units — the unit `kAXSelectedTextRangeAttribute` speaks.
    pub utf16: usize,
    /// Unicode scalar values.
    pub chars: usize,
    /// Extended grapheme clusters, approximated — see the note below.
    pub graphemes: usize,
}

/// Measure a string.
///
/// The grapheme count is a **deliberate approximation**: it counts scalars that
/// are not combining marks, ZWJ, variation selectors, regional-indicator
/// continuations, or ZWJ continuations. This app has no `unicode-segmentation`
/// dependency on purpose — a test fixture that pulls in the same crate the code
/// under test uses can agree with it and still both be wrong.
///
/// It is exact for [`ALL`]. If you add a sample it does not handle, fix the
/// sample or fix this, and say which in the commit.
pub fn counts(text: &str) -> Counts {
    let mut graphemes = 0usize;
    let mut previous_was_ri = false;
    let mut join_next = false;

    for ch in text.chars() {
        let code = ch as u32;
        let is_combining = matches!(code, 0x0300..=0x036F | 0xFE00..=0xFE0F | 0x20D0..=0x20FF);
        let is_zwj = code == 0x200D;
        let is_ri = (0x1F1E6..=0x1F1FF).contains(&code);

        if is_zwj {
            join_next = true;
            previous_was_ri = false;
            continue;
        }
        if is_combining || join_next {
            join_next = false;
            previous_was_ri = false;
            continue;
        }
        if is_ri && previous_was_ri {
            previous_was_ri = false;
            continue;
        }
        previous_was_ri = is_ri;
        graphemes += 1;
    }

    Counts {
        bytes: text.len(),
        utf16: text.encode_utf16().count(),
        chars: text.chars().count(),
        graphemes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_counts_agree() {
        let c = counts(SINGLE_LINE.text);
        assert_eq!(c.bytes, 44);
        assert_eq!(c.utf16, 44);
        assert_eq!(c.graphemes, 44);
    }

    #[test]
    fn japanese_is_three_bytes_and_one_utf16_unit_per_char() {
        let c = counts(SINGLE_LINE_JA.text);
        assert_eq!(c.chars, 13);
        assert_eq!(c.utf16, 13, "BMP: one UTF-16 unit per scalar");
        assert_eq!(c.bytes, 39, "three UTF-8 bytes per scalar");
        assert_eq!(c.graphemes, 13);
    }

    #[test]
    fn the_trap_line_has_three_different_counts() {
        let c = counts(UNICODE_TRAPS.text);
        assert!(
            c.bytes > c.utf16 && c.utf16 > c.chars && c.chars > c.graphemes,
            "the whole point of this sample is bytes > utf16 > chars > graphemes, got {c:?}"
        );
    }

    #[test]
    fn the_family_emoji_is_one_grapheme_and_eleven_utf16_units() {
        let c = counts("👨‍👩‍👧‍👦");
        assert_eq!(c.graphemes, 1);
        assert_eq!(c.utf16, 11, "4 surrogate pairs + 3 ZWJ");
    }

    #[test]
    fn the_flag_is_one_grapheme_and_four_utf16_units() {
        let c = counts("🇯🇵");
        assert_eq!(c.graphemes, 1);
        assert_eq!(c.utf16, 4);
    }

    #[test]
    fn multiline_whitespace_is_exactly_as_declared() {
        // If this ever fails, someone reflowed the constant and every
        // whitespace-preservation assertion built on it became meaningless.
        assert!(MULTI_LINE.text.starts_with("    Indented"));
        assert!(MULTI_LINE.text.contains("\u{3000}三行目"));
        assert!(MULTI_LINE.text.contains("trailing spaces.  \n"));
        assert!(MULTI_LINE.text.contains("\n\n"));
    }

    #[test]
    fn identifiers_are_unique() {
        let mut ids: Vec<&str> = ALL.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "accessibility identifiers must be unique");
    }
}
