// Copyright © 2026 Jalapeno Labs

//! Ergonomic constructors for the contract's shared primitives.
//!
//! The generated types are plain data, which is correct for a wire format and
//! tedious at a call site. Nobody should be computing epoch seconds by hand to
//! stamp an event, so the conversions live here, written once and tested.
//!
//! These live inside the crate rather than under `proto/helpers/rust/` as the
//! protobuf guidelines suggest, for the same reason the generated code does:
//! `cargo publish` only packages files beneath the crate directory.

use crate::proto::common::v1::{Money, Timestamp};
use std::time::{SystemTime, UNIX_EPOCH};

/// The timezone used when an originating zone is unknown.
///
/// Every instant Arsox records is UTC on the wire. The zone is presentational
/// and never changes the instant, only how a client renders it.
pub const DEFAULT_TIMEZONE: &str = "Etc/UTC";

/// Nanoseconds in one second, as the fractional field is defined.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// Billionths in one whole currency unit, as `Money.nanos` is defined.
///
/// Numerically the same as [`NANOS_PER_SECOND`] and named separately on
/// purpose: money and time share an integer scale by coincidence, and a single
/// constant serving both would make either one look like a unit error.
const NANOS_PER_UNIT: i128 = 1_000_000_000;

impl Timestamp {
    /// Builds a timestamp from a system instant, tagged with an IANA zone.
    ///
    /// # Examples
    ///
    /// ```
    /// use arsox_sdk::proto::common::v1::Timestamp;
    /// use std::time::UNIX_EPOCH;
    ///
    /// let stamp = Timestamp::from_system_time(UNIX_EPOCH, "America/Denver");
    /// assert_eq!(stamp.epoch_seconds, 0);
    /// assert_eq!(stamp.timezone, "America/Denver");
    /// ```
    #[must_use]
    pub fn from_system_time(instant: SystemTime, timezone: &str) -> Self {
        // Instants before the epoch are representable on the wire, so they are
        // carried rather than clamped: `epoch_seconds` is explicitly signed.
        let (seconds, nanos) = match instant.duration_since(UNIX_EPOCH) {
            Ok(elapsed) => (
                i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
                elapsed.subsec_nanos(),
            ),
            Err(before_epoch) => {
                let behind = before_epoch.duration();
                let seconds = i64::try_from(behind.as_secs()).unwrap_or(i64::MAX);
                match behind.subsec_nanos() {
                    // Exactly on a second boundary, so no borrow is needed.
                    0 => (-seconds, 0),
                    // `nanos` is defined as never negative, so borrow a second
                    // and carry the remainder forward.
                    fraction => (-seconds - 1, 1_000_000_000 - fraction),
                }
            }
        };

        Self {
            epoch_seconds: seconds,
            nanos,
            timezone: timezone.to_owned(),
        }
    }

    /// Builds a timestamp for the current instant, in UTC.
    #[must_use]
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now(), DEFAULT_TIMEZONE)
    }

    /// Returns the instant as whole milliseconds since the epoch.
    ///
    /// Lossy by construction: sub-millisecond precision is dropped. Named to say
    /// so, because the wire format is nanosecond-capable and a silent truncation
    /// on the way into a millisecond-only language is how precision quietly
    /// disappears.
    #[must_use]
    pub fn to_epoch_millis_lossy(&self) -> i64 {
        self.epoch_seconds
            .saturating_mul(1_000)
            .saturating_add(i64::from(self.nanos) / 1_000_000)
    }
}

impl Money {
    /// Builds an amount in US dollars from whole units and billionths.
    ///
    /// # Examples
    ///
    /// ```
    /// use arsox_sdk::proto::common::v1::Money;
    ///
    /// let ceiling = Money::usd(40, 0);
    /// assert_eq!(ceiling.currency_code, "USD");
    /// assert_eq!(ceiling.units, 40);
    /// ```
    #[must_use]
    pub fn usd(units: i64, nanos: i32) -> Self {
        Self {
            currency_code: "USD".to_owned(),
            units,
            nanos,
        }
    }

    /// The amount as a single count of billionths, for comparing two sums.
    ///
    /// Accumulating spend against a ceiling needs one number rather than a pair,
    /// and `i128` holds every amount the two 64-bit halves can express without
    /// the saturation an `i64` would need.
    ///
    /// # Examples
    ///
    /// ```
    /// use arsox_sdk::proto::common::v1::Money;
    ///
    /// assert_eq!(Money::usd(0, 750_000_000).to_nanos(), 750_000_000);
    /// assert_eq!(Money::usd(2, 500_000_000).to_nanos(), 2_500_000_000);
    /// ```
    #[must_use]
    pub fn to_nanos(&self) -> i128 {
        i128::from(self.units) * NANOS_PER_UNIT + i128::from(self.nanos)
    }

    /// Renders the amount as a decimal string, without going through a float.
    ///
    /// A single model request can cost a small fraction of a cent, and rendering
    /// through `f64` is how a ceiling drifts away from the invoice it was meant
    /// to predict.
    #[must_use]
    pub fn to_decimal_string(&self) -> String {
        let negative = self.units < 0 || self.nanos < 0;
        let units = self.units.unsigned_abs();
        let nanos = self.nanos.unsigned_abs();
        let sign = if negative { "-" } else { "" };

        format!("{sign}{units}.{nanos:09}")
    }
}

/// Total nanoseconds a duration represents, for comparing two spans.
///
/// Saturates rather than wrapping, because a span long enough to overflow is
/// already outside anything the satellite will wait on.
#[must_use]
pub fn duration_to_nanos(duration: &crate::proto::common::v1::Duration) -> i64 {
    duration
        .seconds
        .saturating_mul(NANOS_PER_SECOND)
        .saturating_add(i64::from(duration.nanos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    #[test]
    fn the_epoch_converts_to_zero() {
        let stamp = Timestamp::from_system_time(UNIX_EPOCH, DEFAULT_TIMEZONE);

        assert_eq!(stamp.epoch_seconds, 0);
        assert_eq!(stamp.nanos, 0);
        assert_eq!(stamp.timezone, "Etc/UTC");
    }

    #[test]
    fn sub_second_precision_survives_the_conversion() {
        let instant = UNIX_EPOCH + StdDuration::new(1_700_000_000, 123_456_789);

        // Compared against what `SystemTime` actually holds rather than against
        // the literal above. Clock granularity is platform specific: Windows
        // stores 100ns ticks and drops the last two digits, Linux keeps all
        // nine. The contract is nanosecond-capable either way, and what this
        // asserts is that the conversion adds no loss of its own.
        let held = instant
            .duration_since(UNIX_EPOCH)
            .expect("instant is after the epoch");
        let stamp = Timestamp::from_system_time(instant, DEFAULT_TIMEZONE);

        assert_eq!(stamp.epoch_seconds, 1_700_000_000);
        assert_eq!(stamp.nanos, held.subsec_nanos());
        assert!(stamp.nanos > 0, "sub-second precision should not be zeroed");
    }

    #[test]
    fn instants_before_the_epoch_keep_a_non_negative_nanos_field() {
        // The contract defines `nanos` as never negative, so an instant half a
        // second before the epoch borrows a second rather than storing -500ms.
        let instant = UNIX_EPOCH - StdDuration::from_millis(500);
        let stamp = Timestamp::from_system_time(instant, DEFAULT_TIMEZONE);

        assert_eq!(stamp.epoch_seconds, -1);
        assert_eq!(stamp.nanos, 500_000_000);
        assert_eq!(stamp.to_epoch_millis_lossy(), -500);
    }

    #[test]
    fn whole_seconds_before_the_epoch_do_not_borrow() {
        let instant = UNIX_EPOCH - StdDuration::from_secs(2);
        let stamp = Timestamp::from_system_time(instant, DEFAULT_TIMEZONE);

        assert_eq!(stamp.epoch_seconds, -2);
        assert_eq!(stamp.nanos, 0);
    }

    #[test]
    fn money_renders_without_a_float() {
        assert_eq!(Money::usd(40, 0).to_decimal_string(), "40.000000000");
        // Three quarters of a cent, which no cent-based integer could hold and
        // no float could hold exactly.
        assert_eq!(Money::usd(0, 7_500_000).to_decimal_string(), "0.007500000");
        assert_eq!(
            Money::usd(-2, -500_000_000).to_decimal_string(),
            "-2.500000000"
        );
    }

    #[test]
    fn durations_compare_by_total_nanoseconds() {
        let five_seconds = crate::proto::common::v1::Duration {
            seconds: 5,
            nanos: 0,
        };
        let five_seconds_and_change = crate::proto::common::v1::Duration {
            seconds: 5,
            nanos: 1,
        };

        assert!(duration_to_nanos(&five_seconds) < duration_to_nanos(&five_seconds_and_change));
        assert_eq!(duration_to_nanos(&five_seconds), 5_000_000_000);
    }
}
