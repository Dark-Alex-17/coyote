use crate::mesh::r3::client::RequestOutcome;
use crate::mesh::r3::error::R3Error;

use rmpv::Value;
use std::future::Future;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Where one outbound request stands. `Delivered` means the far end proved it holds the
/// whole request, which only a request sent as a resource gets: upstream proves resource
/// transfers but not plain link packets, so a packet-sized request goes straight from
/// `Sent` to `Ready` or `Failed`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReceiptState {
    Sent,
    Delivered,
    Ready(Value),
    Failed(R3Error),
}

/// A handle on one request in flight. Dropping it aborts the request: nobody is left to
/// read the answer, and the client's pending entry goes with the aborted task. Aborting a
/// finished task is a no-op, so a receipt that already settled costs nothing to drop.
pub(crate) struct RequestReceipt {
    state: watch::Receiver<ReceiptState>,
    driver: JoinHandle<()>,
}

// Read by the REPL and the message tools once they land.
#[allow(dead_code)]
impl RequestReceipt {
    /// Runs the request `send` builds, handing it the hook the client fires on delivery.
    /// `cancel` firing while the request is in flight settles it as `Failed(Shutdown)`.
    pub(crate) fn track<F>(
        cancel: CancellationToken,
        send: impl FnOnce(oneshot::Sender<()>) -> F,
    ) -> Self
    where
        F: Future<Output = Result<RequestOutcome, R3Error>> + Send + 'static,
    {
        let (state_tx, state) = watch::channel(ReceiptState::Sent);
        let (delivered_tx, mut delivered) = oneshot::channel();
        let request = send(delivered_tx);
        let driver = tokio::spawn(async move {
            tokio::pin!(request);
            let outcome = tokio::select! {
                () = cancel.cancelled() => Err(R3Error::Shutdown),
                outcome = &mut request => {
                    // The hook and the reply can fire back to back; keep the order.
                    if delivered.try_recv().is_ok() {
                        state_tx.send_replace(ReceiptState::Delivered);
                    }
                    outcome
                }
                proof = &mut delivered => {
                    if proof.is_ok() {
                        state_tx.send_replace(ReceiptState::Delivered);
                    }
                    tokio::select! {
                        () = cancel.cancelled() => Err(R3Error::Shutdown),
                        outcome = &mut request => outcome,
                    }
                }
            };
            state_tx.send_replace(match outcome {
                Ok(outcome) => ReceiptState::Ready(outcome.value),
                Err(err) => ReceiptState::Failed(err),
            });
        });
        Self { state, driver }
    }

    pub(crate) fn state(&self) -> ReceiptState {
        self.state.borrow().clone()
    }

    /// The next state after the current one, or `None` once no further change can come
    /// because the request task is gone.
    pub(crate) async fn changed(&mut self) -> Option<ReceiptState> {
        self.state.changed().await.ok()?;
        Some(self.state.borrow_and_update().clone())
    }

    /// Waits for the terminal state.
    pub(crate) async fn wait(mut self) -> Result<Value, R3Error> {
        loop {
            match self.state.borrow_and_update().clone() {
                ReceiptState::Ready(value) => return Ok(value),
                ReceiptState::Failed(err) => return Err(err),
                ReceiptState::Sent | ReceiptState::Delivered => {}
            }
            if self.state.changed().await.is_err() {
                return Err(R3Error::Shutdown);
            }
        }
    }
}

impl Drop for RequestReceipt {
    fn drop(&mut self) {
        self.driver.abort();
    }
}
