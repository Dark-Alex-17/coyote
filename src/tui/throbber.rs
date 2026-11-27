//! The throbber: the REPL spinner's braille frames, turned by the App's
//! clock so a frame drawn at a given instant always shows the same one.

use crate::utils::SPINNER_FRAMES;
use std::time::{Duration, Instant};

pub(super) const THROBBER_INTERVAL: Duration = Duration::from_millis(100);

/// The frame showing at `now` for a throbber that started at `origin`.
pub(super) fn frame_at(origin: Instant, now: Instant) -> &'static str {
    let steps = now.saturating_duration_since(origin).as_millis() / THROBBER_INTERVAL.as_millis();
    let index = usize::try_from(steps % SPINNER_FRAMES.len() as u128).unwrap_or(0);
    SPINNER_FRAMES[index]
}

/// A text that starts with a throbber frame and a space, split into the
/// frame and the rest, the space included; None for any other text.
pub(super) fn split_frame(text: &str) -> Option<(&'static str, &str)> {
    SPINNER_FRAMES.iter().find_map(|frame| {
        text.strip_prefix(frame)
            .filter(|rest| rest.starts_with(' '))
            .map(|rest| (*frame, rest))
    })
}

/// When a frame is drawn, and the fixed instant its throbber counts from.
#[derive(Debug, Clone, Copy)]
pub(super) struct Tick {
    pub(super) origin: Instant,
    pub(super) now: Instant,
}

impl Tick {
    pub(super) fn frame(self) -> &'static str {
        frame_at(self.origin, self.now)
    }

    pub(super) fn elapsed(self, since: Instant) -> Duration {
        self.now.saturating_duration_since(since)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_advances_once_per_interval_and_wraps() {
        let origin = Instant::now();
        let at = |millis: u64| frame_at(origin, origin + Duration::from_millis(millis));
        assert_eq!(at(0), SPINNER_FRAMES[0]);
        assert_eq!(at(99), SPINNER_FRAMES[0]);
        assert_eq!(at(100), SPINNER_FRAMES[1]);
        assert_eq!(at(250), SPINNER_FRAMES[2]);
        assert_eq!(at(1000), SPINNER_FRAMES[0]);
        assert_eq!(
            frame_at(origin + Duration::from_secs(1), origin),
            SPINNER_FRAMES[0]
        );
    }

    #[test]
    fn split_frame_takes_only_a_leading_frame_and_its_space() {
        let text = format!("{} running 3s", SPINNER_FRAMES[4]);
        assert_eq!(split_frame(&text), Some((SPINNER_FRAMES[4], " running 3s")));
        assert_eq!(split_frame(SPINNER_FRAMES[4]), None);
        assert_eq!(split_frame("running 3s"), None);
        assert_eq!(split_frame(""), None);
    }
}
