use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub type AbortSignal = Arc<AbortSignalInner>;

pub struct AbortSignalInner {
    ctrlc: AtomicBool,
    ctrld: AtomicBool,
}

pub fn create_abort_signal() -> AbortSignal {
    AbortSignalInner::new()
}

impl AbortSignalInner {
    pub fn new() -> AbortSignal {
        Arc::new(Self {
            ctrlc: AtomicBool::new(false),
            ctrld: AtomicBool::new(false),
        })
    }

    pub fn aborted(&self) -> bool {
        if self.aborted_ctrlc() {
            return true;
        }
        if self.aborted_ctrld() {
            return true;
        }
        false
    }

    pub fn aborted_ctrlc(&self) -> bool {
        self.ctrlc.load(Ordering::SeqCst)
    }

    pub fn aborted_ctrld(&self) -> bool {
        self.ctrld.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.ctrlc.store(false, Ordering::SeqCst);
        self.ctrld.store(false, Ordering::SeqCst);
    }

    pub fn set_ctrlc(&self) {
        self.ctrlc.store(true, Ordering::SeqCst);
    }

    pub fn set_ctrld(&self) {
        self.ctrld.store(true, Ordering::SeqCst);
    }
}

pub async fn wait_abort_signal(abort_signal: &AbortSignal) {
    loop {
        if abort_signal.aborted() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Completes when the user interrupts: a SIGINT arrives, or the session's turn
/// signal is (or becomes) aborted. Needed because tools run in cooked mode --
/// tokio's global SIGINT handler (installed by the first `ctrl_c()` poll)
/// swallows the signal with nobody listening. On SIGINT, sets ctrl-c on the
/// session signal so the surrounding turn aborts too. With `None`, only the
/// SIGINT arm applies.
pub async fn wait_user_interrupt(session: Option<&AbortSignal>) {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    match session {
        Some(signal) => {
            tokio::select! {
                _ = ctrl_c => signal.set_ctrlc(),
                _ = wait_abort_signal(signal) => {}
            }
        }
        None => ctrl_c.await,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainOutcome {
    pub aborted: bool,
    pub resized: bool,
}

/// Drains queued terminal events until an abort key is seen; `timeout` applies
/// to the first poll only.
pub fn drain_terminal_events(
    abort_signal: &AbortSignal,
    timeout: Duration,
) -> Result<DrainOutcome> {
    let mut outcome = DrainOutcome::default();
    let mut timeout = timeout;
    while !outcome.aborted && event::poll(timeout)? {
        timeout = Duration::ZERO;
        classify_event(&event::read()?, abort_signal, &mut outcome);
    }
    Ok(outcome)
}

fn classify_event(event: &Event, abort_signal: &AbortSignal, outcome: &mut DrainOutcome) {
    match event {
        Event::Key(key) => match key.code {
            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                abort_signal.set_ctrlc();
                outcome.aborted = true;
            }
            KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
                abort_signal.set_ctrld();
                outcome.aborted = true;
            }
            _ => {}
        },
        Event::Resize(..) => outcome.resized = true,
        _ => {}
    }
}

pub fn poll_abort_signal(abort_signal: &AbortSignal) -> Result<bool> {
    Ok(drain_terminal_events(abort_signal, Duration::from_millis(25))?.aborted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    #[tokio::test]
    async fn wait_user_interrupt_returns_promptly_on_preset_session_signal() {
        let signal = create_abort_signal();
        signal.set_ctrlc();

        tokio::time::timeout(Duration::from_secs(1), wait_user_interrupt(Some(&signal)))
            .await
            .expect("must return promptly when the session signal is already set");
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn classify_event_ignores_plain_keys_and_notes_resizes() {
        let signal = create_abort_signal();
        let mut outcome = DrainOutcome::default();

        classify_event(
            &key(KeyCode::Char('c'), KeyModifiers::NONE),
            &signal,
            &mut outcome,
        );
        assert_eq!(outcome, DrainOutcome::default());
        assert!(!signal.aborted());

        classify_event(&Event::Resize(80, 24), &signal, &mut outcome);
        assert_eq!(
            outcome,
            DrainOutcome {
                aborted: false,
                resized: true
            }
        );
        assert!(!signal.aborted());
    }

    #[test]
    fn classify_event_ctrl_c_sets_ctrlc_and_aborts() {
        let signal = create_abort_signal();
        let mut outcome = DrainOutcome::default();
        classify_event(
            &key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &signal,
            &mut outcome,
        );
        assert!(outcome.aborted);
        assert!(!outcome.resized);
        assert!(signal.aborted_ctrlc());
        assert!(!signal.aborted_ctrld());
    }

    #[test]
    fn classify_event_ctrl_d_sets_ctrld_and_aborts() {
        let signal = create_abort_signal();
        let mut outcome = DrainOutcome::default();
        classify_event(
            &key(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &signal,
            &mut outcome,
        );
        assert!(outcome.aborted);
        assert!(!outcome.resized);
        assert!(signal.aborted_ctrld());
        assert!(!signal.aborted_ctrlc());
    }
}
