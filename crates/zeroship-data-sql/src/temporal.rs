//! Calendar-date values shared by native codecs and schema validation.

use time::{Date, Month, OffsetDateTime};

const UNIX_EPOCH_DAY: i32 = OffsetDateTime::UNIX_EPOCH.date().to_julian_day();

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
