//! §5's *"strip a leading repetition of the supplied prefix"*.
//!
//! `aibo_core::prompts::post_process` implements the conversational half of §5's
//! two-layer anti-preamble defence — opening phrases, unrequested code fences,
//! wrapping quotes. It does **not** implement the half §5 calls out as
//! *observed, not hypothetical*:
//!
//! > Probing the Codex endpoint on 2026-07-26 … **`gpt-5.6-luna`** returned
//! > `"The deployment should be carefully monitored."` for the prefix `"The
//! > deployment should be"` — it repeats the entire prefix. *"So the filter
//! > must strip a leading repetition of the supplied prefix, not just
//! > conversational preambles."*
//!
//! Inserted at a caret, that duplicates the user's own text. This module is the
//! missing rule.
//!
//! **Where this belongs.** In `aibo_core::prompts::post_process`, beside the
//! other three rules and covered by the same golden tests. It is here because
//! `aibo-core` is owned elsewhere, and shipping the insertion path without the
//! rule means shipping a known text-duplication bug. Fold it in when the two
//! crates can be edited together — see the report.

use aibo_core::context::estimate_tokens;
use aibo_core::types::Surface;

/// The shortest overlap worth acting on, in §4's character-class token
/// estimate.
///
/// Measured in *tokens* rather than characters on purpose. A character
/// threshold calibrated on English (`"completed by"` is twelve characters and
/// carries almost nothing) is three times too strict for Japanese, where four
/// characters can be a whole clause — the same `bytes/4` mistake §4 spends a
/// paragraph on. Four tokens is roughly sixteen ASCII characters or four CJK
/// ones.
const MIN_OVERLAP_TOKENS: usize = 4;

/// How far back from the end of the source the tail search looks.
///
/// A bound, not a heuristic. Without it the search is O(n²) — one
/// [`estimate_tokens`] call per candidate tail — and `source` on a Transform is
/// the user's whole selection, which §13 allows up to 200 000 characters. That
/// cost 42 seconds on a 100 000-character selection before this existed, which
/// is not a tuning issue but a hang.
///
/// 512 is generous for what the rule is for: §5 caps the Complete prefix at
/// ~800 characters, and the failure it describes is a model restating the last
/// clause it was given, not the last page.
const MAX_TAIL_SEARCH_CHARS: usize = 512;

/// Remove a leading restatement of `source` from `output` (§5).
///
/// Applies only to the insertion surfaces. `Ask` output is rendered in the
/// panel, where an echoed prefix is merely verbose rather than a defect pasted
/// into a document.
///
/// Two shapes are handled, both observed in the §5 probe:
///
/// * the model restates the whole prefix and continues — strip the prefix;
/// * the model restates the **tail** of the prefix (the last clause, say) and
///   continues — strip the overlapping tail.
pub fn strip_prefix_repetition(surface: Surface, source: &str, output: &str) -> String {
    if !matches!(surface, Surface::Complete | Surface::Transform) {
        return output.to_owned();
    }
    let source_trimmed = source.trim();
    if estimate_tokens(source_trimmed) < MIN_OVERLAP_TOKENS {
        return output.to_owned();
    }

    let leading: String = output.chars().take_while(|c| c.is_whitespace()).collect();
    let body = output.trim_start();

    // Whole-prefix restatement.
    if let Some(rest) = body.strip_prefix(source_trimmed) {
        return format!("{leading}{}", rest.trim_start());
    }

    // Tail restatement: the longest suffix of the source that the output starts
    // with, searched over a bounded window. Iterating `char_indices` keeps every
    // slice on a character boundary — §5 is explicit that byte-slicing a
    // Japanese string is how this class of code panics.
    let window_start = source_trimmed
        .char_indices()
        .rev()
        .take(MAX_TAIL_SEARCH_CHARS)
        .last()
        .map_or(0, |(index, _)| index);

    for (offset, _) in source_trimmed[window_start..].char_indices() {
        let tail = &source_trimmed[window_start + offset..];
        if estimate_tokens(tail) < MIN_OVERLAP_TOKENS {
            break;
        }
        if let Some(rest) = body.strip_prefix(tail) {
            return format!("{leading}{}", rest.trim_start());
        }
    }

    output.to_owned()
}

/// Whether the filter would change the output — the quality-drift signal §5
/// asks to be logged.
pub fn repeats_prefix(surface: Surface, source: &str, output: &str) -> bool {
    strip_prefix_repetition(surface, source, output) != output
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "The deployment should be";

    #[test]
    fn the_measured_luna_failure_is_stripped() {
        // §5's probe, verbatim.
        let out = strip_prefix_repetition(
            Surface::Complete,
            PREFIX,
            "The deployment should be carefully monitored.",
        );
        assert_eq!(out, "carefully monitored.");
        assert!(repeats_prefix(
            Surface::Complete,
            PREFIX,
            "The deployment should be carefully monitored."
        ));
    }

    #[test]
    fn a_clean_continuation_is_untouched() {
        let out = strip_prefix_repetition(Surface::Complete, PREFIX, "completed by Friday.");
        assert_eq!(out, "completed by Friday.");
        assert!(!repeats_prefix(
            Surface::Complete,
            PREFIX,
            "completed by Friday."
        ));
    }

    #[test]
    fn only_the_tail_of_a_long_prefix_needs_to_match() {
        let source = "Dear Ms Tanaka,\n\nThank you for your message. The deployment should be";
        let out = strip_prefix_repetition(
            Surface::Complete,
            source,
            "The deployment should be completed by Friday.",
        );
        assert_eq!(out, "completed by Friday.");
    }

    #[test]
    fn ask_output_is_never_touched() {
        let out = strip_prefix_repetition(
            Surface::Ask,
            PREFIX,
            "The deployment should be carefully monitored.",
        );
        assert_eq!(out, "The deployment should be carefully monitored.");
    }

    #[test]
    fn a_short_prefix_is_not_worth_matching() {
        // "Hello" is far too short to distinguish a repetition from a genuine
        // continuation that happens to start with the same word.
        let out = strip_prefix_repetition(Surface::Complete, "Hello", "Hello there");
        assert_eq!(out, "Hello there");
    }

    #[test]
    fn japanese_is_sliced_on_character_boundaries() {
        let source = "本日の会議の議事録をまとめました。次のステップは";
        let output = "次のステップは、来週までに設計書を確定することです。";
        let out = strip_prefix_repetition(Surface::Complete, source, output);
        assert_eq!(out, "、来週までに設計書を確定することです。");
    }

    #[test]
    fn leading_whitespace_of_the_output_survives() {
        let out = strip_prefix_repetition(
            Surface::Transform,
            PREFIX,
            "  The deployment should be fine.",
        );
        assert_eq!(out, "  fine.");
    }
}
