/// Parse ISO timestamps emitted by wt and a bounded set of explicitly-zoned
/// legacy timestamps into Unix milliseconds. Zone-less forms are treated as
/// UTC for deterministic cross-host behavior; locale-dependent local times
/// are rejected. Fractional seconds are truncated to milliseconds.
pub fn parse_iso_millis(input: &str) -> Option<i64> {
    parse_iso_form(input).or_else(|| parse_explicit_zone_legacy(input))
}

fn parse_iso_form(input: &str) -> Option<i64> {
    let bytes = input.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = number(&bytes[0..4])? as i64;
    let month = number(&bytes[5..7])? as i64;
    let day = number(&bytes[8..10])? as i64;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if bytes.len() == 10 {
        return Some(days * 86_400_000);
    }
    if !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes.len() < 19
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let hour = number(&bytes[11..13])? as i64;
    let minute = number(&bytes[14..16])? as i64;
    let second = number(&bytes[17..19])? as i64;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut pos = 19;
    let mut millis = 0;
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        let start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        if start == pos {
            return None;
        }
        let digits = &bytes[start..pos];
        for i in 0..3 {
            millis *= 10;
            if let Some(digit) = digits.get(i) {
                millis += (digit - b'0') as i64;
            }
        }
    }
    let offset_minutes = match bytes.get(pos) {
        None => 0,
        Some(b'Z' | b'z') if pos + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) if pos + 6 == bytes.len() && bytes[pos + 3] == b':' => {
            let h = number(&bytes[pos + 1..pos + 3])? as i64;
            let m = number(&bytes[pos + 4..pos + 6])? as i64;
            if h > 23 || m > 59 {
                return None;
            }
            let value = h * 60 + m;
            if *sign == b'+' { value } else { -value }
        }
        _ => return None,
    };
    Some(
        days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000 + millis
            - offset_minutes * 60_000,
    )
}

/// Accept the common explicit-zone date strings that JavaScript's Date.parse
/// accepts in pre-existing records, without inheriting machine locale or TZ.
fn parse_explicit_zone_legacy(input: &str) -> Option<i64> {
    let tokens = input.split_whitespace().collect::<Vec<_>>();
    let tokens = if tokens.len() == 6 && tokens[0].ends_with(',') {
        &tokens[1..]
    } else {
        &tokens[..]
    };
    if tokens.len() != 5 {
        return None;
    }

    let (month, day) = if let Some(month) = month_number(tokens[0]) {
        (month, tokens[1].trim_end_matches(','))
    } else {
        (month_number(tokens[1])?, tokens[0].trim_end_matches(','))
    };
    let day = parse_decimal(day)? as i64;
    let year = parse_decimal(tokens[2])? as i64;
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let (hour, minute, second, millis) = parse_legacy_clock(tokens[3])?;
    let offset_minutes = parse_explicit_zone(tokens[4])?;
    let days = days_from_civil(year, month, day);
    Some(
        days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000 + millis
            - offset_minutes * 60_000,
    )
}

fn month_number(month: &str) -> Option<i64> {
    let month = month.trim_end_matches(',');
    [
        ("jan", "january"),
        ("feb", "february"),
        ("mar", "march"),
        ("apr", "april"),
        ("may", "may"),
        ("jun", "june"),
        ("jul", "july"),
        ("aug", "august"),
        ("sep", "september"),
        ("oct", "october"),
        ("nov", "november"),
        ("dec", "december"),
    ]
    .iter()
    .position(|(short, long)| month.eq_ignore_ascii_case(short) || month.eq_ignore_ascii_case(long))
    .map(|i| i as i64 + 1)
}

fn parse_decimal(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn parse_legacy_clock(value: &str) -> Option<(i64, i64, i64, i64)> {
    let mut parts = value.split(':');
    let hour = parse_decimal(parts.next()?)? as i64;
    let minute = parse_decimal(parts.next()?)? as i64;
    let seconds = parts.next().unwrap_or("0");
    if parts.next().is_some() {
        return None;
    }
    let (seconds, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let second = parse_decimal(seconds)? as i64;
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut millis = 0;
    for i in 0..3 {
        millis *= 10;
        if let Some(digit) = fraction.as_bytes().get(i) {
            millis += (digit - b'0') as i64;
        }
    }
    (hour <= 23 && minute <= 59 && second <= 59).then_some((hour, minute, second, millis))
}

fn parse_explicit_zone(zone: &str) -> Option<i64> {
    if zone.eq_ignore_ascii_case("utc")
        || zone.eq_ignore_ascii_case("gmt")
        || zone.eq_ignore_ascii_case("z")
    {
        return Some(0);
    }
    let bytes = zone.as_bytes();
    if !(bytes.len() == 5 || (bytes.len() == 6 && bytes[3] == b':'))
        || !matches!(bytes[0], b'+' | b'-')
    {
        return None;
    }
    let (hours, minutes) = if bytes.len() == 5 {
        (number(&bytes[1..3])?, number(&bytes[3..5])?)
    } else {
        (number(&bytes[1..3])?, number(&bytes[4..6])?)
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    let offset = (hours as i64) * 60 + minutes as i64;
    Some(if bytes[0] == b'-' { -offset } else { offset })
}

fn number(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    bytes.iter().try_fold(0u32, |n, digit| {
        n.checked_mul(10)?.checked_add((digit - b'0') as u32)
    })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Days from 1970-01-01 (Howard Hinnant's proleptic Gregorian conversion).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * adjusted_month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dates_offsets_and_millisecond_fractions() {
        assert_eq!(parse_iso_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_millis("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(
            parse_iso_millis("2026-08-20T12:00:00.125Z"),
            Some(1_787_227_200_125)
        );
        assert_eq!(parse_iso_millis("1970-01-02"), Some(86_400_000));
    }

    #[test]
    fn rejects_invalid_calendar_and_clock_values() {
        assert_eq!(parse_iso_millis("2026-02-30"), None);
        assert_eq!(parse_iso_millis("2026-01-01T25:00:00Z"), None);
        assert_eq!(parse_iso_millis("not a date"), None);
    }

    #[test]
    fn parses_only_explicitly_zoned_legacy_forms() {
        let october = parse_iso_millis("2026-10-08T12:00:00Z");
        assert_eq!(parse_iso_millis("October 8, 2026 12:00 UTC"), october);
        assert_eq!(parse_iso_millis("Thu, 8 Oct 2026 12:00:00 +0000"), october);
        assert_eq!(parse_iso_millis("8 October 2026 15:00:00 +0300"), october);
        assert_eq!(parse_iso_millis("October 8, 2026 12:00"), None);
        assert_eq!(parse_iso_millis("October 8, 2026 12:00 PST"), None);
        assert_eq!(parse_iso_millis("February 29, 2025 12:00 UTC"), None);
    }
}
