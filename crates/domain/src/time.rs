//! Wall-clock timestamps for durable deadlines.
//!
//! Durable deadlines are stored as UTC; in-process waiting uses a monotonic
//! clock (ADR-13). This type is only the stored form.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, UTC. May be negative only for values
/// before 1970, which the engine never produces but storage may contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(transparent)
)]
pub struct UtcTimestamp(i64);

impl UtcTimestamp {
    /// Wraps a stored millisecond value.
    pub const fn from_unix_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// The stored millisecond value.
    pub const fn unix_millis(self) -> i64 {
        self.0
    }

    /// Converts a system time, saturating outside the representable range.
    pub fn from_system_time(time: SystemTime) -> Self {
        let millis = match time.duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
            Err(before) => i64::try_from(before.duration().as_millis())
                .map(|m| -m)
                .unwrap_or(i64::MIN),
        };
        Self(millis)
    }

    /// This timestamp plus a duration, or `None` on overflow.
    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        let millis = i64::try_from(duration.as_millis()).ok()?;
        self.0.checked_add(millis).map(Self)
    }
}

impl fmt::Display for UtcTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_system_time() {
        let t = UNIX_EPOCH + Duration::from_millis(1_500);
        assert_eq!(UtcTimestamp::from_system_time(t).unix_millis(), 1_500);
    }

    #[test]
    fn addition_is_checked() {
        let t = UtcTimestamp::from_unix_millis(i64::MAX - 1);
        assert_eq!(t.checked_add(Duration::from_millis(2)), None);
        assert_eq!(
            UtcTimestamp::from_unix_millis(0).checked_add(Duration::from_secs(1)),
            Some(UtcTimestamp::from_unix_millis(1_000))
        );
    }
}
