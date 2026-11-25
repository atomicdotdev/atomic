//! Tool-neutral engineering audits and manageability metrics.
//!
//! Cards describe what to inspect and how to interpret evidence. Metric limits
//! are advisory, not acceptance verdicts. Parsing is an optional adapter: enable
//! `structural` to run the existing checks using `atomic-semantic` grammars.
//! Repository scope, finding numbering and publication remain outside this crate.

#![doc = include_str!("../README.md")]

pub mod catalog;
#[cfg(feature = "structural")]
pub mod extract;
pub mod impact;
pub mod metrics;
pub mod risk;
#[cfg(feature = "structural")]
pub mod structural;
#[cfg(feature = "structural")]
mod substantive;

pub use catalog::{AuditCard, MetricCard, AUDIT_CARDS, METRIC_CARDS};
pub use metrics::{
    evaluate, Measurement, Metric, MetricAssessment, MetricObservation, MetricProfile, ReviewBand,
    Threshold,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
