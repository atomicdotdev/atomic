//! The canonical triage projection (milestone T3) — the domain home of the
//! report `atomic triage review` builds.
//!
//! [`report::build_report`] walks the change → file → task → intent →
//! acceptance-criterion join over the knowledge graph, gates each reached
//! intent, attaches best-effort provenance, and mints a reproducible
//! `urn:atomic:triage:<hash>` reference over the pinned inputs. Pure and
//! read-only — it creates no records. The service layer's
//! `GenerateTriageReview` serves it; the CLI renders its skins
//! client-side; outpost reads it over its reactor.

pub mod model;
pub mod report;

pub use model::*;
pub use report::{build_report, hunk_display_summaries, HunkDisplaySummary};
