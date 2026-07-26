//! Fixtures: the ~50 real cases per surface that §5 asks for.
//!
//! > A fixture set of ~50 real cases per surface — captured field prefixes,
//! > real selections, both Japanese and English, drawn from your own daily use.
//! > — §5
//!
//! The seed set in `fixtures/` is a *shape*, not a corpus. Grow it from real
//! use; a harness scored on invented text measures the inventor, not the model.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Which aibo surface a fixture exercises (§1, §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    /// Inline continuation of the user's text at the caret.
    Complete,
    /// Apply an instruction to a delimited selection, replacing it.
    Transform,
    /// Ordinary chat with attachments.
    Ask,
}

impl Surface {
    /// Short label for report tables.
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Complete => "complete",
            Surface::Transform => "transform",
            Surface::Ask => "ask",
        }
    }
}

/// The language a fixture is written in, declared by the author.
///
/// Declared rather than detected, because the *expected* language is the thing
/// the language-match property is scored against; detecting both sides would
/// make the property vacuous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lang {
    /// English.
    En,
    /// Japanese.
    Ja,
}

/// One evaluation case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fixture {
    /// Stable id; used as the join key with recorded outputs.
    pub id: String,
    /// Which surface this exercises.
    pub surface: Surface,
    /// The language the reply must be in (§5 "Language handling").
    pub lang: Lang,

    /// Source application, e.g. `Slack`. §5 puts this in the Complete prompt.
    #[serde(default)]
    pub app: Option<String>,
    /// The field's accessibility label, when the capture had one.
    #[serde(default)]
    pub field_label: Option<String>,

    /// Complete: the ~800 characters before the caret.
    #[serde(default)]
    pub prefix: String,
    /// Complete: text *after* the caret.
    ///
    /// §5: "completing into the middle of existing text without knowing what
    /// follows produces duplicates, and this is the single most common
    /// autocomplete failure." A fixture with a non-empty suffix is what makes
    /// [`crate::properties::Property::NoSuffixDuplication`] meaningful.
    #[serde(default)]
    pub suffix: String,

    /// Transform: the selected text, whitespace exactly as captured.
    #[serde(default)]
    pub selection: String,
    /// Transform / Ask: the user's typed instruction.
    #[serde(default)]
    pub instruction: String,

    /// Optional cap on the reply length in grapheme clusters.
    #[serde(default)]
    pub max_output_graphemes: Option<usize>,

    /// Free-text note for the human reading the report.
    #[serde(default)]
    pub notes: Option<String>,
}

impl Fixture {
    /// The text a Complete reply must not repeat.
    pub fn prefix_tail(&self, graphemes: usize) -> String {
        use unicode_segmentation::UnicodeSegmentation as _;
        let all: Vec<&str> = self.prefix.graphemes(true).collect();
        let start = all.len().saturating_sub(graphemes);
        all[start..].concat()
    }

    /// The text a Complete reply must not duplicate from after the caret.
    pub fn suffix_head(&self, graphemes: usize) -> String {
        use unicode_segmentation::UnicodeSegmentation as _;
        self.suffix.graphemes(true).take(graphemes).collect()
    }
}

/// Load every `*.json` fixture file in a directory.
///
/// Each file holds a JSON array of [`Fixture`]. Files are sorted by name so a
/// report diff between two runs is stable.
pub fn load_dir(dir: &Path) -> Result<Vec<Fixture>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read fixture directory {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();

    let mut fixtures = Vec::new();
    for path in paths {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let batch: Vec<Fixture> = serde_json::from_str(&raw)
            .with_context(|| format!("{} is not a JSON array of fixtures", path.display()))?;
        fixtures.extend(batch);
    }

    let mut seen = std::collections::HashSet::new();
    for fixture in &fixtures {
        anyhow::ensure!(
            seen.insert(fixture.id.clone()),
            "duplicate fixture id {:?} — ids are the join key with recorded outputs",
            fixture.id
        );
    }
    Ok(fixtures)
}

/// One model's answer to one fixture, as recorded on disk (JSONL).
///
/// Written by `run` and consumed by `check`, so a live sweep and an offline
/// re-score of the same outputs after a property change are the same data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recorded {
    /// Which fixture this answers.
    pub fixture_id: String,
    /// Candidate label — model id plus prompt version, e.g. `gpt-x @ complete/3`.
    pub candidate: String,
    /// The model's raw reply, before any anti-preamble filtering (§5).
    pub output: String,
    /// Time to first token, milliseconds.
    #[serde(default)]
    pub ttft_ms: Option<u128>,
    /// Total wall time, milliseconds.
    #[serde(default)]
    pub total_ms: Option<u128>,
    /// Transport or provider error, if the call failed.
    #[serde(default)]
    pub error: Option<String>,
}

/// Read a JSONL file of [`Recorded`] rows, skipping blank lines.
pub fn load_recorded(path: &Path) -> Result<Vec<Recorded>> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    raw.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            serde_json::from_str::<Recorded>(line)
                .with_context(|| format!("{}:{} is not a Recorded row", path.display(), i + 1))
        })
        .collect()
}
