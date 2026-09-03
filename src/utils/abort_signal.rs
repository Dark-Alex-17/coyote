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

pub fn poll_abort_signal(abort_signal: &AbortSignal) -> Result<bool> {
    if event::poll(Duration::from_millis(25))?
        && let Event::Key(key) = event::read()?
    {
        match key.code {
            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                abort_signal.set_ctrlc();
                return Ok(true);
            }
            KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
                abort_signal.set_ctrld();
                return Ok(true);
            }
            _ => {}
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_user_interrupt_returns_promptly_on_preset_session_signal() {
        let signal = create_abort_signal();
        signal.set_ctrlc();

        tokio::time::timeout(Duration::from_secs(1), wait_user_interrupt(Some(&signal)))
            .await
            .expect("must return promptly when the session signal is already set");
    }
}
