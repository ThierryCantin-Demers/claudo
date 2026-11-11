//! How a window's allowance is meant to be spent: evenly over every day, or over workdays
//! alone, for someone who rarely works on weekends.

use std::io;
use std::path::PathBuf;

use claudo_protocol::{Calendar, QuotaWindow, Standing};
use jiff::civil::Weekday;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pace {
    #[default]
    EveryDay,
    Workdays,
}

#[cfg_attr(not(feature = "cosmic"), allow(dead_code))]
impl Pace {
    /// The applets showing one tool, whose files start with `files`, share it.
    pub fn load(files: &str) -> Self {
        match std::fs::read_to_string(path(files)) {
            Ok(text) if text.trim() == "workdays" => Pace::Workdays,
            _ => Pace::EveryDay,
        }
    }

    pub fn save(self, files: &str) -> io::Result<()> {
        let path = path(files);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = match self {
            Pace::EveryDay => "every-day",
            Pace::Workdays => "workdays",
        };
        std::fs::write(path, text)
    }

    pub fn standing(self, window: QuotaWindow, length: u64, now: u64) -> Standing {
        match self {
            // A window shorter than a day has no weekend to leave out.
            Pace::Workdays if length >= 86_400 => {
                window.standing_by(length, now, &Workdays(TimeZone::system()))
            }
            _ => window.standing(length, now),
        }
    }
}

fn path(files: &str) -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(std::env::temp_dir);
    config.join("claudo").join(format!("{files}-pace"))
}

/// Monday to Friday, by the days of a time zone. A workday's share is there from its start:
/// spending it in a few working hours is on pace, though the day has hours left.
struct Workdays(TimeZone);

/// Days enough for any window, should a time zone go wrong.
const MOST_DAYS: usize = 400;

impl Workdays {
    /// Each day from the one `from` falls in on: its start and end, and whether it counts.
    fn days(&self, from: u64) -> impl Iterator<Item = (u64, u64, bool)> {
        let first = Timestamp::from_second(from as i64)
            .ok()
            .and_then(|at| at.to_zoned(self.0.clone()).start_of_day().ok());
        std::iter::successors(first, next_day)
            .filter_map(|day| {
                let end = next_day(&day)?;
                let work = !matches!(day.weekday(), Weekday::Saturday | Weekday::Sunday);
                Some((seconds(&day), seconds(&end), work))
            })
            .take(MOST_DAYS)
    }
}

fn next_day(day: &Zoned) -> Option<Zoned> {
    day.tomorrow().ok()?.start_of_day().ok()
}

fn seconds(day: &Zoned) -> u64 {
    day.timestamp().as_second().max(0) as u64
}

impl Calendar for Workdays {
    fn counted(&self, from: u64, to: u64) -> u64 {
        self.days(from)
            .take_while(|&(start, _, _)| start < to)
            .filter(|&(_, _, work)| work)
            .map(|(start, end, _)| end.min(to).saturating_sub(start.max(from)))
            .sum()
    }

    /// Every workday begun by `now`, whole, the one the window starts in included.
    fn allowed(&self, from: u64, now: u64) -> u64 {
        self.days(from)
            .take_while(|&(start, _, _)| start <= now)
            .filter(|&(_, _, work)| work)
            .map(|(start, end, _)| end - start)
            .sum()
    }

    fn allowed_until(&self, now: u64) -> u64 {
        match self.days(now).next() {
            Some((_, end, true)) => end,
            _ => now,
        }
    }

    fn after(&self, from: u64, seconds: u64) -> u64 {
        let mut left = seconds;
        for (start, end, work) in self.days(from) {
            if !work {
                continue;
            }
            let begin = start.max(from);
            if left <= end - begin {
                return begin + left;
            }
            left -= end - begin;
        }
        from.saturating_add(seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    fn at(time: &str) -> u64 {
        time.parse::<Timestamp>().unwrap().as_second() as u64
    }

    #[test]
    fn weekends_do_not_count() {
        let utc = Workdays(TimeZone::UTC);
        let saturday = at("2026-11-07T00:00:00Z");
        assert_eq!(utc.counted(saturday, saturday + 7 * DAY), 5 * DAY);
        assert_eq!(utc.counted(saturday, saturday + 2 * DAY), 0);
        let friday_noon = at("2026-11-13T12:00:00Z");
        assert_eq!(
            utc.after(friday_noon, DAY),
            at("2026-11-16T12:00:00Z"),
            "half of Friday, then half of Monday"
        );
    }

    #[test]
    fn a_week_spent_on_workdays_is_paced_by_them() {
        let saturday = at("2026-11-07T00:00:00Z");
        let window = QuotaWindow {
            used: 0.25,
            resets_at: saturday + 7 * DAY,
        };
        let monday_noon = at("2026-11-09T12:00:00Z");
        let workdays = window.standing_by(7 * DAY, monday_noon, &Workdays(TimeZone::UTC));
        assert_eq!(
            workdays.even,
            Some(0.2),
            "Monday's whole share, one workday of five"
        );
        assert_eq!(
            workdays.out_in,
            Some(DAY / 2 + 3 * DAY),
            "a quarter on Monday: the rest by the end of Thursday"
        );
        let every_day = window.standing(7 * DAY, monday_noon);
        assert!(every_day.even.unwrap() > 0.35);
        assert_eq!(every_day.out_in, None);
    }

    #[test]
    fn a_window_begun_this_afternoon_allows_todays_whole_share() {
        let tuesday_afternoon = at("2026-11-10T13:00:00Z");
        let window = QuotaWindow {
            used: 0.17,
            resets_at: tuesday_afternoon + 7 * DAY,
        };
        let evening = at("2026-11-10T20:00:00Z");
        let workdays = window.standing_by(7 * DAY, evening, &Workdays(TimeZone::UTC));
        assert_eq!(workdays.even, Some(0.2));
        assert_eq!(workdays.out_in, None, "a day's share a day lasts the week");
        let next_monday = at("2026-11-16T09:00:00Z");
        let late = QuotaWindow {
            used: 0.5,
            ..window
        }
        .standing_by(7 * DAY, next_monday, &Workdays(TimeZone::UTC));
        assert_eq!(late.even, Some(1.0), "Tuesday to Monday: all five shares");
    }
}
