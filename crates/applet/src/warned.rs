use claudo_protocol::Quota;

/// For each window, the reset of the last one warned of, so a window warns once.
#[derive(Debug, Default)]
pub struct Warned([Option<u64>; 2]);

impl Warned {
    /// The windows that would run out before their reset and have not warned yet: each
    /// label with how long until it runs out at the pace so far.
    #[cfg_attr(not(feature = "cosmic"), allow(dead_code))]
    pub fn fresh(&mut self, quota: Quota, now: u64) -> Vec<(&'static str, u64)> {
        quota
            .windows()
            .into_iter()
            .zip(&mut self.0)
            .filter_map(|((label, window, length), warned)| {
                let window = window?;
                let out_in = window.standing(length, now).out_in?;
                if warned.is_some_and(|reset| now < reset) {
                    return None;
                }
                *warned = Some(window.resets_at);
                Some((label, out_in))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudo_protocol::QuotaWindow;

    const HOUR: u64 = 3600;

    fn session(used: f32, resets_at: u64) -> Quota {
        Quota {
            five_hour: Some(QuotaWindow { used, resets_at }),
            seven_day: None,
        }
    }

    #[test]
    fn a_window_ahead_of_its_pace_warns_once_until_it_resets() {
        let mut warned = Warned::default();
        let now = 1_000_000;
        let reset = now + 4 * HOUR;
        assert_eq!(
            warned.fresh(session(0.5, reset), now),
            vec![("Session", HOUR)]
        );
        assert!(warned.fresh(session(0.6, reset + 1), now + 60).is_empty());
        let next = reset + 5 * HOUR;
        assert!(
            warned.fresh(session(0.4, next), reset).is_empty(),
            "just started"
        );
        assert_eq!(
            warned.fresh(session(0.9, next), reset + HOUR).len(),
            1,
            "the next window warns again"
        );
    }
}
