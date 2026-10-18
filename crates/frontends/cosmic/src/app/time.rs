//! Times and durations as the transcript shows them.

use claudo_protocol::TurnTime;
use jiff::{Timestamp, Zoned, tz::TimeZone};

/// How long ago, in the one unit that reads at a glance.
pub(super) fn ago(seconds: u64) -> String {
    match (seconds / 86_400, seconds / 3600, seconds / 60) {
        (0, 0, 0) => "just now".to_string(),
        (0, 0, minutes) => format!("{minutes} min ago"),
        (0, hours, _) => format!("{hours} h ago"),
        (1, _, _) => "yesterday".to_string(),
        (days, _, _) => format!("{days} days ago"),
    }
}

/// How long a finished call ran: tenths under ten seconds, where most calls end.
pub(super) fn lasted(millis: u64) -> String {
    match millis {
        0..10_000 => format!("{:.1} s", millis as f64 / 1000.0),
        _ => took(millis / 1000),
    }
}

pub(super) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub(super) fn took(seconds: u64) -> String {
    match (seconds / 3600, seconds % 3600 / 60, seconds % 60) {
        (0, 0, seconds) => format!("{seconds} s"),
        (0, minutes, seconds) => format!("{minutes} min {seconds} s"),
        (hours, minutes, _) => format!("{hours} h {minutes} min"),
    }
}

/// When a turn started, in local time, and how long it took.
pub(super) fn turn_label(time: TurnTime) -> String {
    let took = took(time.ended.saturating_sub(time.started));
    let Ok(started) = Timestamp::from_second(time.started as i64) else {
        return format!("Took {took}");
    };
    let started = started.to_zoned(TimeZone::system());
    let today = Zoned::now().date();
    let when = if started.date() == today {
        started.strftime("%H:%M").to_string()
    } else if today
        .yesterday()
        .is_ok_and(|yesterday| started.date() == yesterday)
    {
        started.strftime("Yesterday %H:%M").to_string()
    } else {
        started.strftime("%b %-d, %H:%M").to_string()
    };
    format!("{when} · took {took}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_read_at_a_glance() {
        assert_eq!(ago(30), "just now");
        assert_eq!(ago(5 * 60), "5 min ago");
        assert_eq!(ago(3 * 3600 + 59), "3 h ago");
        assert_eq!(ago(30 * 3600), "yesterday");
        assert_eq!(ago(9 * 86_400), "9 days ago");
    }

    #[test]
    fn durations_keep_the_two_units_that_matter() {
        assert_eq!(took(42), "42 s");
        assert_eq!(took(130), "2 min 10 s");
        assert_eq!(lasted(420), "0.4 s");
        assert_eq!(lasted(130_000), "2 min 10 s");
        assert_eq!(took(3 * 3600 + 5 * 60 + 9), "3 h 5 min");
    }
}
