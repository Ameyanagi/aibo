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
    /// Which lane the list is narrowed to.
    pub lane: Lane,
}

/// What the picker is currently narrowed to.
///
/// The equivalent of t3's left icon rail, as words rather than brand marks:
/// `design.md` §9 cut icons on the grounds that "the rail plus a key hint
/// carries every meaning an icon would", and a column of unlabelled vendor
/// logos is the case it was arguing against. It also avoids shipping other
/// people's trademarks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Lane {
    /// Everything, grouped by provider.
    #[default]
    All,
    /// Pinned models only.
    Pinned,
    /// One provider.
    Provider(String),
}

impl Lane {
    /// The label shown in the lane column.
    pub fn label(&self) -> String {
        match self {
            Lane::All => crate::i18n::t(crate::i18n::Key::LaneAll).to_owned(),
            Lane::Pinned => crate::i18n::t(crate::i18n::Key::PickerFavourites).to_owned(),
            Lane::Provider(p) => p.clone(),
        }
    }
}

/// The lanes available for a given catalogue, in cycle order.
///
/// Built from what is actually configured rather than from a fixed list: a lane
/// for a provider the user has not set up is a filter that can only ever return
/// nothing.
pub fn lanes(capable: &[ModelOption], favourites: &[ModelBinding]) -> Vec<Lane> {
    let mut lanes = vec![Lane::All];
    if !favourites.is_empty() {
        lanes.push(Lane::Pinned);
    }
    let mut providers: Vec<&str> = capable
        .iter()
        .map(|o| o.binding.provider.as_str())
        .collect();
    providers.sort_unstable();
    providers.dedup();
    lanes.extend(providers.into_iter().map(|p| Lane::Provider(p.to_owned())));
    lanes
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
        self.lane = Lane::All;
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
        // The lane narrows the candidate set *before* the query, so a search
        // inside a lane stays inside it. The other order would make typing
        // silently escape the filter the user just chose.
        let in_lane: Vec<ModelOption> = match &self.lane {
            Lane::All => capable.to_vec(),
            Lane::Pinned => capable
                .iter()
                .filter(|o| favourites.contains(&o.binding))
                .cloned()
                .collect(),
            Lane::Provider(provider) => capable
                .iter()
                .filter(|o| o.binding.provider.as_str() == provider)
                .cloned()
                .collect(),
        };

        if !self.query.trim().is_empty() {
            return self.fuzzy_rows(&in_lane, favourites);
        }
        // Inside a single lane the provider heading is noise — every row shares
        // it — so only the "everything" lane groups.
        if matches!(self.lane, Lane::All) {
            self.browse_rows(&in_lane, favourites, recent)
        } else {
            in_lane
                .into_iter()
                .map(|option| Row::Model {
                    favourite: favourites.contains(&option.binding),
                    option,
                })
                .collect()
        }
    }

    /// Move to the next lane, wrapping.
    ///
    /// Wrapping is right here and wrong for the highlight: there are a handful
    /// of lanes and cycling them is the gesture, whereas a wrapping list of
    /// eighty-nine loses the user's place.
    pub fn cycle_lane(&mut self, lanes: &[Lane]) {
        if lanes.is_empty() {
            return;
        }
        let at = lanes.iter().position(|l| l == &self.lane).unwrap_or(0);
        self.lane = lanes[(at + 1) % lanes.len()].clone();
        // A new lane is a new result set, so the old index means nothing.
        self.highlight = 0;
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

/// A starter set of pins, for a user who has pinned nothing.
///
/// **Derived, not hardcoded.** §10's whole warning is that a baked-in model list
/// starts failing months later, and a shipped list of "good models" is exactly
/// that with a shorter half-life. The newest model from each configured provider
/// is a rule that stays right as models ship: it is what the user would pick
/// anyway, and it needs no maintenance.
///
/// Returned rather than written into state, so an explicit unpin is never undone
/// by a restart — the defaults apply only while the set is genuinely empty.
pub fn default_pins(capable: &[ModelOption]) -> Vec<ModelBinding> {
    let mut providers: Vec<&str> = capable
        .iter()
        .map(|o| o.binding.provider.as_str())
        .collect();
    providers.sort_unstable();
    providers.dedup();

    providers
        .into_iter()
        .filter_map(|provider| {
            capable
                .iter()
                .filter(|o| o.binding.provider.as_str() == provider)
                // `max_by_key` on the date, falling back to the order the
                // runtime already sorted into when no provider reports one.
                .max_by_key(|o| o.released_at.unwrap_or(0))
                .map(|o| o.binding.clone())
        })
        .collect()
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
            released_at: None,
            abilities: Default::default(),
            cost: None,
        }
    }

    fn dated(provider: &str, model: &str, released: u64) -> ModelOption {
        ModelOption {
            released_at: Some(released),
            ..option(provider, model)
        }
    }

    /// Newest first is the whole point: alphabetical ordering put
    /// `gpt-3.5-turbo` above `gpt-5`, so the first thing offered was the oldest
    /// thing available.
    #[test]
    fn default_pins_are_the_newest_model_per_provider() {
        let capable = vec![
            dated("openai", "gpt-3.5-turbo", 1_600_000_000),
            dated("openai", "gpt-5", 1_760_000_000),
            dated("anthropic", "claude-3", 1_700_000_000),
            dated("anthropic", "claude-sonnet-4-5", 1_780_000_000),
        ];
        let pins = default_pins(&capable);
        let models: Vec<&str> = pins.iter().map(|b| b.model.as_str()).collect();
        assert_eq!(models, vec!["claude-sonnet-4-5", "gpt-5"]);
    }

    /// One pin per provider, so a user with five providers gets five, not fifty.
    #[test]
    fn default_pins_are_one_per_provider() {
        let pins = default_pins(&catalogue());
        let mut providers: Vec<&str> = pins.iter().map(|b| b.provider.as_str()).collect();
        let before = providers.len();
        providers.sort_unstable();
        providers.dedup();
        assert_eq!(providers.len(), before, "no provider appears twice");
    }

    /// A lane narrows before the query, so searching inside a lane cannot escape
    /// it — the other order would silently discard the filter just chosen.
    #[test]
    fn a_lane_narrows_before_the_query() {
        let mut picker = ModelPicker {
            lane: Lane::Provider("openai".to_owned()),
            ..ModelPicker::default()
        };
        picker.set_query("gpt".to_owned());
        let rows = picker.rows(&catalogue(), &[], &[]);
        assert!(
            selectable(&rows)
                .iter()
                .all(|o| o.binding.provider.as_str() == "openai"),
            "the lane must survive a query that matches other providers"
        );
    }

    /// Inside one provider's lane the heading is noise: every row shares it.
    #[test]
    fn a_provider_lane_drops_the_group_heading() {
        let picker = ModelPicker {
            lane: Lane::Provider("openai".to_owned()),
            ..ModelPicker::default()
        };
        let rows = picker.rows(&catalogue(), &[], &[]);
        assert!(!rows.iter().any(|r| matches!(r, Row::Group(_))));
    }

    #[test]
    fn cycling_lanes_wraps_and_resets_the_highlight() {
        let capable = catalogue();
        let pins = default_pins(&capable);
        let lanes = lanes(&capable, &pins);
        let mut picker = ModelPicker {
            highlight: 3,
            ..ModelPicker::default()
        };

        picker.cycle_lane(&lanes);
        assert_ne!(picker.lane, Lane::All, "cycling moves off the first lane");
        assert_eq!(picker.highlight, 0, "a new lane is a new result set");

        for _ in 0..lanes.len() {
            picker.cycle_lane(&lanes);
        }
        assert_eq!(picker.lane, lanes[1], "a full cycle returns");
    }

    /// The pinned lane only exists once something is pinned; a lane that can
    /// only ever be empty is a filter with no purpose.
    #[test]
    fn the_pinned_lane_appears_only_when_something_is_pinned() {
        let capable = catalogue();
        assert!(!lanes(&capable, &[]).contains(&Lane::Pinned));
        let pins = default_pins(&capable);
        assert!(lanes(&capable, &pins).contains(&Lane::Pinned));
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
