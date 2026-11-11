//! How a window's allowance is meant to be spent: evenly over every day, or over the
//! working hours of workdays alone, for someone who rarely works on weekends.

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
                let workdays = Workdays {
                    zone: TimeZone::system(),
                    started: day_started(),
                };
                window.standing_by(length, now, &workdays)
            }
            _ => window.standing(length, now),
        }
    }
}

/// Notes `now` as when the day's work started, unless the day has a start already. Before
/// 4am it is still the night of the day before.
#[cfg_attr(not(feature = "cosmic"), allow(dead_code))]
pub fn note_activity(now: u64) {
    if !begins_day(day_started(), now, &TimeZone::system()) {
        return;
    }
    let path = started_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(error) = std::fs::write(&path, now.to_string()) {
        eprintln!("claudo-applet: cannot note the day's start: {error}");
    }
}

fn begins_day(noted: Option<u64>, now: u64, zone: &TimeZone) -> bool {
    let local = |at: u64| {
        Timestamp::from_second(at as i64)
            .ok()
            .map(|at| at.to_zoned(zone.clone()))
    };
    let Some(now) = local(now) else {
        return false;
    };
    now.hour() >= 4
        && noted
            .and_then(local)
            .is_none_or(|noted| noted.date() != now.date())
}

/// When today's work started, shared by every applet.
fn day_started() -> Option<u64> {
    std::fs::read_to_string(started_path())
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn started_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    state.join("claudo").join("day-started")
}

fn path(files: &str) -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(std::env::temp_dir);
    config.join("claudo").join(format!("{files}-pace"))
}

/// Monday to Friday, by the days of a time zone. A workday's share is spent over the
/// `WORKDAY` from when work started that day, or, for a day with no start noted, from the
/// time of day it started on the day `started` is in.
struct Workdays {
    zone: TimeZone,
    started: Option<u64>,
}

const WORKDAY: u64 = 8 * 3600;

/// When work starts, from midnight, before any day's start is noted.
const USUAL_START: u64 = 9 * 3600;

/// Days enough for any window, should a time zone go wrong.
const MOST_DAYS: usize = 400;

impl Workdays {
    /// When work starts on the day from `start` to `end`.
    fn work_starts(&self, start: u64, end: u64) -> u64 {
        let Some(started) = self.started else {
            return start + USUAL_START;
        };
        if (start..end).contains(&started) {
            return started;
        }
        // The clock time it started at, which a change to or from summer time keeps.
        let zoned = |at: u64| {
            Timestamp::from_second(at as i64)
                .ok()
                .map(|at| at.to_zoned(self.zone.clone()))
        };
        let (Some(started), Some(day)) = (zoned(started), zoned(start)) else {
            return start + USUAL_START;
        };
        day.date()
            .to_datetime(started.time())
            .to_zoned(self.zone.clone())
            .map_or(start + USUAL_START, |at| seconds(&at))
    }

    /// Each day from the one `from` falls in on: its start and end, and whether it counts.
    fn days(&self, from: u64) -> impl Iterator<Item = (u64, u64, bool)> {
        let first = Timestamp::from_second(from as i64)
            .ok()
            .and_then(|at| at.to_zoned(self.zone.clone()).start_of_day().ok());
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

    /// Each workday's share as much as its work by `now` has spent, the day the window
    /// starts in whole.
    fn allowed(&self, from: u64, now: u64) -> u64 {
        self.days(from)
            .take_while(|&(start, _, _)| start <= now)
            .filter(|&(_, _, work)| work)
            .map(|(start, end, _)| {
                let worked = now
                    .saturating_sub(self.work_starts(start, end))
                    .min(WORKDAY);
                (end - start) * worked / WORKDAY
            })
            .sum()
    }

    fn after(&self, from: u64, seconds: u64) -> u64 {
        let mut left = seconds;
        // A day's work started late runs on past midnight.
        for (start, end, work) in self.days(from.saturating_sub(WORKDAY)) {
            let begin = self.work_starts(start, end);
            if !work || begin + WORKDAY <= from {
                continue;
            }
            let at = begin.max(from);
            let share = end - start;
            let given = (begin + WORKDAY - at) * share / WORKDAY;
            if left <= given {
                return at + left * WORKDAY / share;
            }
            left -= given;
        }
        from.saturating_add(seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;
    const HOUR: u64 = 3600;

    fn at(time: &str) -> u64 {
        time.parse::<Timestamp>().unwrap().as_second() as u64
    }

    fn utc(started: Option<&str>) -> Workdays {
        Workdays {
            zone: TimeZone::UTC,
            started: started.map(at),
        }
    }

    fn near(value: Option<f32>, expected: f32) -> bool {
        value.is_some_and(|value| (value - expected).abs() < 1e-4)
    }

    #[test]
    fn a_workdays_share_is_spent_over_its_work_hours() {
        let calendar = utc(None);
        let saturday = at("2026-11-07T00:00:00Z");
        assert_eq!(calendar.counted(saturday, saturday + 7 * DAY), 5 * DAY);
        assert_eq!(calendar.counted(saturday, saturday + 2 * DAY), 0);
        let friday_noon = at("2026-11-13T12:00:00Z");
        assert_eq!(
            calendar.after(friday_noon, DAY),
            at("2026-11-16T12:00:00Z"),
            "Friday's last five hours of work, then Monday's first three"
        );
    }

    #[test]
    fn a_week_spent_on_workdays_is_paced_by_their_work_hours() {
        let saturday = at("2026-11-07T00:00:00Z");
        let window = QuotaWindow {
            used: 0.25,
            resets_at: saturday + 7 * DAY,
        };
        let monday_noon = at("2026-11-09T12:00:00Z");
        let workdays = window.standing_by(7 * DAY, monday_noon, &utc(None));
        assert!(
            near(workdays.even, 0.075),
            "three hours of Monday's eight, of five days"
        );
        assert_eq!(
            workdays.out_in,
            Some(25 * HOUR),
            "a quarter by noon on Monday: the rest by 1pm on Tuesday"
        );
        let every_day = window.standing(7 * DAY, monday_noon);
        assert!(every_day.even.unwrap() > 0.35);
        assert_eq!(every_day.out_in, None);
    }

    #[test]
    fn work_starts_when_the_day_did_and_the_other_days_at_that_time() {
        let tuesday_afternoon = at("2026-11-10T13:00:00Z");
        let window = QuotaWindow {
            used: 0.17,
            resets_at: tuesday_afternoon + 7 * DAY,
        };
        let started = Some("2026-11-10T10:30:00Z");
        let noon = window.standing_by(7 * DAY, at("2026-11-10T14:30:00Z"), &utc(started));
        assert!(
            near(noon.even, 0.1),
            "half of Tuesday's share, four hours in"
        );
        let evening = window.standing_by(7 * DAY, at("2026-11-10T20:00:00Z"), &utc(started));
        assert!(
            near(evening.even, 0.2),
            "the whole day's share eight hours in"
        );
        assert_eq!(evening.out_in, None, "a day's share a day lasts the week");
        let next_monday = QuotaWindow {
            used: 0.5,
            ..window
        }
        .standing_by(7 * DAY, at("2026-11-16T12:30:00Z"), &utc(started));
        assert!(
            near(next_monday.even, 0.85),
            "four days, and two of Monday's eight hours from 10:30"
        );
    }

    #[test]
    fn a_day_begins_at_the_first_activity_after_four() {
        let zone = TimeZone::UTC;
        let today = at("2026-11-10T09:00:00Z");
        assert!(begins_day(None, today, &zone));
        assert!(begins_day(Some(at("2026-11-09T09:00:00Z")), today, &zone));
        assert!(!begins_day(Some(at("2026-11-10T08:00:00Z")), today, &zone));
        let night = at("2026-11-11T01:30:00Z");
        assert!(
            !begins_day(Some(at("2026-11-10T09:00:00Z")), night, &zone),
            "still Tuesday's night"
        );
    }

    #[test]
    fn work_begun_late_runs_on_past_midnight() {
        let calendar = utc(Some("2026-11-13T20:00:00Z"));
        assert_eq!(
            calendar.after(at("2026-11-14T01:00:00Z"), 9 * HOUR),
            at("2026-11-14T04:00:00Z"),
            "Friday's last three hours of work, past midnight"
        );
    }

    #[test]
    fn work_starts_at_the_same_clock_time_across_a_change_of_summer_time() {
        let toronto = Workdays {
            zone: TimeZone::get("America/Toronto").unwrap(),
            started: Some(at("2026-11-01T14:00:00Z")),
        };
        let monday = at("2026-11-02T05:00:00Z");
        assert_eq!(
            toronto.work_starts(monday, monday + DAY),
            at("2026-11-02T14:00:00Z"),
            "9:00 on Sunday, the day clocks fell back, and 9:00 on Monday"
        );
    }
}
