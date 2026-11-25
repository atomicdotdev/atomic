//! Advisory evaluation of measurements supplied by language/tool adapters.
//!
//! This module does not extract metrics, infer missing values, or decide whether
//! a requirement is unmet. The caller supplies comparable before/after facts
//! using the metric card's counting convention and selects a suitable profile.

use crate::catalog::MetricScope;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Counting conventions are versioned independently of advisory project limits.
pub const EXTRACTION_CONVENTION: &str = "atomic.metrics.syntax.v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricLocation {
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractedMetric {
    pub observation: MetricObservation,
    pub path: String,
    pub scope: MetricScope,
    /// Anonymous or overloaded owners cannot be matched across independently
    /// extracted snapshots by their display ordinal alone.
    #[serde(default, skip_serializing_if = "is_false")]
    pub subject_identity_ambiguous: bool,
    pub before_location: Option<MetricLocation>,
    pub after_location: Option<MetricLocation>,
    pub convention: String,
    pub detail: String,
    pub delta: Option<i64>,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    MethodLength,
    ClassLength,
    MethodCount,
    ParameterCount,
    NestingDepth,
    CyclomaticComplexity,
    ClassComplexity,
    DependencyFanOut,
    PublicSurface,
    DuplicatedTokens,
}

impl Metric {
    pub const ALL: [Self; 10] = [
        Self::MethodLength,
        Self::ClassLength,
        Self::MethodCount,
        Self::ParameterCount,
        Self::NestingDepth,
        Self::CyclomaticComplexity,
        Self::ClassComplexity,
        Self::DependencyFanOut,
        Self::PublicSurface,
        Self::DuplicatedTokens,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::MethodLength => "metric.method-length",
            Self::ClassLength => "metric.class-length",
            Self::MethodCount => "metric.method-count",
            Self::ParameterCount => "metric.parameter-count",
            Self::NestingDepth => "metric.nesting-depth",
            Self::CyclomaticComplexity => "metric.cyclomatic-complexity",
            Self::ClassComplexity => "metric.class-complexity",
            Self::DependencyFanOut => "metric.dependency-fan-out",
            Self::PublicSurface => "metric.public-surface",
            Self::DuplicatedTokens => "metric.duplicated-tokens",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Measurement {
    Measured { value: u32 },
    Unavailable { reason: String },
    NotApplicable { reason: String },
}

impl Measurement {
    fn value(&self) -> Option<u32> {
        match self {
            Self::Measured { value } => Some(*value),
            _ => None,
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Unavailable { reason } | Self::NotApplicable { reason }
                if reason.trim().is_empty() =>
            {
                Err("unavailable/not-applicable measurements require a reason")
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricObservation {
    pub metric: Metric,
    /// Caller-owned recorded entity/subject identity; source anchors and analyzer
    /// provenance belong to the review adapter, not this policy-only module.
    pub subject: String,
    pub before: Measurement,
    pub after: Measurement,
}

impl MetricObservation {
    pub fn delta(&self) -> Option<i64> {
        self.before
            .value()
            .zip(self.after.value())
            .map(|(before, after)| i64::from(after) - i64::from(before))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Threshold {
    /// Inclusive review trigger, not a defect/acceptance-failure boundary.
    pub review_at: u32,
    pub high_review_at: Option<u32>,
}

impl Threshold {
    fn validate(self) -> Result<(), &'static str> {
        if self
            .high_review_at
            .is_some_and(|high| high <= self.review_at)
        {
            return Err("high review threshold must exceed the review threshold");
        }
        Ok(())
    }
}

/// No universal limits are installed. Callers select a named profile appropriate
/// for the language, code role and project, excluding generated code as needed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricProfile {
    pub name: String,
    pub limits: BTreeMap<Metric, Threshold>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewBand {
    NotConfigured,
    WithinLimit,
    Review,
    HighReview,
    Unknown,
    NotApplicable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricAssessment {
    pub observation: MetricObservation,
    pub profile: String,
    pub threshold: Option<Threshold>,
    /// Available only when both samples were measured, never by substituting 0.
    pub delta: Option<i64>,
    pub band: ReviewBand,
}

/// Evaluate a metric against explicit advisory limits. A review band is a
/// prioritization signal; it neither creates a finding nor fails a criterion.
///
/// # Errors
/// Rejects empty subject/profile identities, invalid threshold ordering, and
/// unavailable/not-applicable samples without an explanation.
pub fn evaluate(
    observation: &MetricObservation,
    profile: &MetricProfile,
) -> Result<MetricAssessment, &'static str> {
    if observation.subject.trim().is_empty() || profile.name.trim().is_empty() {
        return Err("metric evaluation requires a subject and named profile");
    }
    observation.before.validate()?;
    observation.after.validate()?;
    for limit in profile.limits.values() {
        limit.validate()?;
    }
    let threshold = profile.limits.get(&observation.metric).copied();
    let delta = observation.delta();
    let band = match &observation.after {
        Measurement::Unavailable { .. } => ReviewBand::Unknown,
        Measurement::NotApplicable { .. } => ReviewBand::NotApplicable,
        Measurement::Measured { value } => match threshold {
            None => ReviewBand::NotConfigured,
            Some(limit) if limit.high_review_at.is_some_and(|high| *value >= high) => {
                ReviewBand::HighReview
            }
            Some(limit) if *value >= limit.review_at => ReviewBand::Review,
            Some(_) => ReviewBand::WithinLimit,
        },
    };
    Ok(MetricAssessment {
        observation: observation.clone(),
        profile: profile.name.clone(),
        threshold,
        delta,
        band,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> MetricProfile {
        MetricProfile {
            name: "example-project-policy".into(),
            limits: BTreeMap::from([(
                Metric::MethodLength,
                Threshold {
                    review_at: 50,
                    high_review_at: Some(100),
                },
            )]),
        }
    }

    fn observation(after: Measurement) -> MetricObservation {
        MetricObservation {
            metric: Metric::MethodLength,
            subject: "src/report.ts#ReportBuilder.build".into(),
            before: Measurement::Measured { value: 34 },
            after,
        }
    }

    #[test]
    fn thresholds_are_inclusive_configurable_review_signals() {
        for (value, band) in [
            (49, ReviewBand::WithinLimit),
            (50, ReviewBand::Review),
            (99, ReviewBand::Review),
            (100, ReviewBand::HighReview),
        ] {
            let result =
                evaluate(&observation(Measurement::Measured { value }), &profile()).unwrap();
            assert_eq!(result.band, band);
            assert_eq!(result.delta, Some(i64::from(value) - 34));
        }
    }

    #[test]
    fn missing_or_inapplicable_is_neither_zero_nor_a_pass() {
        let missing = evaluate(
            &observation(Measurement::Unavailable {
                reason: "No extractor for this syntax".into(),
            }),
            &profile(),
        )
        .unwrap();
        assert_eq!(missing.band, ReviewBand::Unknown);
        assert_eq!(missing.delta, None);
        let mut record = observation(Measurement::NotApplicable {
            reason: "No classes in this language construct".into(),
        });
        record.metric = Metric::ClassLength;
        assert_eq!(
            evaluate(&record, &profile()).unwrap().band,
            ReviewBand::NotApplicable
        );
        record.before = Measurement::Unavailable {
            reason: "No baseline".into(),
        };
        record.after = Measurement::Measured { value: 20 };
        assert_eq!(evaluate(&record, &profile()).unwrap().delta, None);
    }

    #[test]
    fn zero_is_real_and_large_deltas_do_not_overflow() {
        let mut record = observation(Measurement::Measured { value: 0 });
        record.before = Measurement::Measured { value: u32::MAX };
        assert_eq!(
            evaluate(&record, &profile()).unwrap().delta,
            Some(-i64::from(u32::MAX))
        );
        record.before = Measurement::Measured { value: 0 };
        record.after = Measurement::Measured { value: u32::MAX };
        assert_eq!(
            evaluate(&record, &profile()).unwrap().delta,
            Some(i64::from(u32::MAX))
        );
    }

    #[test]
    fn absent_limits_are_not_an_implicit_quality_gate() {
        let result = evaluate(
            &observation(Measurement::Measured { value: 112 }),
            &MetricProfile {
                name: "observe-only".into(),
                limits: BTreeMap::new(),
            },
        )
        .unwrap();
        assert_eq!(result.band, ReviewBand::NotConfigured);
        assert_eq!(result.delta, Some(78));
        let roundtrip: MetricAssessment =
            serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
        assert_eq!(roundtrip, result);
    }

    #[test]
    fn malformed_profiles_and_samples_are_rejected() {
        let record = observation(Measurement::Measured { value: 10 });
        let mut policy = profile();
        policy
            .limits
            .get_mut(&Metric::MethodLength)
            .unwrap()
            .high_review_at = Some(50);
        assert!(evaluate(&record, &policy).is_err());
        assert!(evaluate(
            &observation(Measurement::Unavailable { reason: " ".into() }),
            &profile()
        )
        .is_err());
        let mut empty = record;
        empty.subject.clear();
        assert!(evaluate(&empty, &profile()).is_err());
    }
}
