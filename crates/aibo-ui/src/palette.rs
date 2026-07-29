//! A reusable search-and-pick overlay (§16).
//!
//! The model quick-pick is the first user, not the only one. §12's history
//! search is the reason `nucleo-matcher` is in the workspace at all;
//! `Section::Actions` is a stub waiting for a saved-actions browser; §9's hotkey
//! rebind needs to offer candidate combos. All four are the same interaction —
//! *narrow a long list, then choose one* — so the machinery lives here and each
//! caller supplies only what is specific to it: how its items match, how they
//! group, and how one row draws.
//!
//! What the widget owns, because getting any of it wrong is the difference
//! between a palette and a list:
//!
//! * **Lanes narrow before the query.** Searching inside a lane must stay inside
//!   it; the other order silently discards the filter the user just chose.
//! * **The highlight indexes selectable rows only**, so it can never land on a
//!   group heading and leave the return key doing nothing.
//! * **Pinned outranks recent**, because a pin is a decision and recency is an
//!   accident of what was asked last.
//! * **The highlight clamps, lanes wrap.** There are a handful of lanes and
//!   cycling them is the gesture; a wrapping list of ninety loses your place.
//!
//! Identity is a `String` throughout — `provider/model`, a history row id, an
//! action name. That is deliberately what persistence wants, so a pinned set can
//! be written to `config.toml` without a bespoke encoding per caller.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

/// Something the palette can list.
pub trait Item {
    /// Stable identity, used for pinning, recency and equality.
    ///
    /// Must be stable across a refresh: a pin keyed on something that changes
    /// when the list reloads is a pin that quietly detaches.
    fn id(&self) -> String;

    /// The text a query matches against.
    ///
    /// Include everything the user might reasonably type — for a model that is
    /// the provider *and* the name, so `openai 4o` and `4o` both find it.
    fn search_text(&self) -> String;

    /// Which lane this item belongs to, if the caller groups.
    fn lane(&self) -> Option<String> {
        None
    }
}

/// What the list is currently narrowed to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Lane {
    /// Everything, grouped by lane.
    #[default]
    All,
    /// Pinned entries only.
    Pinned,
    /// One named lane.
    Named(String),
}

/// A row to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row<'a, I> {
    /// A heading. Present only while browsing the `All` lane.
    Group(String),
    /// A selectable entry.
    Entry {
        /// The item.
        item: &'a I,
        /// Whether it is pinned.
        pinned: bool,
        /// Whether the row must name its own lane.
        ///
        /// False under a group heading that already says it — repeating the
        /// provider on every row of a group headed by that provider is noise
        /// that makes the name itself harder to find. True when the row stands
        /// alone: in search results, and under `pinned` or `recent`.
        show_lane: bool,
    },
}

/// The palette's state. Owned by the caller so a redraw cannot lose it.
#[derive(Debug, Clone, Default)]
pub struct Palette {
    /// Whether the overlay is showing.
    pub open: bool,
    /// What the user has typed.
    pub query: String,
    /// Index into the selectable rows.
    pub highlight: usize,
    /// The active lane.
    pub lane: Lane,
}

impl Palette {
    /// Open it, from a clean query and the `All` lane.
    ///
    /// A retained query would show the *last* search rather than where you are,
    /// which is the opposite of what a palette is for.
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
        self.lane = Lane::All;
    }

    /// Replace the query, resetting the highlight.
    ///
    /// The reset matters: leaving it put means a keystroke can slide the
    /// selection onto an unrelated entry, and return then picks something the
    /// user never looked at.
    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.highlight = 0;
    }

    /// Move the highlight, clamped rather than wrapping.
    pub fn move_highlight(&mut self, delta: isize, selectable: usize) {
        if selectable == 0 {
            self.highlight = 0;
            return;
        }
        self.highlight = self
            .highlight
            .saturating_add_signed(delta)
            .min(selectable - 1);
    }

    /// Advance to the next lane, wrapping.
    pub fn cycle_lane(&mut self, lanes: &[Lane]) {
        if lanes.is_empty() {
            return;
        }
        let at = lanes.iter().position(|l| l == &self.lane).unwrap_or(0);
        self.lane = lanes[(at + 1) % lanes.len()].clone();
        // A new lane is a new result set, so the old index means nothing.
        self.highlight = 0;
    }

    /// The rows to draw, in order.
    pub fn rows<'a, I: Item>(
        &self,
        items: &'a [I],
        pinned: &[String],
        recent: &[String],
    ) -> Vec<Row<'a, I>> {
        let in_lane: Vec<&'a I> = items
            .iter()
            .filter(|item| match &self.lane {
                Lane::All => true,
                Lane::Pinned => pinned.contains(&item.id()),
                Lane::Named(name) => item.lane().as_deref() == Some(name.as_str()),
            })
            .collect();

        if !self.query.trim().is_empty() {
            return Self::fuzzy(&self.query, &in_lane, pinned);
        }
        if matches!(self.lane, Lane::All) {
            Self::browse(&in_lane, pinned, recent)
        } else {
            // Inside one lane the heading is noise: every row shares it.
            in_lane
                .into_iter()
                .map(|item| Row::Entry {
                    pinned: pinned.contains(&item.id()),
                    item,
                    show_lane: false,
                })
                .collect()
        }
    }

    /// Fuzzy-matched, flat, best first.
    ///
    /// No headings: a query spans lanes, so grouping would scatter the best
    /// matches behind headings the user did not ask for.
    fn fuzzy<'a, I: Item>(query: &str, items: &[&'a I], pinned: &[String]) -> Vec<Row<'a, I>> {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(query.trim(), CaseMatching::Ignore, Normalization::Smart);

        let mut scored: Vec<(u32, &'a I)> = items
            .iter()
            .filter_map(|item| {
                let hay = item.search_text();
                let mut buf = Vec::new();
                pattern
                    .score(nucleo_matcher::Utf32Str::new(&hay, &mut buf), &mut matcher)
                    .map(|score| (score, *item))
            })
            .collect();

        // Score descending, then by id so equal scores are stable between
        // keystrokes rather than reshuffling under the cursor.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id().cmp(&b.1.id())));

        scored
            .into_iter()
            .map(|(_, item)| Row::Entry {
                pinned: pinned.contains(&item.id()),
                item,
                // Standing alone, so the row must say where it came from.
                show_lane: true,
            })
            .collect()
    }

    /// Pinned, then recent, then everything else under its lane.
    fn browse<'a, I: Item>(
        items: &[&'a I],
        pinned: &[String],
        recent: &[String],
    ) -> Vec<Row<'a, I>> {
        let mut rows = Vec::new();
        let mut shown: Vec<String> = Vec::new();

        let section =
            |rows: &mut Vec<Row<'a, I>>, shown: &mut Vec<String>, heading: &str, ids: &[String]| {
                let found: Vec<&'a I> = ids
                    .iter()
                    .filter(|id| !shown.contains(id))
                    .filter_map(|id| items.iter().find(|item| &item.id() == id).copied())
                    .collect();
                if found.is_empty() {
                    return;
                }
                rows.push(Row::Group(heading.to_owned()));
                for item in found {
                    shown.push(item.id());
                    rows.push(Row::Entry {
                        pinned: pinned.contains(&item.id()),
                        item,
                        show_lane: true,
                    });
                }
            };

        section(&mut rows, &mut shown, PINNED_HEADING, pinned);
        section(&mut rows, &mut shown, RECENT_HEADING, recent);

        let mut lanes: Vec<String> = items.iter().filter_map(|i| i.lane()).collect();
        lanes.sort_unstable();
        lanes.dedup();

        for lane in lanes {
            let rest: Vec<&'a I> = items
                .iter()
                .filter(|i| i.lane().as_deref() == Some(lane.as_str()))
                .filter(|i| !shown.contains(&i.id()))
                .copied()
                .collect();
            if rest.is_empty() {
                continue;
            }
            rows.push(Row::Group(lane));
            for item in rest {
                rows.push(Row::Entry {
                    pinned: pinned.contains(&item.id()),
                    item,
                    // The heading already says it.
                    show_lane: false,
                });
            }
        }
        rows
    }
}

/// Heading above pinned entries. Resolved by the caller for display.
pub const PINNED_HEADING: &str = "\u{1}pinned";
/// Heading above recently used entries.
pub const RECENT_HEADING: &str = "\u{1}recent";

/// The selectable entries in `rows`, in display order.
///
/// The highlight indexes this rather than `rows`, which is what makes it
/// impossible to highlight a heading.
pub fn selectable<'a, I>(rows: &'a [Row<'a, I>]) -> Vec<&'a I> {
    rows.iter()
        .filter_map(|row| match row {
            Row::Entry { item, .. } => Some(*item),
            Row::Group(_) => None,
        })
        .collect()
}

/// The lanes available for `items`, in cycle order.
///
/// Built from what is present rather than a fixed list, so there is never a lane
/// that can only ever return nothing.
pub fn lanes<I: Item>(items: &[I], pinned: &[String]) -> Vec<Lane> {
    let mut lanes = vec![Lane::All];
    if !pinned.is_empty() {
        lanes.push(Lane::Pinned);
    }
    let mut named: Vec<String> = items.iter().filter_map(|i| i.lane()).collect();
    named.sort_unstable();
    named.dedup();
    lanes.extend(named.into_iter().map(Lane::Named));
    lanes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct Fake {
        lane: &'static str,
        name: &'static str,
    }

    impl Item for Fake {
        fn id(&self) -> String {
            format!("{}/{}", self.lane, self.name)
        }
        fn search_text(&self) -> String {
            format!("{} {}", self.lane, self.name)
        }
        fn lane(&self) -> Option<String> {
            Some(self.lane.to_owned())
        }
    }

    fn items() -> Vec<Fake> {
        vec![
            Fake {
                lane: "openai",
                name: "gpt-5",
            },
            Fake {
                lane: "openai",
                name: "gpt-4o-mini",
            },
            Fake {
                lane: "anthropic",
                name: "claude-sonnet-4-5",
            },
            Fake {
                lane: "codex",
                name: "gpt-5.6-sol",
            },
        ]
    }

    #[test]
    fn a_query_matches_across_lanes() {
        let mut p = Palette::default();
        p.set_query("son".to_owned());
        let all = items();
        let rows = p.rows(&all, &[], &[]);
        assert_eq!(
            selectable(&rows).first().map(|i| i.name),
            Some("claude-sonnet-4-5")
        );
    }

    #[test]
    fn a_query_can_be_an_abbreviation() {
        let mut p = Palette::default();
        p.set_query("4om".to_owned());
        let all = items();
        let rows = p.rows(&all, &[], &[]);
        assert_eq!(
            selectable(&rows).first().map(|i| i.name),
            Some("gpt-4o-mini")
        );
    }

    /// The filter must survive a query that matches other lanes, or typing
    /// silently escapes the narrowing the user just chose.
    #[test]
    fn a_lane_narrows_before_the_query() {
        let mut p = Palette {
            lane: Lane::Named("openai".to_owned()),
            ..Palette::default()
        };
        p.set_query("gpt".to_owned());
        let all = items();
        let rows = p.rows(&all, &[], &[]);
        assert!(selectable(&rows).iter().all(|i| i.lane == "openai"));
    }

    #[test]
    fn searching_is_flat_and_browsing_is_grouped() {
        let p = Palette::default();
        let all = items();
        assert!(
            p.rows(&all, &[], &[])
                .iter()
                .any(|r| matches!(r, Row::Group(_))),
            "browsing groups"
        );

        let mut q = Palette::default();
        q.set_query("gpt".to_owned());
        assert!(
            !q.rows(&all, &[], &[])
                .iter()
                .any(|r| matches!(r, Row::Group(_))),
            "searching does not"
        );
    }

    /// A row under a heading that already names its lane must not repeat it;
    /// a row standing alone must state it.
    #[test]
    fn a_row_names_its_lane_only_when_no_heading_does() {
        let p = Palette::default();
        let all = items();
        let pinned = vec!["openai/gpt-5".to_owned()];
        let rows = p.rows(&all, &pinned, &[]);

        let mut under_pinned = None;
        let mut under_lane = None;
        let mut heading = String::new();
        for row in &rows {
            match row {
                Row::Group(g) => heading = g.clone(),
                Row::Entry {
                    item, show_lane, ..
                } => {
                    if heading == PINNED_HEADING && item.name == "gpt-5" {
                        under_pinned = Some(*show_lane);
                    }
                    if heading == "openai" && item.name == "gpt-4o-mini" {
                        under_lane = Some(*show_lane);
                    }
                }
            }
        }
        assert_eq!(under_pinned, Some(true), "pinned rows stand alone");
        assert_eq!(under_lane, Some(false), "the heading already says openai");
    }

    #[test]
    fn pinned_outranks_recent_and_nothing_appears_twice() {
        let p = Palette::default();
        let all = items();
        let id = "openai/gpt-5".to_owned();
        let rows = p.rows(&all, std::slice::from_ref(&id), std::slice::from_ref(&id));
        assert_eq!(
            selectable(&rows).iter().filter(|i| i.id() == id).count(),
            1,
            "a duplicate makes the highlight ambiguous"
        );
    }

    #[test]
    fn the_highlight_clamps_and_lanes_wrap() {
        let all = items();
        let count = selectable(&Palette::default().rows(&all, &[], &[])).len();
        let mut p = Palette::default();
        p.move_highlight(1_000, count);
        assert_eq!(p.highlight, count - 1);
        p.move_highlight(-1_000, count);
        assert_eq!(p.highlight, 0);

        let ls = lanes(&all, &[]);
        let mut q = Palette::default();
        for _ in 0..ls.len() {
            q.cycle_lane(&ls);
        }
        assert_eq!(q.lane, Lane::All, "a full cycle returns to the start");
    }

    #[test]
    fn the_pinned_lane_exists_only_when_something_is_pinned() {
        let all = items();
        assert!(!lanes(&all, &[]).contains(&Lane::Pinned));
        assert!(lanes(&all, &["openai/gpt-5".to_owned()]).contains(&Lane::Pinned));
    }

    #[test]
    fn an_empty_list_yields_no_rows_and_no_panic() {
        let p = Palette::default();
        let empty: Vec<Fake> = Vec::new();
        assert!(p.rows(&empty, &[], &[]).is_empty());
        let mut q = Palette::default();
        q.move_highlight(1, 0);
        assert_eq!(q.highlight, 0);
    }
}
