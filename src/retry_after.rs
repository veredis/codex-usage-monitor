//! Convert server Retry-After hints into bounded timer delays. Adapted from
//! upstream retry_after.rs (PR #8); the HTTP-date parser is local so no date
//! crate is needed, and a zero hint is treated as no hint because the Claude
//! usage endpoint has been observed returning `retry-after: 0` while limiting.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Delay in milliseconds requested by a Retry-After value (delta-seconds or
/// IMF-fixdate), at least one second. `None` for absent, zero, past or invalid hints.
pub fn delay_ms(value: &str, now: SystemTime) -> Option<u32> {
    let value = value.trim();
    let millis = if !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit()) {
        value.parse::<u64>().ok()?.saturating_mul(1000)
    } else {
        let at = parse_http_date(value)?;
        at.duration_since(now)
            .ok()?
            .as_millis()
            .min(u64::MAX as u128) as u64
    };
    if millis == 0 {
        return None;
    }
    Some(millis.clamp(1000, u32::MAX as u64) as u32)
}

/// Parse `Wed, 21 Oct 2015 07:28:00 GMT`.
fn parse_http_date(value: &str) -> Option<SystemTime> {
    let (_weekday, rest) = value.split_once(", ")?;
    let parts: Vec<&str> = rest.split(' ').collect();
    let [day, month, year, time, "GMT"] = parts.as_slice() else {
        return None;
    };
    let day: u64 = day.parse().ok()?;
    let month = MONTHS.iter().position(|name| name == month)? as u64 + 1;
    let year: u64 = year.parse().ok()?;
    let time: Vec<u64> = time
        .split(':')
        .map(|part| part.parse().ok())
        .collect::<Option<_>>()?;
    let [hour, minute, second] = time.as_slice() else {
        return None;
    };
    if !(1..=31).contains(&day) || year < 1970 || *hour > 23 || *minute > 59 || *second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(UNIX_EPOCH + Duration::from_secs(days * 86_400 + hour * 3600 + minute * 60 + second))
}

fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let leap = |y: u64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let month_days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut days: u64 = (1970..year).map(|y| if leap(y) { 366 } else { 365 }).sum();
    days += month_days[..(month - 1) as usize].iter().sum::<u64>();
    if month > 2 && leap(year) {
        days += 1;
    }
    days + day - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seconds_dates_and_rejects_invalid_or_zero_hints() {
        let date = parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        assert_eq!(
            date.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_445_412_480
        );
        assert_eq!(delay_ms(" 120 ", UNIX_EPOCH), Some(120_000));
        assert_eq!(
            delay_ms(
                "Wed, 21 Oct 2015 07:28:00 GMT",
                date - Duration::from_secs(90)
            ),
            Some(90_000)
        );
        assert_eq!(delay_ms("Wed, 21 Oct 2015 07:28:00 GMT", date), None);
        assert_eq!(delay_ms("0", date), None);
        assert_eq!(delay_ms("18446744073709551615", date), Some(u32::MAX));
        for invalid in [
            "",
            "-1",
            "1.5",
            "tomorrow",
            "999999999999999999999999999",
            "Wed, 21 Foo 2015 07:28:00 GMT",
            "Wed, 32 Oct 2015 07:28:00 GMT",
            "Wed, 21 Oct 2015 25:28:00 GMT",
            "Wed, 21 Oct 2015 07:28:00 UTC",
        ] {
            assert_eq!(delay_ms(invalid, date), None, "{invalid}");
        }
    }

    #[test]
    fn leap_years_are_counted() {
        let date = parse_http_date("Thu, 29 Feb 2024 00:00:00 GMT").unwrap();
        assert_eq!(
            date.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_709_164_800
        );
    }
}
