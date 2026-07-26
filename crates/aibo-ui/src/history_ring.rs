//! Input history, recalled with `↑` / `↓`.
//!
//! The convention every shell and every agent CLI shares: `↑` walks backwards
//! through what you submitted, `↓` walks forwards, and walking past the newest
//! entry returns the draft you were part-way through typing. Getting that last
//! part wrong is what makes an otherwise-correct implementation feel broken —
//! a user who presses `↑` to check something and `↓` to come back expects their
//! half-written sentence to still be there.
//!
//! Kept separate from `panel` so the cursor rules are testable without a UI:
//! this module has no iced dependency at all.
//!
//! Scope: session-local. §12's `messages` table is the eventual home, but the
//! database is not created until onboarding produces a key, and history that
//! silently does nothing until some unrelated step completes is worse than
//! history that is honestly per-session. Persisting is a later, additive change
//! — [`HistoryRing::seed`] exists for it.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};

/// How many submissions to keep. Beyond this the oldest are dropped.
///
/// A panel is not a shell: the useful recall window is the last few things you
/// asked, not a scrollback of everything.
pub const CAPACITY: usize = 100;

/// Submitted inputs, newest last, plus a cursor for `↑`/`↓` recall.
#[derive(Debug, Default, Clone)]
pub struct HistoryRing {
    /// Newest last. Never contains consecutive duplicates.
    entries: Vec<String>,
    /// `None` = not recalling; the input is the user's live draft.
    /// `Some(i)` = showing `entries[i]`.
    cursor: Option<usize>,
    /// The draft displaced by the first `↑`, restored by walking back down
    /// past the newest entry.
    draft: String,
}

impl HistoryRing {
    /// Empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-populate from storage, oldest first. For a future §12-backed ring.
    pub fn seed(entries: impl IntoIterator<Item = String>) -> Self {
        let mut ring = Self::new();
        for entry in entries {
            ring.record(&entry);
        }
        ring
    }

    /// Record a submission and reset the cursor.
    ///
    /// Resetting matters: after submitting, `↑` must offer the thing just sent,
    /// not resume from wherever the previous recall left off.
    pub fn record(&mut self, input: &str) {
        let input = input.trim();
        if input.is_empty() {
            return;
        }
        // Collapse consecutive duplicates — pressing ⏎ twice on the same
        // instruction should not cost two ↑ presses to walk back over.
        if self.entries.last().map(String::as_str) != Some(input) {
            self.entries.push(input.to_owned());
            if self.entries.len() > CAPACITY {
                self.entries.remove(0);
            }
        }
        self.cursor = None;
        self.draft.clear();
    }

    /// `↑` — older. Returns the text to show, or `None` at the oldest entry.
    ///
    /// `current` is what is in the input right now; on the first `↑` it is
    /// stashed as the draft.
    pub fn older(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let next = match self.cursor {
            None => {
                self.draft = current.to_owned();
                self.entries.len() - 1
            }
            Some(0) => return None, // already oldest: stay put, do not wrap
            Some(i) => i - 1,
        };
        self.cursor = Some(next);
        self.entries.get(next).cloned()
    }

    /// `↓` — newer. Returns the next entry, or the stashed draft once past the
    /// newest. `None` when not recalling.
    pub fn newer(&mut self) -> Option<String> {
        let i = self.cursor?;
        if i + 1 < self.entries.len() {
            self.cursor = Some(i + 1);
            return self.entries.get(i + 1).cloned();
        }
        // Past the newest: hand the draft back and stop recalling.
        self.cursor = None;
        Some(std::mem::take(&mut self.draft))
    }

    /// Whether `↑`/`↓` are currently walking history.
    pub fn is_recalling(&self) -> bool {
        self.cursor.is_some()
    }

    /// Abandon recall without altering the entries, e.g. when the panel closes.
    pub fn reset(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }

    /// Entries, oldest first.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// fzf-style fuzzy search over the entries, best match first.
    ///
    /// An empty query returns everything newest-first, which is what an
    /// just-opened search overlay should show — not an empty list.
    ///
    /// `nucleo-matcher` rather than a hand-rolled subsequence check because it
    /// scores the way fzf does (contiguity, word boundaries, camelCase) and
    /// because it normalises Unicode: a history containing 日本語 and ASCII has
    /// to match sensibly, and byte-wise matching does not.
    pub fn search(&self, query: &str) -> Vec<HistoryMatch> {
        if query.trim().is_empty() {
            return self
                .entries
                .iter()
                .enumerate()
                .rev()
                .map(|(index, text)| HistoryMatch {
                    index,
                    text: text.clone(),
                    score: 0,
                    matched: Vec::new(),
                })
                .collect();
        }

        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

        let mut hits: Vec<HistoryMatch> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, text)| {
                let mut buf = Vec::new();
                let haystack = Utf32Str::new(text, &mut buf);
                let mut matched = Vec::new();
                let score = pattern.indices(haystack, &mut matcher, &mut matched)?;
                matched.sort_unstable();
                matched.dedup();
                Some(HistoryMatch {
                    index,
                    text: text.clone(),
                    score,
                    matched,
                })
            })
            .collect();

        // Score descending, then newest first — with equal scores the thing you
        // asked most recently is nearly always the one you meant.
        hits.sort_by(|a, b| b.score.cmp(&a.score).then(b.index.cmp(&a.index)));
        hits
    }
}

/// One fuzzy-search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMatch {
    /// Position in [`HistoryRing::entries`], oldest = 0.
    pub index: usize,
    /// The entry itself.
    pub text: String,
    /// Higher is better. Zero for the unfiltered listing.
    pub score: u32,
    /// Indices of the matched characters, for highlighting. **Char offsets,
    /// not bytes** — slicing `text` with these directly will panic on any
    /// multi-byte character, which for this product means most Japanese input.
    pub matched: Vec<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> HistoryRing {
        HistoryRing::seed(["first".into(), "second".into(), "third".into()])
    }

    #[test]
    fn up_walks_backwards_newest_first() {
        let mut h = seeded();
        assert_eq!(h.older("").as_deref(), Some("third"));
        assert_eq!(h.older("").as_deref(), Some("second"));
        assert_eq!(h.older("").as_deref(), Some("first"));
    }

    #[test]
    fn oldest_entry_does_not_wrap() {
        let mut h = seeded();
        for _ in 0..3 {
            h.older("");
        }
        assert_eq!(h.older(""), None, "must not wrap to the newest");
        // The cursor stays put, so ↓ still walks forwards from the oldest.
        assert_eq!(h.newer().as_deref(), Some("second"));
    }

    #[test]
    fn down_past_newest_restores_the_draft() {
        let mut h = seeded();
        assert_eq!(h.older("half-written").as_deref(), Some("third"));
        assert_eq!(h.newer().as_deref(), Some("half-written"));
        assert!(!h.is_recalling());
    }

    #[test]
    fn draft_survives_a_deep_walk() {
        let mut h = seeded();
        h.older("draft");
        h.older("");
        h.older("");
        h.newer();
        h.newer();
        assert_eq!(h.newer().as_deref(), Some("draft"));
    }

    #[test]
    fn down_without_recalling_does_nothing() {
        let mut h = seeded();
        assert_eq!(h.newer(), None);
    }

    #[test]
    fn recording_resets_the_cursor() {
        let mut h = seeded();
        h.older("");
        h.older("");
        h.record("fourth");
        assert!(!h.is_recalling());
        assert_eq!(h.older("").as_deref(), Some("fourth"));
    }

    #[test]
    fn consecutive_duplicates_collapse() {
        let mut h = HistoryRing::new();
        h.record("same");
        h.record("same");
        assert_eq!(h.entries().len(), 1);
        // Non-consecutive repeats are kept: they are real history.
        h.record("other");
        h.record("same");
        assert_eq!(h.entries().len(), 3);
    }

    #[test]
    fn blank_input_is_not_recorded() {
        let mut h = HistoryRing::new();
        h.record("   ");
        h.record("");
        assert!(h.entries().is_empty());
    }

    #[test]
    fn entries_are_trimmed() {
        let mut h = HistoryRing::new();
        h.record("  padded  ");
        assert_eq!(h.entries(), ["padded"]);
    }

    #[test]
    fn capacity_drops_the_oldest() {
        let mut h = HistoryRing::new();
        for i in 0..CAPACITY + 10 {
            h.record(&format!("entry {i}"));
        }
        assert_eq!(h.entries().len(), CAPACITY);
        assert_eq!(h.entries()[0], format!("entry {}", 10));
    }

    #[test]
    fn empty_history_ignores_both_keys() {
        let mut h = HistoryRing::new();
        assert_eq!(h.older("draft"), None);
        assert_eq!(h.newer(), None);
    }

    #[test]
    fn empty_query_lists_everything_newest_first() {
        let h = seeded();
        let hits: Vec<_> = h.search("  ").into_iter().map(|m| m.text).collect();
        assert_eq!(hits, ["third", "second", "first"]);
    }

    #[test]
    fn fuzzy_matches_non_contiguous_characters() {
        let h = HistoryRing::seed([
            "rewrite this as a changelog entry".into(),
            "summarise the release notes".into(),
        ]);
        let hits = h.search("rwchg");
        assert_eq!(hits.len(), 1, "only the changelog entry is a subsequence");
        assert!(hits[0].text.starts_with("rewrite"));
        assert!(!hits[0].matched.is_empty(), "indices drive highlighting");
    }

    #[test]
    fn ranks_the_better_match_first() {
        let h = HistoryRing::seed([
            "deploy the release gate".into(),
            "d-e-p tangential nonsense".into(),
        ]);
        let hits = h.search("deploy");
        assert!(hits[0].text.starts_with("deploy"), "got {:?}", hits[0].text);
    }

    #[test]
    fn ties_prefer_the_more_recent_entry() {
        let h = HistoryRing::seed(["same text".into(), "other".into(), "same text".into()]);
        let hits = h.search("same text");
        // Consecutive dupes collapse, so both remaining "same text" entries are
        // distinct positions; the newer index must win.
        assert_eq!(hits[0].index, 2);
    }

    #[test]
    fn matches_japanese() {
        let h = HistoryRing::seed([
            "変更履歴のエントリとして書き直して".into(),
            "release notes".into(),
        ]);
        let hits = h.search("変更");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.starts_with("変更履歴"));
    }

    #[test]
    fn match_indices_are_char_offsets_not_bytes() {
        // Every index must be a valid char position; a byte-offset bug shows up
        // here as an index past the char count.
        let h = HistoryRing::seed(["日本語のテキスト".into()]);
        let hits = h.search("テキスト");
        assert_eq!(hits.len(), 1);
        let chars = hits[0].text.chars().count() as u32;
        assert!(
            hits[0].matched.iter().all(|&i| i < chars),
            "indices {:?} exceed char count {chars}",
            hits[0].matched
        );
    }

    #[test]
    fn no_match_returns_nothing() {
        let h = seeded();
        assert!(h.search("zzzzz").is_empty());
    }

    #[test]
    fn reset_keeps_entries_but_stops_recall() {
        let mut h = seeded();
        h.older("draft");
        h.reset();
        assert!(!h.is_recalling());
        assert_eq!(h.entries().len(), 3);
    }
}
