use crate::i18n::Tr;
use jiff::{Timestamp, tz};

/// Parse a Git-style `+HHMM` offset.
fn offset(tz: &[u8]) -> Option<tz::Offset> {
    let text = std::str::from_utf8(tz).ok()?;
    let (sign, digits) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    if digits.len() != 4 {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    tz::Offset::from_seconds(sign * (hours * 3600 + minutes * 60)).ok()
}
/// The time in the recorded zone, or in `preferred` when one is chosen.
pub fn absolute(seconds: i64, recorded: &[u8], preferred: Option<&str>) -> String {
    let Ok(at) = Timestamp::from_second(seconds) else {
        return seconds.to_string();
    };
    let zone = preferred
        .and_then(|name| tz::TimeZone::get(name).ok())
        .or_else(|| offset(recorded).map(tz::TimeZone::fixed))
        .unwrap_or(tz::TimeZone::UTC);
    at.to_zoned(zone).strftime("%Y-%m-%d %H:%M %z").to_string()
}
pub fn iso(seconds: i64) -> String {
    Timestamp::from_second(seconds)
        .map(|t| t.to_string())
        .unwrap_or_default()
}
pub fn relative(seconds: i64, now: i64, t: Tr) -> String {
    let delta = (now - seconds).max(0);
    let (value, unit) = match delta {
        0..60 => return t.s("just now").to_owned(),
        60..3600 => (delta / 60, "{n} minutes ago"),
        3600..86_400 => (delta / 3600, "{n} hours ago"),
        86_400..2_592_000 => (delta / 86_400, "{n} days ago"),
        2_592_000..31_536_000 => (delta / 2_592_000, "{n} months ago"),
        _ => (delta / 31_536_000, "{n} years ago"),
    };
    t.s(unit).replace("{n}", &value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recorded_offsets_are_honored() {
        assert_eq!(absolute(0, b"+0900", None), "1970-01-01 09:00 +0900");
        assert_eq!(absolute(0, b"-0130", None), "1969-12-31 22:30 -0130");
        assert_eq!(absolute(0, b"bogus", None), "1970-01-01 00:00 +0000");
        assert_eq!(absolute(0, b"+0900", Some("UTC")), "1970-01-01 00:00 +0000");
    }
}
