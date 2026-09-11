use super::{AbortSignal, IS_STDOUT_TERMINAL, poll_abort_signal, wait_abort_signal};

use anyhow::{Result, bail};
use crossterm::{cursor, queue, style, terminal};
#[cfg(test)]
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::{
    future::Future,
    io::{Write, stdout},
    time::Duration,
};
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver},
        oneshot,
    },
    time::interval,
};

#[derive(Debug, Default)]
pub struct SpinnerInner {
    index: usize,
    message: String,
    #[cfg(test)]
    cleared: Option<Arc<AtomicUsize>>,
}

impl SpinnerInner {
    const DATA: [&'static str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    #[cfg(test)]
    fn observed(cleared: Arc<AtomicUsize>) -> Self {
        Self {
            index: 0,
            message: String::new(),
            cleared: Some(cleared),
        }
    }

    fn step(&mut self) -> Result<()> {
        if !*IS_STDOUT_TERMINAL || self.message.is_empty() {
            return Ok(());
        }
        let mut writer = stdout();
        let frame = Self::DATA[self.index % Self::DATA.len()];
        let dots = ".".repeat((self.index / 5) % 4);
        let line = format!("{frame}{}{:<3}", self.message, dots);
        queue!(writer, cursor::MoveToColumn(0), style::Print(line),)?;
        if self.index == 0 {
            queue!(writer, cursor::Hide)?;
        }
        writer.flush()?;
        self.index += 1;
        Ok(())
    }

    fn set_message(&mut self, message: String) -> Result<()> {
        self.clear_message()?;
        if !message.is_empty() {
            self.message = format!(" {message}");
        }
        Ok(())
    }

    fn clear_message(&mut self) -> Result<()> {
        if self.message.is_empty() {
            return Ok(());
        }
        self.message.clear();
        #[cfg(test)]
        if let Some(cleared) = &self.cleared {
            cleared.fetch_add(1, Ordering::SeqCst);
        }
        if !*IS_STDOUT_TERMINAL {
            return Ok(());
        }
        let mut writer = stdout();
        queue!(
            writer,
            cursor::MoveToColumn(0),
            terminal::Clear(terminal::ClearType::FromCursorDown),
            cursor::Show
        )?;
        writer.flush()?;
        Ok(())
    }
}

/// Restores the terminal when the spinner goes away by any route, not just the
/// loop's normal exit: a future cancelled mid-spin (the surrounding task was
/// aborted) or an early `?` return would otherwise leave the last frame on
/// screen and the cursor hidden. `clear_message` is a no-op once the message
/// is gone, so a normal-path clear leaves nothing for the drop to redo.
impl Drop for SpinnerInner {
    fn drop(&mut self) {
        let _ = self.clear_message();
    }
}

#[derive(Clone)]
pub struct Spinner(mpsc::UnboundedSender<SpinnerEvent>);

impl Spinner {
    pub fn create(message: &str) -> (Self, UnboundedReceiver<SpinnerEvent>) {
        let (tx, spinner_rx) = mpsc::unbounded_channel();
        let spinner = Spinner(tx);
        let _ = spinner.set_message(message.to_string());
        (spinner, spinner_rx)
    }

    pub fn set_message(&self, message: String) -> Result<()> {
        self.0.send(SpinnerEvent::SetMessage(message))?;
        std::thread::sleep(Duration::from_millis(10));
        Ok(())
    }

    pub fn stop(&self) {
        let _ = self.0.send(SpinnerEvent::Stop);
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub enum SpinnerEvent {
    SetMessage(String),
    Stop,
}

pub fn spawn_spinner(message: &str) -> Spinner {
    let (spinner, mut spinner_rx) = Spinner::create(message);
    tokio::spawn(async move {
        let mut spinner = SpinnerInner::default();
        let mut interval = interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                evt = spinner_rx.recv() => {
                    if let Some(evt) = evt {
                        match evt {
                            SpinnerEvent::SetMessage(message) => {
                                spinner.set_message(message)?;
                            }
                            SpinnerEvent::Stop => {
                                spinner.clear_message()?;
                                break;
                            }
                        }

                    }
                }
                _ = interval.tick() => {
                    let _ = spinner.step();
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    });
    spinner
}

pub async fn abortable_run_with_spinner<F, T>(
    task: F,
    message: &str,
    abort_signal: AbortSignal,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let (_, spinner_rx) = Spinner::create(message);
    abortable_run_with_spinner_rx(task, spinner_rx, abort_signal).await
}

pub async fn abortable_run_with_spinner_rx<F, T>(
    task: F,
    spinner_rx: UnboundedReceiver<SpinnerEvent>,
    abort_signal: AbortSignal,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    if *IS_STDOUT_TERMINAL {
        let (done_tx, done_rx) = oneshot::channel();
        let run_task = async {
            tokio::select! {
                ret = task => {
                    let _ = done_tx.send(());
                    ret
                }
                _ = tokio::signal::ctrl_c() => {
                    abort_signal.set_ctrlc();
                    let _ = done_tx.send(());
                    bail!("Aborted!")
                },
                _ = wait_abort_signal(&abort_signal) => {
                    let _ = done_tx.send(());
                    bail!("Aborted.");
                },
            }
        };
        let (task_ret, spinner_ret) = tokio::join!(
            run_task,
            run_abortable_spinner(spinner_rx, done_rx, abort_signal.clone())
        );
        spinner_ret?;
        task_ret
    } else {
        task.await
    }
}

async fn run_abortable_spinner(
    mut spinner_rx: UnboundedReceiver<SpinnerEvent>,
    mut done_rx: oneshot::Receiver<()>,
    abort_signal: AbortSignal,
) -> Result<()> {
    let mut spinner = SpinnerInner::default();
    loop {
        if abort_signal.aborted() {
            break;
        }

        tokio::time::sleep(Duration::from_millis(25)).await;

        match done_rx.try_recv() {
            Ok(_) | Err(oneshot::error::TryRecvError::Closed) => {
                break;
            }
            _ => {}
        }

        match spinner_rx.try_recv() {
            Ok(SpinnerEvent::SetMessage(message)) => {
                spinner.set_message(message)?;
            }
            Ok(SpinnerEvent::Stop) => {
                spinner.clear_message()?;
            }
            Err(_) => {}
        }

        if poll_abort_signal(&abort_signal)? {
            break;
        }

        spinner.step()?;
    }

    spinner.clear_message()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed(message: &str) -> (SpinnerInner, Arc<AtomicUsize>) {
        let cleared = Arc::new(AtomicUsize::new(0));
        let mut spinner = SpinnerInner::observed(cleared.clone());
        spinner.set_message(message.to_string()).unwrap();
        (spinner, cleared)
    }

    #[test]
    fn drop_with_active_message_clears_once() {
        let (spinner, cleared) = observed("Thinking");
        assert_eq!(cleared.load(Ordering::SeqCst), 0);
        drop(spinner);
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn explicit_clear_then_drop_clears_exactly_once() {
        let (mut spinner, cleared) = observed("Thinking");
        spinner.clear_message().unwrap();
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
        drop(spinner);
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn drop_without_message_is_silent() {
        let (spinner, cleared) = observed("");

        drop(spinner);

        assert_eq!(cleared.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn set_message_replaces_and_clears_previous_once() {
        let (mut spinner, cleared) = observed("Thinking");
        spinner.set_message("Fetching".to_string()).unwrap();
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
        drop(spinner);
        assert_eq!(cleared.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancelled_task_restores_terminal_on_drop() {
        let (spinner, cleared) = observed("Thinking");
        let task = tokio::spawn(async move {
            let _spinner = spinner;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(cleared.load(Ordering::SeqCst), 0);

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
    }
}
