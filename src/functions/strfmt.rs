//! kube-openapi's `strfmt` date and date-time handling, as the apiserver uses
//! it: `IsDate` / `IsDateTime` check a `format: date` / `format: date-time`
//! value (OpenAPI validation, and the `format.date()` / `format.datetime()`
//! library), and `ParseDateTime` binds a `format: date-time` value for CEL.
//! kube-openapi v0.0.0-20260317180543-43fb72c5454a, the version the reference
//! apiserver requires.

/// A non-empty run of ASCII digits. Go's `time.Parse` (and strfmt's RFC3339
/// regex `[0-9]{2}`) accept only digits in numeric fields, but Rust's
/// `parse::<u32>` also accepts a leading `+` (e.g. `"+024"` → `24`); this guard
/// closes that gap so date/time components match the apiserver.
fn all_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// kube-openapi's `strfmt.IsDate`: Go's `time.Parse("2006-01-02", s)` succeeds.
pub(crate) fn is_date(s: &str) -> bool {
    date_parts(s).is_some()
}

/// The year, month and day of a date Go's `time.Parse("2006-01-02", s)`
/// takes: `YYYY-MM-DD`, ASCII digits only at fixed width (no sign, space or
/// short field), and a day that exists in that month (year 0000 included).
pub(crate) fn date_parts(s: &str) -> Option<(u32, u32, u32)> {
    let bytes = s.as_bytes();
    if s.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let (year_str, month_str, day_str) = (&s[0..4], &s[5..7], &s[8..10]);
    if !all_ascii_digits(year_str) || !all_ascii_digits(month_str) || !all_ascii_digits(day_str) {
        return None;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        year_str.parse::<u32>(),
        month_str.parse::<u32>(),
        day_str.parse::<u32>(),
    ) else {
        return None;
    };
    ((1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day))
        .then_some((year, month, day))
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn is_leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

/// kube-openapi's `strfmt.IsDateTime`, the apiserver's OpenAPI check of a
/// `format: date-time` value (see `named_format::validate_datetime` for what it
/// takes).
pub(crate) fn is_date_time(s: &str) -> bool {
    // Lowercasing only matters for `T` and `Z`: no other character lowercases
    // to `t`, `z`, a digit, `:`, `+`, `-` or a newline, and Go's simple case
    // mapping keeps one character per character.
    let mut pieces = s.split(['T', 't']);
    let date = pieces.next().unwrap_or_default();
    let Some(time) = pieces.next() else {
        return false;
    };
    is_date(date) && is_strfmt_time(time)
}

/// The time piece of [`is_date_time`].
fn is_strfmt_time(t: &str) -> bool {
    let b = t.as_bytes();
    let two_digits = |i: usize| b.get(i..i + 2).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    if !(two_digits(0)
        && b.get(2) == Some(&b':')
        && two_digits(3)
        && b.get(5) == Some(&b':')
        && two_digits(6))
    {
        return false;
    }
    // Two ASCII digits compare as strings the way they do as numbers.
    if &t[0..2] > "23" || &t[3..5] > "59" || &t[6..8] > "59" {
        return false;
    }
    let rest = &t[8..];
    // The zone ends the string: `z` (or `Z`), else `[+-]dd:dd`.
    let fraction = match rest.strip_suffix(['z', 'Z']) {
        Some(fraction) => fraction,
        None => {
            let Some(at) = rest.len().checked_sub(6) else {
                return false;
            };
            let zone = &rest.as_bytes()[at..];
            let offset = matches!(zone[0], b'+' | b'-')
                && zone[1].is_ascii_digit()
                && zone[2].is_ascii_digit()
                && zone[3] == b':'
                && zone[4].is_ascii_digit()
                && zone[5].is_ascii_digit();
            if !offset {
                return false;
            }
            // The zone is ASCII, so `at` is a character boundary.
            &rest[..at]
        }
    };
    // `(.[0-9]+)?`: nothing, or any character but a newline and then digits.
    let mut chars = fraction.chars();
    match chars.next() {
        None => true,
        Some('\n') => false,
        Some(_) => {
            let digits = chars.as_str();
            !digits.is_empty() && digits.bytes().all(|d| d.is_ascii_digit())
        }
    }
}

/// Why [`parse_date_time`] gives no timestamp.
#[cfg(feature = "validation")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DateTimeRefusal {
    /// `strfmt.ParseDateTime` fails: the apiserver binds an error.
    Unparsed,
    /// It parses, but with a zone offset of 24 hours or more (Go takes up to
    /// `+24:60`), which `chrono` cannot hold.
    OffsetOutOfRange,
}

/// kube-openapi's `strfmt.ParseDateTime`, which the apiserver binds a
/// `format: date-time` value with: Go's `time.Parse` with each of
/// `RFC3339Micro`, `RFC3339Millis`, `time.RFC3339`, `time.RFC3339Nano` and
/// `ISO8601LocalTime` (`2006-01-02T15:04:05`) in turn, the first that parses
/// winning. The RFC 3339 layouts accept the same strings with the same
/// result as `time.RFC3339` (a `.` or `,` fraction of any length, nine digits
/// kept), and `ISO8601LocalTime` the same with no zone, read as UTC; the empty
/// string is the Unix epoch (`NewDateTime()`).
#[cfg(feature = "validation")]
pub(crate) fn parse_date_time(s: &str) -> Result<chrono::DateTime<chrono::FixedOffset>, DateTimeRefusal> {
    let t = if s.is_empty() {
        super::cel_time::GoTime {
            unix: 0,
            nanos: 0,
            offset: 0,
        }
    } else {
        super::cel_time::parse_rfc3339(s.as_bytes(), true).ok_or(DateTimeRefusal::Unparsed)?
    };
    let offset = chrono::FixedOffset::east_opt(t.offset).ok_or(DateTimeRefusal::OffsetOutOfRange)?;
    let utc = chrono::DateTime::from_timestamp(t.unix, t.nanos).ok_or(DateTimeRefusal::Unparsed)?;
    Ok(utc.with_timezone(&offset))
}
