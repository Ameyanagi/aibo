//! The model quick-pick (§4, §16).
//!
//! A flat `pick_list` was tolerable while Codex was the only provider and the
//! list was five entries. One OpenRouter key turns it into several hundred, and
//! a single OpenAI key already makes it eighty-eight — sorted by provider, so
//! reaching the second provider means scrolling past the whole of the first.
//! Length is not the only problem: a bare model name cannot say whether `gpt-5`
//! is OpenAI directly or OpenRouter fronting it, which are different prices,
//! context windows and trust boundaries (§14).
//!
//! So this is a quick-pick with two ways in, and they are the same widget:
//!
//! * **Browse.** With no query, entries are grouped under their provider, with
//!   pinned favourites first and then recently used — because in practice people
//!   cycle between two or three models, and those should be reachable without
//!   reading anything.
//! * **Type.** Any query fuzzy-matches across every provider at once, so
//!   `son` finds `claude-sonnet-4-5` and `4om` finds `gpt-4o-mini` without
//!   knowing which provider serves it.
//!
//! Matching uses `nucleo-matcher`, already in the workspace for §12's history
//! search. It is Unicode-aware and returns a score, which is what makes "type
//! three letters and press return" land on the right entry rather than the first
//! alphabetical one.

use aibo_core::types::ModelBinding;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

use crate::bridge::ModelOption;

/// One row the picker can show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A provider heading, shown only while browsing.
    Group(String),
    /// A selectable model.
    Model {
        /// The option itself.
        option: ModelOption,
        /// Pinned by the user.
        favourite: bool,
    },
}

impl Row {
    /// The binding this row selects, if it selects one.
    pub fn binding(&self) -> Option<&ModelBinding> {
        match self {
            Row::Group(_) => None,
            Row::Model { option, .. } => Some(&option.binding),
        }
    }
}

/// The quick-pick's state.
///
/// Lives in [`crate::panel::PanelState`] rather than being rebuilt per frame:
/// the query and the highlight are user state, and losing them on a redraw
/// would make the widget unusable.
#[derive(Debug, Clone, Default)]
pub struct ModelPicker {
    /// Whether the overlay is showing.
    pub open: bool,
    /// What the user has typed.
    pub query: String,
    /// Index into [`ModelPicker::rows`]'s selectable entries.
    pub highlight: usize,
}

impl ModelPicker {
    /// Open the picker, starting from a clean query.
    ///
    /// A retained query would mean reopening the picker shows the *last*
    /// search rather than where you are, which is the opposite of what a
    /// quick-pick is for.
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.highlight = 0;
    }

    /// Close it.
    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.highlight = 0;
    }

    /// Replace the query, resetting the highlight.
    ///
    /// The reset matters: leaving the highlight where it was means typing a
    /// character can move the selection onto an unrelated entry, and pressing
    /// return then picks something the user never looked at.
    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.highlight = 0;
    }

    /// Move the highlight, clamped rather than wrapping.
    ///
    /// Wrapping in a list of eighty-eight means holding the key past the end
    /// silently returns to the top, and the user loses their place.
    pub fn move_highlight(&mut self, delta: isize, selectable: usize) {
        if selectable == 0 {
            self.highlight = 0;
            return;
        }
        let last = selectable - 1;
        self.highlight = self.highlight.saturating_add_signed(delta).min(last);
    }

    /// The rows to render, in order.
    ///
    /// `capable` has already filtered out models that cannot serve the current
    /// request — a vision-incapable model with an image attached, for instance.
    /// Filtering *before* this point rather than dimming here is deliberate:
    /// §17 treats an offered action that cannot work as worse than an absent
    /// one, and the alternative was the confusion of a picker naming a Codex
    /// model while §4 routed the request to OpenAI because Codex cannot see.
    pub fn rows(
        &self,
        capable: &[ModelOption],
        favourites: &[ModelBinding],
        recent: &[ModelBinding],
    ) -> Vec<Row> {
        if !self.query.trim().is_empty() {
            return self.fuzzy_rows(capable, favourites);
        }
        self.browse_rows(capable, favourites, recent)
    }

    /// Fuzzy-matched, flat, best first.
    ///
    /// No group headings here, and that is the point: a query spans providers,
    /// so grouping would scatter the best matches down the list behind headings
    /// the user did not ask for.
    fn fuzzy_rows(&self, capable: &[ModelOption], favourites: &[ModelBinding]) -> Vec<Row> {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(
            self.query.trim(),
            CaseMatching::Ignore,
            Normalization::Smart,
        );

        // Matched against `provider model`, so "openai 4o" and "4o" both work
        // and a provider name alone lists that provider.
        let haystacks: Vec<String> = capable
            .iter()
            .map(|o| format!("{} {}", o.binding.provider.as_str(), o.display_name))
            .collect();

        let mut scored: Vec<(u32, &ModelOption)> = capable
            .iter()
            .zip(&haystacks)
            .filter_map(|(option, hay)| {
                let mut buf = Vec::new();
                let haystack = nucleo_matcher::Utf32Str::new(hay, &mut buf);
                pattern.score(haystack, &mut matcher).map(|s| (s, option))
            })
            .collect();

        // Score descending, then by name so equal scores are stable rather than
        // reordering between keystrokes.
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.display_name.cmp(&b.1.display_name))
        });

        scored
            .into_iter()
            .map(|(_, option)| Row::Model {
                favourite: favourites.contains(&option.binding),
                option: option.clone(),
            })
            .collect()
    }

    /// Browsing order: favourites, then recents, then grouped by provider.
    fn browse_rows(
        &self,
        capable: &[ModelOption],
        favourites: &[ModelBinding],
        recent: &[ModelBinding],
    ) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut shown: Vec<&ModelBinding> = Vec::new();

        // Favourites first: pinned deliberately, so they outrank recency.
        let favourite_rows: Vec<&ModelOption> = favourites
            .iter()
            .filter_map(|b| capable.iter().find(|o| &o.binding == b))
            .collect();
        if !favourite_rows.is_empty() {
            rows.push(Row::Group(
                crate::i18n::t(crate::i18n::Key::PickerFavourites).to_owned(),
            ));
            for option in favourite_rows {
                shown.push(&option.binding);
                rows.push(Row::Model {
                    option: option.clone(),
                    favourite: true,
                });
            }
        }

        // Then recents, skipping anything already pinned above — a model listed
        // twice makes the highlight ambiguous.
        let recent_rows: Vec<&ModelOption> = recent
            .iter()
            .filter(|b| !favourites.contains(b))
            .filter_map(|b| capable.iter().find(|o| &o.binding == b))
            .collect();
        if !recent_rows.is_empty() {
            rows.push(Row::Group(
                crate::i18n::t(crate::i18n::Key::PickerRecent).to_owned(),
            ));
            for option in recent_rows {
                shown.push(&option.binding);
                rows.push(Row::Model {
                    option: option.clone(),
                    favourite: false,
                });
            }
        }

        // Then everything else, under its provider.
        let mut providers: Vec<&str> = capable
            .iter()
            .map(|o| o.binding.provider.as_str())
            .collect();
        providers.sort_unstable();
        providers.dedup();

        for provider in providers {
            let rest: Vec<&ModelOption> = capable
                .iter()
                .filter(|o| o.binding.provider.as_str() == provider)
                .filter(|o| !shown.contains(&&o.binding))
                .collect();
            if rest.is_empty() {
                continue;
            }
            rows.push(Row::Group(provider.to_owned()));
            for option in rest {
                rows.push(Row::Model {
                    option: option.clone(),
                    favourite: favourites.contains(&option.binding),
                });
            }
        }
        rows
    }
}

/// The bindings in `rows` that can actually be chosen, in display order.
///
/// The highlight indexes this rather than `rows`, so arrowing past a group
/// heading is impossible — a highlight that can land on a heading produces a
/// return key that does nothing, which reads as the widget being broken.
pub fn selectable(rows: &[Row]) -> Vec<&ModelOption> {
    rows.iter()
        .filter_map(|r| match r {
            Row::Model { option, .. } => Some(option),
            Row::Group(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aibo_core::types::ProviderId;

    fn option(provider: &str, model: &str) -> ModelOption {
        ModelOption {
            binding: ModelBinding {
                provider: ProviderId::new(provider),
                model: model.to_owned(),
            },
            display_name: model.to_owned(),
            latency_ms: None,
        }
    }

    fn catalogue() -> Vec<ModelOption> {
        vec![
            option("codex", "gpt-5.6-sol"),
            option("openai", "gpt-5"),
            option("openai", "gpt-4o-mini"),
            option("openai", "o3"),
            option("anthropic", "claude-sonnet-4-5"),
        ]
    }

    /// The whole reason for the query: find a model without knowing, or
    /// scrolling to, its provider.
    #[test]
    fn a_query_matches_across_providers() {
        let mut picker = ModelPicker::default();
        picker.set_query("son".to_owned());
        let rows = picker.rows(&catalogue(), &[], &[]);
        let names: Vec<&str> = selectable(&rows)
            .iter()
            .map(|o| o.display_name.as_str())
            .collect();
        assert_eq!(names.first(), Some(&"claude-sonnet-4-5"));
    }

    #[test]
    fn a_query_can_be_an_abbreviation() {
        let mut picker = ModelPicker::default();
        picker.set_query("4om".to_owned());
        let rows = picker.rows(&catalogue(), &[], &[]);
        assert_eq!(
            selectable(&rows).first().map(|o| o.display_name.as_str()),
            Some("gpt-4o-mini")
        );
    }

    /// A provider name alone narrows to that provider, which is the "browse by
    /// typing" path.
    #[test]
    fn a_query_can_be_a_provider_name() {
        let mut picker = ModelPicker::default();
        picker.set_query("anthropic".to_owned());
        let rows = picker.rows(&catalogue(), &[], &[]);
        let providers: Vec<&str> = selectable(&rows)
            .iter()
            .map(|o| o.binding.provider.as_str())
            .collect();
        assert!(providers.iter().all(|p| *p == "anthropic"), "{providers:?}");
    }

    /// Searching is flat. Group headings would push the best match down behind
    /// a heading the user did not ask for.
    #[test]
    fn a_search_shows_no_group_headings() {
        let mut picker = ModelPicker::default();
        picker.set_query("gpt".to_owned());
        let rows = picker.rows(&catalogue(), &[], &[]);
        assert!(!rows.iter().any(|r| matches!(r, Row::Group(_))));
    }

    #[test]
    fn browsing_groups_by_provider() {
        let picker = ModelPicker::default();
        let rows = picker.rows(&catalogue(), &[], &[]);
        let groups: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Group(g) => Some(g.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(groups, vec!["anthropic", "codex", "openai"]);
    }

    /// Pinned outranks recent: a favourite is a deliberate choice, recency is
    /// an accident of what was asked last.
    #[test]
    fn favourites_come_before_recents() {
        let picker = ModelPicker::default();
        let favourite = option("openai", "o3").binding;
        let recent = option("openai", "gpt-5").binding;
        let rows = picker.rows(
            &catalogue(),
            std::slice::from_ref(&favourite),
            std::slice::from_ref(&recent),
        );

        let order: Vec<&ModelBinding> = selectable(&rows).iter().map(|o| &o.binding).collect();
        let fav_at = order.iter().position(|b| **b == favourite).expect("pinned");
        let rec_at = order.iter().position(|b| **b == recent).expect("recent");
        assert!(fav_at < rec_at);
    }

    /// A model listed twice makes the highlight ambiguous and the count wrong.
    #[test]
    fn no_model_appears_twice_while_browsing() {
        let picker = ModelPicker::default();
        let b = option("openai", "gpt-5").binding;
        let rows = picker.rows(
            &catalogue(),
            std::slice::from_ref(&b),
            std::slice::from_ref(&b),
        );
        let count = selectable(&rows).iter().filter(|o| o.binding == b).count();
        assert_eq!(count, 1);
    }

    /// The highlight indexes selectable rows, so it can never land on a heading
    /// and leave the return key doing nothing.
    #[test]
    fn the_highlight_clamps_and_never_lands_on_a_heading() {
        let picker = ModelPicker::default();
        let rows = picker.rows(&catalogue(), &[], &[]);
        let count = selectable(&rows).len();

        let mut p = ModelPicker::default();
        p.move_highlight(1_000, count);
        assert_eq!(p.highlight, count - 1, "clamped to the last entry");
        p.move_highlight(-1_000, count);
        assert_eq!(p.highlight, 0, "and to the first, never wrapping");
    }

    /// Typing must not leave the highlight pointing at an entry from the
    /// previous result set, or return picks something never looked at.
    #[test]
    fn typing_resets_the_highlight() {
        let mut picker = ModelPicker::default();
        picker.move_highlight(3, 5);
        assert_ne!(picker.highlight, 0);
        picker.set_query("gpt".to_owned());
        assert_eq!(picker.highlight, 0);
    }

    #[test]
    fn an_empty_catalogue_yields_no_rows_and_no_panic() {
        let picker = ModelPicker::default();
        assert!(picker.rows(&[], &[], &[]).is_empty());
        let mut p = ModelPicker::default();
        p.move_highlight(1, 0);
        assert_eq!(p.highlight, 0);
    }
}
