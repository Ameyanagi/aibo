//! §4 rule 7's *"parsed leading verb"*.
//!
//! Rule 7 sends a short `Ask` to `Fast` when it opens with `Define`,
//! `Translate`, `Spell` or `Convert` — cheap, closed-form questions that do not
//! need a reasoning model. That rule cannot fire unless something turns the
//! user's first word into a [`Verb`], and nothing did.
//!
//! Deliberately a table of literals and not a classifier: §4's whole argument
//! is that *"an LLM classifier in the hot path would add a round trip to a
//! 250 ms budget"*, and the same logic applies to anything cleverer than a
//! lowercase prefix match. Getting it wrong costs one role step, not a wrong
//! answer.
//!
//! Japanese is included because §4 already treats CJK as a first-class case in
//! the token estimate, and because the surrounding language handling in §5
//! assumes Japanese input is ordinary.

use aibo_core::types::Verb;

/// The trigger words for each verb, lowercase.
///
/// Order matters only for the longest-match rule below: entries are compared
/// longest-first so `"summarise"` is not shadowed by a hypothetical `"sum"`.
const TRIGGERS: &[(&str, Verb)] = &[
    ("translate", Verb::Translate),
    ("翻訳", Verb::Translate),
    ("訳して", Verb::Translate),
    ("define", Verb::Define),
    ("definition", Verb::Define),
    ("意味", Verb::Define),
    ("fix", Verb::Fix),
    ("correct", Verb::Fix),
    ("修正", Verb::Fix),
    ("explain", Verb::Explain),
    ("説明", Verb::Explain),
    ("summarise", Verb::Summarise),
    ("summarize", Verb::Summarise),
    ("summary", Verb::Summarise),
    ("要約", Verb::Summarise),
    ("spell", Verb::Spell),
    ("spellcheck", Verb::Spell),
    ("convert", Verb::Convert),
    ("変換", Verb::Convert),
    ("rewrite", Verb::Rewrite),
    ("reword", Verb::Rewrite),
    ("書き直", Verb::Rewrite),
    ("shorten", Verb::Shorten),
    ("短く", Verb::Shorten),
    ("expand", Verb::Expand),
    ("elaborate", Verb::Expand),
];

/// Parse the leading verb of a panel input, if there is one.
///
/// English matches the first whitespace-delimited word; Japanese has no such
/// delimiter, so the CJK triggers match as a prefix instead.
///
/// ```
/// use aibo_core::types::Verb;
/// use aibo_session::verb::parse_leading_verb;
///
/// assert_eq!(parse_leading_verb("translate this"), Some(Verb::Translate));
/// assert_eq!(parse_leading_verb("Define: ontology"), Some(Verb::Define));
/// assert_eq!(parse_leading_verb("翻訳して"), Some(Verb::Translate));
/// assert_eq!(parse_leading_verb("what is an ontology"), None);
/// ```
pub fn parse_leading_verb(input: &str) -> Option<Verb> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    // The first word, with trailing punctuation removed: "Define:" and
    // "translate," are the same intent as the bare word.
    let word: String = trimmed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();

    let mut best: Option<(usize, Verb)> = None;
    for (trigger, verb) in TRIGGERS {
        let matched = if trigger.is_ascii() {
            word == *trigger
        } else {
            // CJK: no word boundary to split on, so match the head of the input.
            trimmed.starts_with(trigger)
        };
        if matched && best.is_none_or(|(len, _)| trigger.len() > len) {
            best = Some((trigger.len(), *verb));
        }
    }
    best.map(|(_, verb)| verb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rule_seven_verbs_all_parse() {
        for (input, expected) in [
            ("define recursion", Verb::Define),
            ("Translate to Japanese", Verb::Translate),
            ("spell   accommodate", Verb::Spell),
            ("convert 3kg to lb", Verb::Convert),
        ] {
            assert_eq!(parse_leading_verb(input), Some(expected), "{input}");
        }
    }

    #[test]
    fn punctuation_and_case_do_not_matter() {
        assert_eq!(parse_leading_verb("  DEFINE: ontology"), Some(Verb::Define));
        assert_eq!(parse_leading_verb("Fix, please"), Some(Verb::Fix));
    }

    #[test]
    fn a_verb_that_is_not_leading_does_not_match() {
        assert_eq!(parse_leading_verb("please translate this"), None);
        assert_eq!(parse_leading_verb(""), None);
        assert_eq!(parse_leading_verb("   "), None);
    }

    #[test]
    fn japanese_matches_as_a_prefix() {
        assert_eq!(parse_leading_verb("要約して"), Some(Verb::Summarise));
        assert_eq!(parse_leading_verb("説明してください"), Some(Verb::Explain));
        assert_eq!(parse_leading_verb("これは何ですか"), None);
    }
}
