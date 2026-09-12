//! Portable temporal values shared by native codecs and schema validation.

use time::{Date, Month, OffsetDateTime};

const UNIX_EPOCH_DAY: i32 = OffsetDateTime::UNIX_EPOCH.date().to_julian_day();

pub const MIN_TIMESTAMP_MILLIS: i64 = -62_135_596_800_000;
pub const MAX_TIMESTAMP_MILLIS: i64 = 253_402_300_799_999;

/// Instants whose UTC date fits the portable positive `YYYY-MM-DD` calendar.
pub fn is_timestamp_millis(value: i64) -> bool {
    (MIN_TIMESTAMP_MILLIS..=MAX_TIMESTAMP_MILLIS).contains(&value)
}

/// Decode a native timestamp, integral Unix milliseconds, or an ISO timestamp.
pub fn timestamp_millis(value: &crate::value::Value) -> Option<i64> {
    match value {
        crate::value::Value::String(text) => parse_timestamp_millis(text),
        _ => value
            .as_i64()
            .filter(|value| is_timestamp_millis(*value))
            .or_else(|| {
                // Floating-point inputs can still represent integral milliseconds.
                // The portable domain is exactly representable by both number types.
                let number = value.as_f64()?;
                (number.fract() == 0.0
                    && number >= MIN_TIMESTAMP_MILLIS as f64
                    && number <= MAX_TIMESTAMP_MILLIS as f64)
                    .then_some(number as i64)
            }),
    }
}

/// Parse a real calendar date and optional time. An omitted zone means UTC.
/// Fractional seconds are floored to the containing millisecond, including
/// instants before the Unix epoch.
pub fn parse_timestamp_millis(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    let days = i64::from(parse_calendar_date(value.get(..10)?)?);
    if bytes.len() == 10 {
        return Some(days * 86_400_000);
    }
    if bytes.len() < 19
        || !matches!(bytes[10], b'T' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let hour = digits(&bytes[11..13])?;
    let minute = digits(&bytes[14..16])?;
    let second = digits(&bytes[17..19])?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut index = 19;
    let mut millis = 0;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        let mut scale = 100;
        while let Some(digit) = bytes.get(index).filter(|digit| digit.is_ascii_digit()) {
            millis += i64::from(digit - b'0') * scale;
            scale /= 10;
            index += 1;
        }
        if index == start {
            return None;
        }
    }
    let offset = match &bytes[index..] {
        b"" | b"Z" | b"z" => 0,
        [sign @ (b'+' | b'-'), rest @ ..] => {
            let (hours, minutes) = match rest {
                [h1, h2] => (digits(&[*h1, *h2])?, 0),
                [h1, h2, m1, m2] | [h1, h2, b':', m1, m2] => {
                    (digits(&[*h1, *h2])?, digits(&[*m1, *m2])?)
                }
                _ => return None,
            };
            if hours > 23 || minutes > 59 {
                return None;
            }
            i64::from(hours * 60 + minutes) * if *sign == b'-' { -1 } else { 1 }
        }
        _ => return None,
    };
    let result = days * 86_400_000 + i64::from(hour * 3600 + minute * 60 + second) * 1000 + millis
        - offset * 60_000;
    is_timestamp_millis(result).then_some(result)
}

/// Canonical UTC storage text; lexical order matches instant order.
pub fn format_timestamp_millis(millis: i64) -> Option<String> {
    if !is_timestamp_millis(millis) {
        return None;
    }
    let date = OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000).ok()?;
    Some(format!(
        "{}T{:02}:{:02}:{:02}.{:03}Z",
        date.date(),
        date.hour(),
        date.minute(),
        date.second(),
        date.millisecond()
    ))
}

/// Parse a positive Gregorian year in the `YYYY-MM-DD` form into Unix days.
pub fn parse_calendar_date(value: &str) -> Option<i32> {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = digits(&bytes[..4])?;
    if year == 0 {
        return None;
    }
    let month = Month::try_from(u8::try_from(digits(&bytes[5..7])?).ok()?).ok()?;
    let day = u8::try_from(digits(&bytes[8..])?).ok()?;
    let date = Date::from_calendar_date(year, month, day).ok()?;
    Some(date.to_julian_day() - UNIX_EPOCH_DAY)
}

/// Format a Unix day as a portable `YYYY-MM-DD` date.
pub fn format_calendar_date(unix_days: i32) -> Option<String> {
    let date = Date::from_julian_day(UNIX_EPOCH_DAY.checked_add(unix_days)?).ok()?;
    if !(1..=9999).contains(&date.year()) {
        return None;
    }
    Some(date.to_string())
}

fn digits(bytes: &[u8]) -> Option<i32> {
    bytes.iter().try_fold(0, |value, digit| {
        digit
            .is_ascii_digit()
            .then(|| value * 10 + i32::from(digit - b'0'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_forms_preserve_the_same_instant() {
        for (text, millis) in [
            ("0001-01-01", MIN_TIMESTAMP_MILLIS),
            ("9999-12-31T23:59:59.999Z", MAX_TIMESTAMP_MILLIS),
            ("1970-01-01", 0),
            ("1970-01-01 00:00:00", 0),
            ("1969-12-31T23:59:59.999999999Z", -1),
            ("1970-01-01T02:30:00+02:30", 0),
            ("1970-01-01T02:30:00+0230", 0),
            ("1969-12-31T22:00:00-02", 0),
            ("1970-01-01T00:00:00.1z", 100),
            ("2000-02-29T00:00:00Z", 951_782_400_000),
        ] {
            assert_eq!(parse_timestamp_millis(text), Some(millis), "{text}");
            assert_eq!(
                parse_timestamp_millis(&format_timestamp_millis(millis).unwrap()),
                Some(millis)
            );
        }
        assert_eq!(
            format_timestamp_millis(-1).as_deref(),
            Some("1969-12-31T23:59:59.999Z")
        );
    }

    #[test]
    fn timestamps_reject_invalid_calendar_time_zone_and_range() {
        for text in [
            "0000-01-01",
            "1900-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:60:00Z",
            "2026-01-01T00:00:60Z",
            "2026-01-01T+1:00:00Z",
            "2026-01-01T00:00:00+01:-1",
            "2026-01-01T00:00:00+24:00",
            "2026-01-01T00:00:00+00:60",
            "2026-01-01T00:00:00.Z",
            "2026-01-01T00:00:00Z ",
            "0001-01-01T00:00:00+01:00",
            "9999-12-31T23:59:59-01:00",
            "🐈-01-01T00:00:00Z",
            "2026",
            "2026-01-01T00:00",
        ] {
            assert_eq!(parse_timestamp_millis(text), None, "{text}");
        }
        for millis in [
            i64::MIN,
            MIN_TIMESTAMP_MILLIS - 1,
            MAX_TIMESTAMP_MILLIS + 1,
            i64::MAX,
        ] {
            assert!(!is_timestamp_millis(millis));
            assert_eq!(format_timestamp_millis(millis), None);
        }
    }

    #[test]
    fn calendar_dates_match_independent_epoch_and_leap_day_references() {
        for (date, days) in [
            ("0001-01-01", -719162),
            ("1969-12-31", -1),
            ("1970-01-01", 0),
            ("2000-01-01", 10957),
            ("2000-02-29", 11016),
            ("9999-12-31", 2932896),
        ] {
            assert_eq!(parse_calendar_date(date), Some(days));
            assert_eq!(format_calendar_date(days).as_deref(), Some(date));
        }
        for date in ["0004-02-29", "0099-12-31", "0100-03-01", "2400-02-29"] {
            assert_eq!(
                format_calendar_date(parse_calendar_date(date).unwrap()).as_deref(),
                Some(date)
            );
        }
    }

    #[test]
    fn invalid_or_unrepresentable_calendar_dates_are_refused() {
        for date in [
            "0000-01-01",
            "1900-02-29",
            "2026-02-30",
            "2026-04-31",
            "2026-13-01",
            "2026-00-01",
            "2026-01-00",
            "2026-01-32",
            "+001-01-01",
            "2026-+1-01",
            "10000-01-01",
            "2026-01-01T00:00:00Z",
            "🐈-01-01",
            "2026-01-01 ",
        ] {
            assert_eq!(parse_calendar_date(date), None, "{date}");
        }
        for days in [i32::MIN, i32::MAX, -719163, 2932897] {
            assert_eq!(format_calendar_date(days), None);
        }
    }
}
