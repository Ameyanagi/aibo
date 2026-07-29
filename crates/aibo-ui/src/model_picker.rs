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
//!
//! **This module is an adapter.** All of the mechanics — lanes, fuzzy ranking,
//! the clamped highlight, pinned-before-recent — live in [`crate::palette`],
//! because none of it is about models. What is left here is the part that is:
//! how a model is identified (`provider/model`, the spelling `config.toml`
//! uses), what a query matches against, and what "the newest one per provider"
//! means for a starter set of pins.

use aibo_core::types::ModelBinding;

use crate::bridge::ModelOption;
use crate::palette::{self, Item, Palette};

pub use crate::palette::Lane;

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
        /// Whether the row states its own provider.
        ///
        /// False beneath a provider heading that already says it. Repeating
        /// `openai` on all eighty-eight rows of the openai group is noise
        /// competing with the only thing that distinguishes them.
        show_provider: bool,
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

/// A model, as something the palette can list.
impl Item for ModelOption {
    fn id(&self) -> String {
        binding_id(&self.binding)
    }

    /// Provider *and* name, so `openai 4o` and `4om` both find it.
    fn search_text(&self) -> String {
        format!("{} {}", self.binding.provider.as_str(), self.display_name)
    }

    fn lane(&self) -> Option<String> {
        Some(self.binding.provider.as_str().to_owned())
    }
}

/// The label shown in the lane column.
///
/// t3's equivalent is a rail of vendor icons. This is the same affordance in
/// words, for two reasons: `design.md` §9 cut icons on the grounds that "the
/// rail plus a key hint carries every meaning an icon would", and a column of
/// other people's logos is a trademark question aibo does not need to answer.
/// `⇥` cycles, which is what the search placeholder already promises.
pub fn lane_label(lane: &Lane) -> String {
    match lane {
        Lane::All => crate::i18n::t(crate::i18n::Key::LaneAll).to_owned(),
        Lane::Pinned => crate::i18n::t(crate::i18n::Key::PickerFavourites).to_owned(),
        Lane::Named(p) => p.clone(),
    }
}

/// The heading text for a group row, resolving the palette's reserved headings.
pub fn group_label(group: &str) -> String {
    match group {
        palette::PINNED_HEADING => crate::i18n::t(crate::i18n::Key::PickerFavourites).to_owned(),
        palette::RECENT_HEADING => crate::i18n::t(crate::i18n::Key::PickerRecent).to_owned(),
        other => other.to_owned(),
    }
}

/// The lanes available for a given catalogue, in cycle order.
///
/// Built from what is actually configured rather than from a fixed list: a lane
/// for a provider the user has not set up is a filter that can only ever return
/// nothing.
pub fn lanes(capable: &[ModelOption], favourites: &[ModelBinding]) -> Vec<Lane> {
    palette::lanes(capable, &ids(favourites))
}

/// The quick-pick's state.
///
/// Lives in [`crate::panel::PanelState`] rather than being rebuilt per frame:
/// the query and the highlight are user state, and losing them on a redraw
/// would make the widget unusable.
#[derive(Debug, Clone, Default)]
pub struct ModelPicker(pub Palette);

impl std::ops::Deref for ModelPicker {
    type Target = Palette;
    fn deref(&self) -> &Palette {
        &self.0
    }
}

impl std::ops::DerefMut for ModelPicker {
    fn deref_mut(&mut self) -> &mut Palette {
        &mut self.0
    }
}

impl ModelPicker {
    /// The rows to draw, in display order.
    pub fn rows(
        &self,
        capable: &[ModelOption],
        favourites: &[ModelBinding],
        recents: &[ModelBinding],
    ) -> Vec<Row> {
        self.0
            .rows(capable, &ids(favourites), &ids(recents))
            .into_iter()
            .map(|row| match row {
                palette::Row::Group(g) => Row::Group(group_label(&g)),
                palette::Row::Entry {
                    item,
                    pinned,
                    show_lane,
                } => Row::Model {
                    option: item.clone(),
                    favourite: pinned,
                    show_provider: show_lane,
                },
            })
            .collect()
    }
}

/// A binding as a palette id.
///
/// `provider/model` — the same spelling `config.toml` uses, so a pinned set can
/// be persisted without a second encoding.
fn binding_id(binding: &ModelBinding) -> String {
    format!("{}/{}", binding.provider.as_str(), binding.model)
}

/// Bindings as palette ids.
fn ids(bindings: &[ModelBinding]) -> Vec<String> {
    bindings.iter().map(binding_id).collect()
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
        let mut picker = ModelPicker(Palette {
            lane: Lane::Named("openai".to_owned()),
            ..Palette::default()
        });
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
        let picker = ModelPicker(Palette {
            lane: Lane::Named("openai".to_owned()),
            ..Palette::default()
        });
        let rows = picker.rows(&catalogue(), &[], &[]);
        assert!(!rows.iter().any(|r| matches!(r, Row::Group(_))));
    }

    #[test]
    fn cycling_lanes_wraps_and_resets_the_highlight() {
        let capable = catalogue();
        let pins = default_pins(&capable);
        let lanes = lanes(&capable, &pins);
        let mut picker = ModelPicker(Palette {
            highlight: 3,
            ..Palette::default()
        });

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
