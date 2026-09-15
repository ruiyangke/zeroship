//! The typed timestamp a model field carries.

use super::{DecodeValue, EncodeValue, Value, codecs::sql_types};
use crate::sql::temporal;
use zeroship_data_orm::error::DbError;

/// An instant on the portable calendar, in Unix microseconds.
///
/// This is the only Rust type the `Timestamp` codec accepts. A raw `i64` is
/// refused on purpose: the unit of a bare integer is invisible at the call
/// site, and the same integer means milliseconds everywhere a JSON or
/// JavaScript value crosses the boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcInstant(i64);

impl UtcInstant {
    /// # Errors
    /// [`DbError::ValidationFailed`] when the instant falls outside the
    /// portable calendar.
    pub fn from_unix_micros(micros: i64) -> Result<Self, DbError> {
        if temporal::is_timestamp_micros(micros) {
            Ok(Self(micros))
        } else {
            Err(outside_calendar())
        }
    }

    /// # Errors
    /// [`DbError::ValidationFailed`] when the instant falls outside the
    /// portable calendar.
    pub fn from_unix_millis(millis: i64) -> Result<Self, DbError> {
        millis
            .checked_mul(1_000)
            .ok_or_else(outside_calendar)
            .and_then(Self::from_unix_micros)
    }

    #[must_use]
    pub const fn unix_micros(self) -> i64 {
        self.0
    }

    /// The containing millisecond, rounding toward negative infinity so the
    /// result stays chronological on both sides of the epoch. This is lossy;
    /// it exists for boundaries whose contract is milliseconds.
    #[must_use]
    pub const fn floor_unix_millis(self) -> i64 {
        self.0.div_euclid(1_000)
    }
}

fn outside_calendar() -> DbError {
    DbError::validation(
        "invalid_timestamp",
        "timestamp is outside the portable calendar",
    )
}

impl EncodeValue<sql_types::Timestamp> for UtcInstant {
    fn encode_value(self) -> Result<Value, DbError> {
        Ok(Value::TimestampMicros(self.0))
    }
}

impl DecodeValue<sql_types::Timestamp> for UtcInstant {
    /// A scalar timestamp column decodes as a native microsecond value. A
    /// temporal field nested inside JSON decodes as a number of milliseconds,
    /// because that is the unit JSON storage keeps; both readings live in
    /// [`temporal::timestamp_micros`], which is the storage contract itself.
    fn decode_value(value: Value) -> Result<Self, DbError> {
        temporal::timestamp_micros(&value)
            .ok_or_else(|| {
                DbError::validation("invalid_model_value", "expected a database timestamp")
            })
            .and_then(Self::from_unix_micros)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instants_round_trip_their_microseconds_and_refuse_the_calendar_edge() {
        for micros in [
            temporal::MIN_TIMESTAMP_MICROS,
            -1,
            0,
            1,
            temporal::MAX_TIMESTAMP_MICROS,
        ] {
            let instant = UtcInstant::from_unix_micros(micros).unwrap();
            assert_eq!(instant.unix_micros(), micros);
            assert_eq!(
                <UtcInstant as EncodeValue<sql_types::Timestamp>>::encode_value(instant).unwrap(),
                Value::TimestampMicros(micros)
            );
            assert_eq!(
                <UtcInstant as DecodeValue<sql_types::Timestamp>>::decode_value(
                    Value::TimestampMicros(micros)
                )
                .unwrap(),
                instant
            );
        }
        // Rejection control: one microsecond past either end, and a value that
        // is not a timestamp at all.
        for micros in [
            i64::MIN,
            temporal::MIN_TIMESTAMP_MICROS - 1,
            temporal::MAX_TIMESTAMP_MICROS + 1,
            i64::MAX,
        ] {
            assert!(UtcInstant::from_unix_micros(micros).is_err(), "{micros}");
            assert!(
                <UtcInstant as DecodeValue<sql_types::Timestamp>>::decode_value(
                    Value::TimestampMicros(micros)
                )
                .is_err(),
                "{micros}"
            );
        }
        for value in [Value::Null, Value::from("0"), Value::Bool(true)] {
            assert!(
                <UtcInstant as DecodeValue<sql_types::Timestamp>>::decode_value(value.clone())
                    .is_err(),
                "{value:?}"
            );
        }
    }

    /// A temporal field nested inside JSON is stored as a number of
    /// milliseconds, so decoding one scales it. The native microsecond form
    /// does not scale. Control: the two forms of the same instant agree.
    #[test]
    fn nested_json_numbers_decode_as_milliseconds() {
        let decode = |value| <UtcInstant as DecodeValue<sql_types::Timestamp>>::decode_value(value);
        assert_eq!(decode(Value::from(5)).unwrap().unix_micros(), 5_000);
        assert_eq!(decode(Value::from(-1)).unwrap().unix_micros(), -1_000);
        assert_eq!(decode(Value::TimestampMicros(5)).unwrap().unix_micros(), 5);
        assert_eq!(
            decode(Value::from(5)).unwrap(),
            decode(Value::TimestampMicros(5_000)).unwrap()
        );
    }

    /// Milliseconds enter through their own constructor, so the unit is named
    /// at the call site rather than inferred from the integer.
    #[test]
    fn millisecond_construction_scales_and_flooring_rounds_toward_negative_infinity() {
        assert_eq!(
            UtcInstant::from_unix_millis(5).unwrap().unix_micros(),
            5_000
        );
        assert_eq!(
            UtcInstant::from_unix_millis(-1).unwrap().unix_micros(),
            -1_000
        );
        for (micros, millis) in [(-1, -1), (-1_000, -1), (-1_001, -2), (1_999, 1), (0, 0)] {
            assert_eq!(
                UtcInstant::from_unix_micros(micros).unwrap().floor_unix_millis(),
                millis,
                "{micros}"
            );
        }
        // Rejection control: a millisecond value outside the calendar, and one
        // whose scaling would overflow before the range check.
        for millis in [
            temporal::MIN_TIMESTAMP_MILLIS - 1,
            temporal::MAX_TIMESTAMP_MILLIS + 1,
            i64::MIN,
            i64::MAX,
        ] {
            assert!(UtcInstant::from_unix_millis(millis).is_err(), "{millis}");
        }
    }
}
