//! S9 — Complete quality + eval harness (§5, §20).
//!
//! > Chat-instruct vs FIM vs local small model for Complete (§5), measured on
//! > ~50 real fixtures in Japanese and English. Pass rate and TTFT per
//! > candidate. **The harness matters more than the answer.** — §20
//!
//! The library half is deliberately separable from the binary:
//!
//! - [`fixture`] — the corpus on disk and the recorded outputs.
//! - [`prompt`] — version-stamped prompt assembly, a copy of §5's per-surface
//!   specs so prompt versions can be swept against each other.
//! - [`live`] — the one network call, streaming, with TTFT measurement.
//! - [`properties`] — the pure property assertions. **This is the part that
//!   graduates into `aibo-core` and CI (§18 tier 1).** It has no I/O and no
//!   network, and is fully unit-tested.
//! - [`report`] — join, aggregate, render markdown.
//!
//! `properties` never depends on `live`, so an offline re-score of yesterday's
//! outputs after a property change is the same code path as a live sweep.

pub mod fixture;
pub mod live;
pub mod prompt;
pub mod properties;
pub mod report;
