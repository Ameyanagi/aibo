//! Scoring and the markdown report.
//!
//! > Run against every candidate model and prompt version; record pass rate and
//! > TTFT in a table. Rerun on every prompt edit and every model binding change.
//! > — §5

use std::collections::BTreeMap;

use crate::fixture::{Fixture, Recorded};
use crate::properties::{Property, Verdict, evaluate};

/// The scored result for one (fixture, candidate) pair.
#[derive(Debug, Clone)]
pub struct Scored {
    /// Fixture id.
    pub fixture_id: String,
    /// Candidate label.
    pub candidate: String,
    /// Per-property verdicts.
    pub verdicts: Vec<(Property, Verdict)>,
    /// Time to first token, if recorded.
    pub ttft_ms: Option<u128>,
    /// Provider error, if the call failed.
    pub error: Option<String>,
    /// The model's reply, kept for the failure listing.
    pub output: String,
}

impl Scored {
    /// Did every scored property hold?
    pub fn clean(&self) -> bool {
        self.error.is_none()
            && self
                .verdicts
                .iter()
                .all(|(_, v)| !matches!(v, Verdict::Fail(_)))
    }
}

/// Join recorded outputs against fixtures and score them.
///
/// Rows whose `fixture_id` is unknown are returned as the second element rather
/// than silently dropped — a typo in a fixture id would otherwise show up as a
/// suspiciously good pass rate.
pub fn score(fixtures: &[Fixture], recorded: &[Recorded]) -> (Vec<Scored>, Vec<String>) {
    let index: BTreeMap<&str, &Fixture> = fixtures.iter().map(|f| (f.id.as_str(), f)).collect();

    let mut scored = Vec::new();
    let mut orphans = Vec::new();
    for row in recorded {
        let Some(fixture) = index.get(row.fixture_id.as_str()) else {
            orphans.push(row.fixture_id.clone());
            continue;
        };
        scored.push(Scored {
            fixture_id: row.fixture_id.clone(),
            candidate: row.candidate.clone(),
            verdicts: if row.error.is_some() {
                Vec::new()
            } else {
                evaluate(fixture, &row.output)
            },
            ttft_ms: row.ttft_ms,
            error: row.error.clone(),
            output: row.output.clone(),
        });
    }
    (scored, orphans)
}

/// Per-candidate aggregate.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// Cases attempted.
    pub cases: usize,
    /// Cases where every scored property held.
    pub clean: usize,
    /// Cases that errored.
    pub errors: usize,
    /// `(passes, scored)` per property.
    pub per_property: BTreeMap<Property, (usize, usize)>,
    /// Sorted TTFT samples.
    pub ttft: Vec<u128>,
}

impl Summary {
    /// Pass rate over cases with no failing property.
    pub fn clean_rate(&self) -> f64 {
        if self.cases == 0 {
            0.0
        } else {
            self.clean as f64 / self.cases as f64
        }
    }

    /// A percentile of the TTFT samples (`p` in 0..=100).
    pub fn ttft_percentile(&self, p: usize) -> Option<u128> {
        if self.ttft.is_empty() {
            return None;
        }
        let index = (self.ttft.len().saturating_sub(1) * p) / 100;
        self.ttft.get(index).copied()
    }
}

/// Aggregate scored rows per candidate.
pub fn summarise(scored: &[Scored]) -> BTreeMap<String, Summary> {
    let mut out: BTreeMap<String, Summary> = BTreeMap::new();
    for row in scored {
        let entry = out.entry(row.candidate.clone()).or_default();
        entry.cases += 1;
        if row.error.is_some() {
            entry.errors += 1;
            continue;
        }
        if row.clean() {
            entry.clean += 1;
        }
        for (property, verdict) in &row.verdicts {
            if !verdict.is_scored() {
                continue;
            }
            let slot = entry.per_property.entry(*property).or_insert((0, 0));
            slot.1 += 1;
            if verdict.is_pass() {
                slot.0 += 1;
            }
        }
        if let Some(ttft) = row.ttft_ms {
            entry.ttft.push(ttft);
        }
    }
    for summary in out.values_mut() {
        summary.ttft.sort_unstable();
    }
    out
}

/// Render the whole report as markdown.
pub fn markdown(
    summaries: &BTreeMap<String, Summary>,
    scored: &[Scored],
    max_failures: usize,
) -> String {
    let mut out = String::new();

    out.push_str("## S9 — pass rate and TTFT per candidate\n\n");
    out.push_str("| Candidate | Cases | Clean | Errors | TTFT p50 | TTFT p90 |");
    for property in Property::all() {
        out.push_str(&format!(" {} |", property.as_str()));
    }
    out.push_str("\n|---|---|---|---|---|---|");
    for _ in Property::all() {
        out.push_str("---|");
    }
    out.push('\n');

    for (candidate, summary) in summaries {
        out.push_str(&format!(
            "| `{}` | {} | {:.0}% | {} | {} | {} |",
            candidate,
            summary.cases,
            summary.clean_rate() * 100.0,
            summary.errors,
            summary
                .ttft_percentile(50)
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            summary
                .ttft_percentile(90)
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
        ));
        for property in Property::all() {
            match summary.per_property.get(&property) {
                None => out.push_str(" n/a |"),
                Some((passes, total)) => {
                    let pct = if *total == 0 {
                        0.0
                    } else {
                        *passes as f64 / *total as f64 * 100.0
                    };
                    out.push_str(&format!(" {pct:.0}% |"));
                }
            }
        }
        out.push('\n');
    }

    out.push_str("\n## Failures\n\n");
    let mut shown = 0usize;
    for row in scored {
        if shown >= max_failures {
            out.push_str(&format!("\n_(truncated at {max_failures} failures)_\n"));
            break;
        }
        if let Some(error) = &row.error {
            out.push_str(&format!(
                "- **{}** / `{}` — ERROR: {}\n",
                row.fixture_id, row.candidate, error
            ));
            shown += 1;
            continue;
        }
        let failed: Vec<String> = row
            .verdicts
            .iter()
            .filter_map(|(p, v)| match v {
                Verdict::Fail(reason) => Some(format!("{}: {reason}", p.as_str())),
                _ => None,
            })
            .collect();
        if failed.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "- **{}** / `{}` — {}\n  - reply: `{}`\n",
            row.fixture_id,
            row.candidate,
            failed.join("; "),
            row.output
                .replace('\n', "\\n")
                .chars()
                .take(200)
                .collect::<String>()
        ));
        shown += 1;
    }
    if shown == 0 {
        out.push_str("_none_\n");
    }
    out
}
