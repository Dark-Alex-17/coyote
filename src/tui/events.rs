//! What drives a frame: the frame scheduler coalesces every request made
//! between two draws into one and keeps draws at most 120 per second, and
//! terminal events are reduced to the few inputs the app reacts to before
//! they reach it. The clock is injected so tests can move time by hand.

use crossterm::event::{Event, KeyEvent, KeyEventKind};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Shortest gap between two draws (120 frames per second).
pub(crate) const MIN_FRAME_INTERVAL: Duration = Duration::from_micros(8_333);

pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Decides when the next frame is due. Requests made while one is pending
/// fold into it; a request right after a draw waits out the frame interval.
pub(crate) struct FrameScheduler {
    clock: Arc<dyn Clock>,
    due: Option<Instant>,
    last_draw: Option<Instant>,
}

impl FrameScheduler {
    pub(crate) fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            due: None,
            last_draw: None,
        }
    }

    pub(crate) fn schedule_frame(&mut self) {
        self.schedule_frame_in(Duration::ZERO);
    }

    pub(crate) fn schedule_frame_in(&mut self, delay: Duration) {
        let now = self.clock.now();
        let earliest = self
            .last_draw
            .map_or(now, |last| last + MIN_FRAME_INTERVAL)
            .max(now);
        let due = (now + delay).max(earliest);
        self.due = Some(self.due.map_or(due, |pending| pending.min(due)));
    }

    #[cfg(test)]
    pub(crate) fn due_at(&self) -> Option<Instant> {
        self.due
    }

    /// Records that the pending frame was drawn.
    pub(crate) fn frame_drawn(&mut self) {
        self.due = None;
        self.last_draw = Some(self.clock.now());
    }

    /// Resolves when the pending frame is due; never resolves while none is.
    pub(crate) async fn next_frame(&self) {
        match self.due {
            Some(due) => tokio::time::sleep_until(due.into()).await,
            None => std::future::pending().await,
        }
    }
}

/// A terminal event reduced to what the app handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TermInput {
    /// A key press; releases and repeats are dropped.
    Key(KeyEvent),
    Paste(String),
    /// Only schedules a frame: the next draw reads the new size from
    /// `Frame::area()`.
    Resize,
    Ignored,
}

pub(crate) fn classify(event: Event) -> TermInput {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => TermInput::Key(key),
        Event::Key(_) => TermInput::Ignored,
        Event::Paste(text) => TermInput::Paste(text),
        Event::Resize(_, _) => TermInput::Resize,
        Event::Mouse(_) | Event::FocusGained | Event::FocusLost => TermInput::Ignored,
    }
}

#[cfg(test)]
pub(crate) struct ManualClock(pub(crate) std::sync::Mutex<Instant>);

#[cfg(test)]
impl ManualClock {
    pub(crate) fn new() -> Self {
        Self(std::sync::Mutex::new(Instant::now()))
    }

    pub(crate) fn advance(&self, by: Duration) {
        let mut now = self.0.lock().unwrap();
        *now += by;
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventState, KeyModifiers};

    #[test]
    fn five_requests_fold_into_one_frame() {
        let clock = Arc::new(ManualClock::new());
        let mut frames = FrameScheduler::new(clock.clone());
        assert_eq!(frames.due_at(), None);
        for _ in 0..5 {
            frames.schedule_frame();
        }
        assert_eq!(frames.due_at(), Some(clock.now()));
        frames.frame_drawn();
        assert_eq!(frames.due_at(), None);
    }

    #[test]
    fn a_request_right_after_a_draw_waits_out_the_frame_interval() {
        let clock = Arc::new(ManualClock::new());
        let mut frames = FrameScheduler::new(clock.clone());
        frames.schedule_frame();
        frames.frame_drawn();
        let drawn_at = clock.now();
        frames.schedule_frame();
        assert_eq!(frames.due_at(), Some(drawn_at + MIN_FRAME_INTERVAL));
        clock.advance(Duration::from_secs(1));
        frames.frame_drawn();
        frames.schedule_frame();
        assert_eq!(frames.due_at(), Some(clock.now() + MIN_FRAME_INTERVAL));
    }

    #[test]
    fn a_nearer_request_pulls_a_delayed_frame_forward_and_never_pushes_it_back() {
        let clock = Arc::new(ManualClock::new());
        let mut frames = FrameScheduler::new(clock.clone());
        frames.schedule_frame_in(Duration::from_secs(1));
        assert_eq!(frames.due_at(), Some(clock.now() + Duration::from_secs(1)));
        frames.schedule_frame();
        assert_eq!(frames.due_at(), Some(clock.now()));
        frames.schedule_frame_in(Duration::from_secs(1));
        assert_eq!(frames.due_at(), Some(clock.now()));
    }

    fn key(code: KeyCode, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn only_key_presses_reach_the_app() {
        assert!(matches!(
            classify(key(KeyCode::Char('a'), KeyEventKind::Press)),
            TermInput::Key(_)
        ));
        assert_eq!(
            classify(key(KeyCode::Char('a'), KeyEventKind::Release)),
            TermInput::Ignored
        );
        assert_eq!(
            classify(key(KeyCode::Char('a'), KeyEventKind::Repeat)),
            TermInput::Ignored
        );
    }

    #[test]
    fn resize_only_asks_for_a_frame() {
        assert_eq!(classify(Event::Resize(40, 24)), TermInput::Resize);
        assert_eq!(classify(Event::FocusGained), TermInput::Ignored);
    }
}
