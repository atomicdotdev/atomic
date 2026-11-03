//! Compatibility for untagged agent timestamps: new writes use milliseconds,
//! while historical provenance and session records may contain seconds.

use chrono::{DateTime, Utc};

/// Values below this magnitude retain the historical seconds interpretation.
/// Untagged timestamps near the epoch are inherently ambiguous; this convention
/// supports existing records without rewriting content-addressed payloads.
pub const MILLISECOND_EPOCH_THRESHOLD: u64 = 100_000_000_000;

/// Interpret an agent timestamp as milliseconds, accepting legacy seconds.
///
/// Suitable for comparisons across old and new records. The multiplication is
/// bounded by the threshold, so even `i64::MIN` and `i64::MAX` cannot overflow.
pub fn epoch_millis(timestamp: i64) -> i64 {
    if timestamp.unsigned_abs() < MILLISECOND_EPOCH_THRESHOLD {
        timestamp * 1000
    } else {
        timestamp
    }
}

/// Decode current millisecond and historical second agent epochs without
/// modifying their serialized values. Values outside Chrono's range return None.
pub fn epoch_datetime(timestamp: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(epoch_millis(timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_and_milliseconds_preserve_their_precision() {
        for (raw, expected) in [
            (1_792_920_379, "2026-10-25T09:26:19+00:00"),
            (1_792_920_379_589, "2026-10-25T09:26:19.589+00:00"),
            (0, "1970-01-01T00:00:00+00:00"),
        ] {
            assert_eq!(epoch_datetime(raw).unwrap().to_rfc3339(), expected);
        }
    }

    #[test]
    fn comparisons_handle_mixed_units_and_subsecond_actions() {
        assert!(epoch_millis(1_792_920_379_589) < epoch_millis(1_792_920_380));
        assert!(epoch_millis(1_792_920_379_100) < epoch_millis(1_792_920_379_589));
        assert_eq!(epoch_millis(1_792_920_379), 1_792_920_379_000);
    }

    #[test]
    fn negative_and_extreme_values_do_not_overflow() {
        assert_eq!(epoch_millis(-1), -1000);
        assert_eq!(epoch_millis(-1_792_920_379_589), -1_792_920_379_589);
        assert_eq!(epoch_millis(i64::MIN), i64::MIN);
        assert_eq!(epoch_millis(i64::MAX), i64::MAX);
        assert!(epoch_datetime(i64::MIN).is_none());
        assert!(epoch_datetime(i64::MAX).is_none());
    }
}
