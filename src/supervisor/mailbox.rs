use serde::{Deserialize, Serialize};

/// A message envelope routed between agents.
///
/// Agents communicate by sending `Envelope`s to each other's mailboxes.
/// The sender fires and forgets; the receiver drains its inbox between
/// LLM turns via the `check_inbox` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub from: String,
    pub to: String,
    pub payload: EnvelopePayload,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// The content of an inter-agent message.
///
/// Separates the **control plane** (shutdown signals, task lifecycle events)
/// from the **data plane** (free-form text). Control-plane messages are
/// processed before data-plane messages to prevent race conditions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EnvelopePayload {
    Text { content: String },
    TaskCompleted { task_id: String, summary: String },
    ShutdownRequest { reason: String },
    ShutdownApproved,
}

/// A per-agent inbox that collects incoming messages.
///
/// Backed by a `Vec` behind a `parking_lot::Mutex` so it can be shared
/// between the supervisor (which delivers messages) and the agent's tool
/// handler (which drains them). We use `parking_lot::Mutex` to match the
/// locking convention used elsewhere in Loki (`parking_lot::RwLock` for
/// GlobalConfig).
#[derive(Debug, Default)]
pub struct Inbox {
    messages: parking_lot::Mutex<Vec<Envelope>>,
}

impl Inbox {
    pub fn new() -> Self {
        Self {
            messages: parking_lot::Mutex::new(Vec::new()),
        }
    }

    pub fn deliver(&self, envelope: Envelope) {
        self.messages.lock().push(envelope);
    }

    /// Drain all pending messages, returning them sorted with control-plane
    /// messages first (shutdown, task events) then data-plane (text).
    /// This ordering prevents the class of bugs where a text message
    /// references state that a control message was supposed to set up.
    pub fn drain(&self) -> Vec<Envelope> {
        let mut msgs = {
            let mut guard = self.messages.lock();
            std::mem::take(&mut *guard)
        };

        // Stable partition: control messages first, then data messages,
        // preserving relative order within each group.
        msgs.sort_by_key(|e| match &e.payload {
            EnvelopePayload::ShutdownRequest { .. } => 0,
            EnvelopePayload::ShutdownApproved => 0,
            EnvelopePayload::TaskCompleted { .. } => 1,
            EnvelopePayload::Text { .. } => 2,
        });

        msgs
    }

    pub fn pending_count(&self) -> usize {
        self.messages.lock().len()
    }
}

impl Clone for Inbox {
    fn clone(&self) -> Self {
        let messages = self.messages.lock().clone();
        Self {
            messages: parking_lot::Mutex::new(messages),
        }
    }
}
