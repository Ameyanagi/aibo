//! The property assertions.
//!
//! > Expected properties rather than exact strings: no preamble, no prefix
//! > repetition, correct language, whitespace preserved, length within bounds,
//! > ends at a sentence boundary. — §5
//!
//! Every check is deliberately *conservative*: it fires only when it is
//! confident. A harness that cries wolf gets ignored after a week, and then the
//! whole point of §5 ("there is no way to tell whether a prompt change made
//! things better") is lost again.
//!
//! These functions are pure and fully unit-tested — this is the part of S9 that
//! belongs in CI (§18 tier 1) and eventually in `aibo-core`.

use unicode_segmentation::UnicodeSegmentation as _;

use crate::fixture::{Fixture, Lang, Surface};

/// One scored property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Property {
    /// No "Sure," / "Here's the" / "承知しました" opener (§5, anti-preamble).
    NoPreamble,
    /// A Complete reply must not restate the prefix it was given.
    NoPrefixRepetition,
    /// A Complete reply must not duplicate the text after the caret.
    NoSuffixDuplication,
    /// The reply is in the fixture's declared language.
    LanguageMatch,
    /// Transform preserves the selection's leading and trailing whitespace.
    WhitespacePreserved,
    /// Transform adds no code fence the input did not have.
    NoAddedCodeFence,
    /// The reply is within the fixture's length cap.
    LengthWithinBounds,
    /// A Complete reply stops at a sentence boundary.
    EndsAtSentenceBoundary,
    /// The reply is not empty.
    NonEmpty,
}

impl Property {
    /// Stable column label for the report table.
    pub fn as_str(self) -> &'static str {
        match self {
            Property::NoPreamble => "no_preamble",
            Property::NoPrefixRepetition => "no_prefix_repetition",
            Property::NoSuffixDuplication => "no_suffix_duplication",
            Property::LanguageMatch => "language_match",
            Property::WhitespacePreserved => "whitespace_preserved",
            Property::NoAddedCodeFence => "no_added_code_fence",
            Property::LengthWithinBounds => "length_within_bounds",
            Property::EndsAtSentenceBoundary => "ends_at_sentence_boundary",
            Property::NonEmpty => "non_empty",
        }
    }

    /// Every property, in report order.
    pub fn all() -> [Property; 9] {
        [
            Property::NonEmpty,
            Property::NoPreamble,
            Property::NoPrefixRepetition,
            Property::NoSuffixDuplication,
            Property::LanguageMatch,
            Property::WhitespacePreserved,
            Property::NoAddedCodeFence,
            Property::LengthWithinBounds,
            Property::EndsAtSentenceBoundary,
        ]
    }
}

/// The verdict for one property on one output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Held.
    Pass,
    /// Violated, with the evidence.
    Fail(String),
    /// Not meaningful for this surface or fixture (e.g. suffix duplication when
    /// the fixture has no suffix). Excluded from the denominator.
    NotApplicable,
}

impl Verdict {
    /// Did this count as a pass?
    pub fn is_pass(&self) -> bool {
        matches!(self, Verdict::Pass)
    }
    /// Was this scored at all?
    pub fn is_scored(&self) -> bool {
        !matches!(self, Verdict::NotApplicable)
    }
}

/// Score every property for one output.
pub fn evaluate(fixture: &Fixture, output: &str) -> Vec<(Property, Verdict)> {
    Property::all()
        .into_iter()
        .map(|property| (property, check(property, fixture, output)))
        .collect()
}

/// Score one property.
pub fn check(property: Property, fixture: &Fixture, output: &str) -> Verdict {
    match property {
        Property::NonEmpty => {
            if output.trim().is_empty() {
                Verdict::Fail("empty reply".into())
            } else {
                Verdict::Pass
            }
        }
        Property::NoPreamble => no_preamble(output),
        Property::NoPrefixRepetition => {
            if fixture.surface != Surface::Complete || fixture.prefix.trim().is_empty() {
                Verdict::NotApplicable
            } else {
                no_prefix_repetition(fixture, output)
            }
        }
        Property::NoSuffixDuplication => {
            if fixture.surface != Surface::Complete || fixture.suffix.trim().is_empty() {
                Verdict::NotApplicable
            } else {
                no_suffix_duplication(fixture, output)
            }
        }
        Property::LanguageMatch => language_match(fixture.lang, output),
        Property::WhitespacePreserved => {
            if fixture.surface != Surface::Transform {
                Verdict::NotApplicable
            } else {
                whitespace_preserved(&fixture.selection, output)
            }
        }
        Property::NoAddedCodeFence => {
            if fixture.surface != Surface::Transform {
                Verdict::NotApplicable
            } else {
                no_added_code_fence(&fixture.selection, output)
            }
        }
        Property::LengthWithinBounds => length_within_bounds(fixture, output),
        Property::EndsAtSentenceBoundary => {
            if fixture.surface != Surface::Complete {
                Verdict::NotApplicable
            } else {
                ends_at_sentence_boundary(output)
            }
        }
    }
}

/// Openers the §5 post-filter is specified to strip.
///
/// Lower-cased ASCII prefixes plus their Japanese equivalents. Kept as data so
/// the list can grow from real failures rather than from imagination.
const PREAMBLES: &[&str] = &[
    "sure,",
    "sure!",
    "sure -",
    "certainly",
    "of course",
    "here's",
    "here is",
    "here are",
    "i'd be happy",
    "i would be happy",
    "absolutely",
    "no problem",
    "got it",
    "understood",
    "as an ai",
    "the revised",
    "the rewritten",
    "承知しました",
    "承知いたしました",
    "かしこまりました",
    "もちろん",
    "はい、",
    "以下が",
    "以下の",
    "こちらが",
    "了解しました",
];

fn no_preamble(output: &str) -> Verdict {
    let trimmed = output.trim_start();
    if trimmed.starts_with("```") {
        return Verdict::Fail("reply opens with a code fence".into());
    }
    let head: String = trimmed.chars().take(40).collect::<String>().to_lowercase();
    match PREAMBLES.iter().find(|p| head.starts_with(**p)) {
        Some(hit) => Verdict::Fail(format!("preamble {hit:?}")),
        None => Verdict::Pass,
    }
}

/// The longest tail of the prefix a reply is allowed to echo, in graphemes.
///
/// Short overlaps are legitimate — finishing the word the caret sits inside is
/// the *point* of Complete. The threshold is where an overlap stops being a
/// word and starts being a restatement.
const PREFIX_OVERLAP_LIMIT: usize = 12;

fn no_prefix_repetition(fixture: &Fixture, output: &str) -> Verdict {
    let tail = normalise(&fixture.prefix_tail(80));
    let reply = normalise(output);
    if tail.is_empty() || reply.is_empty() {
        return Verdict::Pass;
    }

    // Longest suffix of `tail` that is also a prefix of `reply`.
    let tail_graphemes: Vec<&str> = tail.graphemes(true).collect();
    let mut longest = 0usize;
    for start in 0..tail_graphemes.len() {
        let candidate = tail_graphemes[start..].concat();
        if reply.starts_with(&candidate) {
            longest = tail_graphemes.len() - start;
            break;
        }
    }
    if longest > PREFIX_OVERLAP_LIMIT {
        return Verdict::Fail(format!("repeats {longest} graphemes of the prefix"));
    }

    // A whole sentence from the prefix appearing anywhere in the reply is the
    // other common shape of this failure.
    for sentence in tail.split(['。', '.', '!', '?', '！', '？']) {
        let sentence = sentence.trim();
        if sentence.graphemes(true).count() >= 20 && reply.contains(sentence) {
            return Verdict::Fail("restates a full sentence from the prefix".into());
        }
    }
    Verdict::Pass
}

fn no_suffix_duplication(fixture: &Fixture, output: &str) -> Verdict {
    let head = normalise(&fixture.suffix_head(60));
    let reply = normalise(output);
    if head.is_empty() || reply.is_empty() {
        return Verdict::Pass;
    }
    let head_graphemes: Vec<&str> = head.graphemes(true).collect();
    for take in (PREFIX_OVERLAP_LIMIT + 1..=head_graphemes.len()).rev() {
        let candidate = head_graphemes[..take].concat();
        if reply.contains(&candidate) {
            return Verdict::Fail(format!(
                "duplicates {take} graphemes of the text after the caret"
            ));
        }
    }
    Verdict::Pass
}

/// Collapse whitespace and lower-case, so an overlap check is not defeated by a
/// reflowed newline.
fn normalise(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Script-based language check.
///
/// Deliberately crude and deliberately one-directional: it can tell Japanese
/// from English reliably (kana are unambiguous) and nothing else. It is not a
/// language detector and must not be reused as one. §5's rule is only "reply in
/// the same language as the input", and for a ja/en product that is exactly
/// what this measures.
pub fn looks_japanese(text: &str) -> bool {
    let mut kana = 0usize;
    let mut han = 0usize;
    let mut letters = 0usize;
    for ch in text.chars() {
        match ch {
            '\u{3040}'..='\u{309f}' | '\u{30a0}'..='\u{30ff}' | '\u{ff66}'..='\u{ff9d}' => {
                kana += 1;
                letters += 1;
            }
            '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}' => {
                han += 1;
                letters += 1;
            }
            c if c.is_alphabetic() => letters += 1,
            _ => {}
        }
    }
    if letters == 0 {
        return false;
    }
    // Any meaningful amount of kana settles it. Han without kana could be
    // Chinese, but within this product's ja/en scope it means Japanese.
    kana * 20 >= letters || (kana + han) * 4 >= letters
}

fn language_match(expected: Lang, output: &str) -> Verdict {
    if output.trim().is_empty() {
        return Verdict::NotApplicable;
    }
    let japanese = looks_japanese(output);
    match (expected, japanese) {
        (Lang::Ja, true) | (Lang::En, false) => Verdict::Pass,
        (Lang::Ja, false) => Verdict::Fail("expected Japanese, reply has no kana".into()),
        (Lang::En, true) => Verdict::Fail("expected English, reply is Japanese".into()),
    }
}

fn whitespace_preserved(selection: &str, output: &str) -> Verdict {
    if selection.trim().is_empty() {
        return Verdict::NotApplicable;
    }
    let leading = |s: &str| {
        s.chars()
            .take_while(|c| c.is_whitespace())
            .collect::<String>()
    };
    let trailing = |s: &str| {
        s.chars()
            .rev()
            .take_while(|c| c.is_whitespace())
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    };

    let (want_lead, got_lead) = (leading(selection), leading(output));
    if want_lead != got_lead {
        return Verdict::Fail(format!(
            "leading whitespace {want_lead:?} became {got_lead:?}"
        ));
    }
    let (want_trail, got_trail) = (trailing(selection), trailing(output));
    if want_trail != got_trail {
        return Verdict::Fail(format!(
            "trailing whitespace {want_trail:?} became {got_trail:?}"
        ));
    }
    Verdict::Pass
}

fn no_added_code_fence(selection: &str, output: &str) -> Verdict {
    if selection.contains("```") {
        return Verdict::NotApplicable;
    }
    if output.contains("```") {
        Verdict::Fail("added a code fence the input did not have".into())
    } else {
        Verdict::Pass
    }
}

/// Default cap when a fixture does not set one.
///
/// §5 binds Complete to `max_tokens: 64`; 400 graphemes is a generous ceiling
/// that still catches a model that ignored the instruction and wrote an essay.
const DEFAULT_COMPLETE_CAP: usize = 400;

fn length_within_bounds(fixture: &Fixture, output: &str) -> Verdict {
    let cap = fixture
        .max_output_graphemes
        .unwrap_or(match fixture.surface {
            Surface::Complete => DEFAULT_COMPLETE_CAP,
            // Transform replaces a selection; more than 4x its size means the model
            // expanded rather than transformed.
            Surface::Transform => (fixture.selection.graphemes(true).count() * 4).max(200),
            Surface::Ask => return Verdict::NotApplicable,
        });
    let length = output.graphemes(true).count();
    if length > cap {
        Verdict::Fail(format!("{length} graphemes > cap {cap}"))
    } else {
        Verdict::Pass
    }
}

/// Terminators that count as a sentence boundary in both scripts.
const TERMINATORS: &[char] = &[
    '.', '!', '?', '。', '！', '？', '…', '»', '"', '\'', '」', '』', ')', '）',
];

fn ends_at_sentence_boundary(output: &str) -> Verdict {
    let trimmed = output.trim_end();
    if trimmed.is_empty() {
        return Verdict::NotApplicable;
    }
    match trimmed.chars().last() {
        Some(last) if TERMINATORS.contains(&last) => Verdict::Pass,
        Some(last) => Verdict::Fail(format!("ends on {last:?}, not a sentence boundary")),
        None => Verdict::NotApplicable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete(prefix: &str, suffix: &str, lang: Lang) -> Fixture {
        Fixture {
            id: "t".into(),
            surface: Surface::Complete,
            lang,
            app: None,
            field_label: None,
            prefix: prefix.into(),
            suffix: suffix.into(),
            selection: String::new(),
            instruction: String::new(),
            max_output_graphemes: None,
            notes: None,
        }
    }

    fn transform(selection: &str, lang: Lang) -> Fixture {
        Fixture {
            id: "t".into(),
            surface: Surface::Transform,
            lang,
            app: None,
            field_label: None,
            prefix: String::new(),
            suffix: String::new(),
            selection: selection.into(),
            instruction: "make it polite".into(),
            max_output_graphemes: None,
            notes: None,
        }
    }

    #[test]
    fn preamble_is_caught_in_both_languages() {
        assert!(matches!(
            no_preamble("Sure, here you go."),
            Verdict::Fail(_)
        ));
        assert!(matches!(
            no_preamble("承知しました。以下です"),
            Verdict::Fail(_)
        ));
        assert!(no_preamble("the meeting is at three.").is_pass());
    }

    #[test]
    fn a_leading_fence_is_a_preamble() {
        assert!(matches!(
            no_preamble("```rust\nfn main() {}"),
            Verdict::Fail(_)
        ));
    }

    #[test]
    fn short_word_completion_is_not_prefix_repetition() {
        let f = complete("I am writing to confirm the meet", "", Lang::En);
        assert!(check(Property::NoPrefixRepetition, &f, "ing time.").is_pass());
    }

    #[test]
    fn restating_the_prefix_fails() {
        let f = complete("I am writing to confirm the meeting time", "", Lang::En);
        let verdict = check(
            Property::NoPrefixRepetition,
            &f,
            "I am writing to confirm the meeting time for tomorrow.",
        );
        assert!(matches!(verdict, Verdict::Fail(_)));
    }

    #[test]
    fn duplicating_the_text_after_the_caret_fails() {
        let f = complete("The release is ", " and the notes are attached.", Lang::En);
        let verdict = check(
            Property::NoSuffixDuplication,
            &f,
            "ready for Friday and the notes are attached.",
        );
        assert!(matches!(verdict, Verdict::Fail(_)));
    }

    #[test]
    fn suffix_property_is_not_applicable_without_a_suffix() {
        let f = complete("hello", "", Lang::En);
        assert_eq!(
            check(Property::NoSuffixDuplication, &f, "there"),
            Verdict::NotApplicable
        );
    }

    #[test]
    fn language_match_distinguishes_ja_and_en() {
        assert!(looks_japanese("明日の会議は三時です。"));
        assert!(looks_japanese("リリースは金曜です"));
        assert!(!looks_japanese("The release is on Friday."));
        assert!(matches!(
            language_match(Lang::Ja, "The release is on Friday."),
            Verdict::Fail(_)
        ));
        assert!(language_match(Lang::Ja, "金曜にリリースします。").is_pass());
    }

    #[test]
    fn a_stripped_leading_space_is_a_visible_bug() {
        let f = transform("  hello world  ", Lang::En);
        let verdict = check(Property::WhitespacePreserved, &f, "hello world");
        assert!(matches!(verdict, Verdict::Fail(_)));
        assert!(check(Property::WhitespacePreserved, &f, "  good day  ").is_pass());
    }

    #[test]
    fn a_fence_the_input_lacked_is_a_failure() {
        let f = transform("let x = 1", Lang::En);
        assert!(matches!(
            check(Property::NoAddedCodeFence, &f, "```\nlet x = 1;\n```"),
            Verdict::Fail(_)
        ));
        let fenced = transform("```\nlet x = 1\n```", Lang::En);
        assert_eq!(
            check(Property::NoAddedCodeFence, &fenced, "```\nlet x = 1;\n```"),
            Verdict::NotApplicable
        );
    }

    #[test]
    fn sentence_boundary_accepts_both_scripts() {
        assert!(ends_at_sentence_boundary("done.").is_pass());
        assert!(ends_at_sentence_boundary("できました。").is_pass());
        assert!(matches!(
            ends_at_sentence_boundary("this trails off and"),
            Verdict::Fail(_)
        ));
    }

    #[test]
    fn grapheme_counting_survives_emoji_and_combining_marks() {
        // §5 requires grapheme-cluster-safe handling everywhere; a family emoji
        // is one cluster and several chars.
        let f = complete("x", "", Lang::En);
        let mut f = f;
        f.max_output_graphemes = Some(2);
        assert!(check(Property::LengthWithinBounds, &f, "👨‍👩‍👧‍👦e").is_pass());
        assert!(matches!(
            check(Property::LengthWithinBounds, &f, "abc"),
            Verdict::Fail(_)
        ));
    }
}
