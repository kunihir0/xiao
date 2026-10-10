//! Independent-receipt comparison: the operator reads the time an event
//! reached an independent observer (for example the phone's Rust+ app) and
//! types it as `HH:MM:SS` UTC; the harness compares it with its own wall
//! clock. Only the pass/fail verdict reaches the manifest.

/// Maximum permitted difference between harness and human times.
pub const RECEIPT_WINDOW_SECONDS: u32 = 120;
const SECONDS_PER_DAY: u32 = 86_400;

/// Parses `HH:MM:SS` (24-hour, UTC) into seconds since midnight.
#[must_use]
pub fn parse_time_of_day(input: &str) -> Option<u32> {
    let mut parts = input.trim().split(':');
    let hours = field(parts.next()?)?;
    let minutes = field(parts.next()?)?;
    let seconds = field(parts.next()?)?;
    if parts.next().is_some() || hours > 23 || minutes > 59 || seconds > 59 {
        return None;
    }
    Some(hours * 3_600 + minutes * 60 + seconds)
}

fn field(part: &str) -> Option<u32> {
    if part.len() != 2 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

/// Whether two times of day differ by at most `window` seconds, treating
/// midnight as adjacent.
#[must_use]
pub fn within_window(recorded: u32, confirmed: u32, window: u32) -> bool {
    let difference = recorded.abs_diff(confirmed);
    difference.min(SECONDS_PER_DAY.saturating_sub(difference)) <= window
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_strict_time_of_day() {
        assert_eq!(parse_time_of_day("00:00:00"), Some(0));
        assert_eq!(parse_time_of_day(" 23:59:59\n"), Some(86_399));
        assert_eq!(parse_time_of_day("12:34:56"), Some(45_296));
        for bad in [
            "",
            "1:2:3",
            "24:00:00",
            "12:60:00",
            "12:00:60",
            "12:00",
            "12:00:00:00",
            "aa:bb:cc",
            "+1:00:00",
            "12-00-00",
        ] {
            assert_eq!(parse_time_of_day(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn window_is_inclusive_and_wraps_midnight() {
        assert!(within_window(1_000, 1_120, RECEIPT_WINDOW_SECONDS));
        assert!(within_window(1_120, 1_000, RECEIPT_WINDOW_SECONDS));
        assert!(!within_window(1_000, 1_121, RECEIPT_WINDOW_SECONDS));
        assert!(within_window(86_399, 30, RECEIPT_WINDOW_SECONDS));
        assert!(within_window(30, 86_399, RECEIPT_WINDOW_SECONDS));
        assert!(!within_window(86_399, 200, RECEIPT_WINDOW_SECONDS));
    }
}
