//! The `@` file finder (§P9+): yuru-backed fuzzy search over the user's files.
//!
//! Typing `@` in the composer opens this as a floating overlay, the same
//! surface pattern as the model quick-pick. Matching is
//! [`yuru_core`]/[`yuru_ja`] rather than the palette's nucleo, and that is the
//! reason the feature exists at all: yuru expands Japanese candidates with
//! kana, romaji and Lindera kanji readings, so typing `toukei` finds
//! `統計資料.pdf` — the owner's own tool, doing the one thing nucleo cannot.
//!
//! Candidates come from the runtime (`UiRequest::ListFiles`), which owns the
//! bounded directory walk; this module owns only the index and the query.

use yuru_core::{Candidate, SearchConfig, build_index, search};
use yuru_ja::JapaneseBackend;

use crate::bridge::FileCandidate;

/// How many results the overlay shows.
const RESULT_LIMIT: usize = 50;

/// The finder's state, held by the panel like the quick-pick's.
pub struct FileFinder {
    /// Whether the overlay is up.
    pub open: bool,
    /// The current query.
    pub query: String,
    /// Highlighted result, indexing [`FileFinder::results`].
    pub highlight: usize,
    /// The runtime's candidate list, in walk order.
    candidates: Vec<FileCandidate>,
    /// yuru's index over `candidates`, ids matching their positions.
    index: Vec<Candidate>,
    config: SearchConfig,
}

impl std::fmt::Debug for FileFinder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileFinder")
            .field("open", &self.open)
            .field("query", &self.query)
            .field("highlight", &self.highlight)
            .field("candidates", &self.candidates.len())
            .finish_non_exhaustive()
    }
}

impl Default for FileFinder {
    fn default() -> Self {
        let config = SearchConfig {
            limit: RESULT_LIMIT,
            ..SearchConfig::default()
        };
        Self {
            open: false,
            query: String::new(),
            highlight: 0,
            candidates: Vec::new(),
            index: Vec::new(),
            config,
        }
    }
}

impl FileFinder {
    /// Open with a fresh query. The candidate list, if already loaded, stays —
    /// re-walking on every `@` would make the second open slower than the
    /// first for no reason; the runtime refreshes it anyway on each request.
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.highlight = 0;
    }

    /// Close, dropping the query but keeping the index warm.
    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.highlight = 0;
    }

    /// Replace the candidate list and rebuild the reading index.
    ///
    /// Indexed by **file name**, not the full path. The first cut indexed the
    /// display path, and at real scale the directory components were pure
    /// ranking noise: `tesutofo` scattered across
    /// `…/torture-test by CreativeTools…/files/3DBenchy….stl` outranked the
    /// kana match in `テスト フォローアップ報告書` (owner screenshots,
    /// 2026-08-01). A file picker matches names; the path is context.
    pub fn set_candidates(&mut self, files: Vec<FileCandidate>) {
        self.index = build_index(
            files.iter().map(|file| file_name(&file.display).to_owned()),
            japanese_backend(),
            &self.config,
        );
        self.candidates = files;
        self.highlight = 0;
    }

    /// Update the query, resetting the highlight — a new result set under an
    /// old highlight commits something never looked at.
    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.highlight = 0;
    }

    /// Move the highlight within the current results, clamped.
    pub fn move_highlight(&mut self, delta: isize) {
        let count = self.results().len();
        if count == 0 {
            self.highlight = 0;
            return;
        }
        let current = isize::try_from(self.highlight.min(count - 1)).unwrap_or(0);
        let moved = current.saturating_add(delta).clamp(0, (count - 1) as isize);
        self.highlight = usize::try_from(moved).unwrap_or(0);
    }

    /// The candidates matching the current query, best first; the whole list
    /// (bounded) while the query is empty.
    pub fn results(&self) -> Vec<&FileCandidate> {
        if self.query.trim().is_empty() {
            return self.candidates.iter().take(RESULT_LIMIT).collect();
        }
        search(&self.query, &self.index, japanese_backend(), &self.config)
            .into_iter()
            .filter_map(|scored| self.candidates.get(scored.id))
            .collect()
    }

    /// The highlighted candidate, if any.
    pub fn highlighted(&self) -> Option<FileCandidate> {
        self.results()
            .get(self.highlight)
            .map(|file| (*file).clone())
    }
}

/// The last path component of a display path.
pub fn file_name(display: &str) -> &str {
    display.rsplit('/').next().unwrap_or(display)
}

/// One reading backend for the whole UI.
///
/// The embedded Lindera dictionary is the expensive part of yuru; the finder
/// and the slash popup both match through this single instance rather than
/// each loading their own copy.
pub(crate) fn japanese_backend() -> &'static JapaneseBackend {
    static BACKEND: std::sync::LazyLock<JapaneseBackend> =
        std::sync::LazyLock::new(JapaneseBackend::default);
    &BACKEND
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finder_with(names: &[&str]) -> FileFinder {
        let mut finder = FileFinder::default();
        finder.set_candidates(
            names
                .iter()
                .map(|name| FileCandidate {
                    display: format!("~/Documents/{name}"),
                    path: format!("/Users/x/Documents/{name}"),
                })
                .collect(),
        );
        finder
    }

    /// The reason yuru is here: romaji finds kanji filenames.
    #[test]
    fn romaji_finds_a_kanji_filename() {
        let mut finder = finder_with(&["統計資料.pdf", "report.md", "写真.png"]);
        finder.set_query("toukei".to_owned());
        let results = finder.results();
        assert!(
            results
                .first()
                .is_some_and(|file| file.display.contains("統計資料")),
            "{results:?}"
        );
    }

    #[test]
    fn latin_queries_still_match() {
        let mut finder = finder_with(&["統計資料.pdf", "report.md"]);
        finder.set_query("rep".to_owned());
        assert!(
            finder
                .results()
                .first()
                .is_some_and(|file| file.display.contains("report.md"))
        );
    }

    #[test]
    fn an_empty_query_lists_everything_bounded() {
        let finder = finder_with(&["a.txt", "b.txt"]);
        assert_eq!(finder.results().len(), 2);
    }

    #[test]
    fn the_highlight_clamps() {
        let mut finder = finder_with(&["a.txt", "b.txt"]);
        finder.move_highlight(10);
        assert_eq!(finder.highlight, 1);
        finder.move_highlight(-10);
        assert_eq!(finder.highlight, 0);
    }
}

#[cfg(test)]
mod repro {
    use super::*;

    /// The ranking half of the owner's miss: at real scale, `tesutofo`
    /// scattered across long junk paths beat the kana match. With names
    /// indexed instead of paths, the kana file must win against a crowd of
    /// 3DBenchy-style noise in deep directories.
    #[test]
    fn kana_names_outrank_scattered_latin_path_noise() {
        let mut finder = FileFinder::default();
        let mut files = vec![FileCandidate {
            display: "~/Downloads/followup-テスト フォローアップ報告書-2026-04-03.csv".to_owned(),
            path: "/x/report.csv".to_owned(),
        }];
        for index in 0..30 {
            files.push(FileCandidate {
                display: format!(
                    "~/Downloads/#3DBenchy - The jolly 3D printing torture-test by \
                     CreativeTools.se - 763622 - part 2 of 2/files/3DBenchy_-_Multi-part_-\
                     _Single_-_Doorframe_port_{index}_-_3DBenchy.com.stl"
                ),
                path: format!("/x/{index}.stl"),
            });
        }
        finder.set_candidates(files);
        finder.set_query("tesutofo".to_owned());
        let results = finder.results();
        assert!(
            results
                .first()
                .is_some_and(|file| file.display.contains("テスト")),
            "the kana name must outrank path noise: {:?}",
            results
                .iter()
                .take(3)
                .map(|f| f.display.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// The owner's exact miss (2026-08-01): katakana with an ideographic
    /// space, queried in romaji. Matching was never the problem — the walk
    /// was starving the file's root — but this pins the matcher half.
    #[test]
    fn katakana_with_ideographic_space_matches_romaji() {
        let mut finder = FileFinder::default();
        finder.set_candidates(vec![
            FileCandidate {
                display: "~/Documents/テスト　フォローアップ.txt".to_owned(),
                path: "/x/テスト　フォローアップ.txt".to_owned(),
            },
            FileCandidate {
                display: "~/Documents/report.md".to_owned(),
                path: "/x/report.md".to_owned(),
            },
        ]);
        finder.set_query("tesutofo".to_owned());
        let results = finder.results();
        assert!(
            results
                .first()
                .is_some_and(|f| f.display.contains("テスト")),
            "tesutofo should match テスト　フォローアップ: {results:?}"
        );
    }
}

#[cfg(test)]
mod exact_repro {
    use super::*;
    #[test]
    fn exact_owner_filename() {
        let mut finder = FileFinder::default();
        finder.set_candidates(vec![
            FileCandidate {
                display: "~/Downloads/followup-テスト フォローアップ報告書-2026-04-03.csv"
                    .to_owned(),
                path: "/x/a.csv".to_owned(),
            },
            FileCandidate {
                display: "~/Downloads/テスト.dmg".to_owned(),
                path: "/x/b.dmg".to_owned(),
            },
            FileCandidate {
                display: "~/Documents/report.md".to_owned(),
                path: "/x/c.md".to_owned(),
            },
        ]);
        for query in ["tesutofo", "tesuto", "fo-roappu", "houkokusho"] {
            finder.set_query(query.to_owned());
            let results = finder.results();
            eprintln!(
                "QUERY {query:?} => {:?}",
                results
                    .iter()
                    .map(|f| f.display.as_str())
                    .collect::<Vec<_>>()
            );
        }
        finder.set_query("tesutofo".to_owned());
        assert!(!finder.results().is_empty(), "tesutofo matched nothing");
    }
}
