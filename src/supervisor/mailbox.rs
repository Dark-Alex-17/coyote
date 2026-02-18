use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub from: String,
    pub to: String,
    pub payload: EnvelopePayload,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EnvelopePayload {
    Text { content: String },
    TaskCompleted { task_id: String, summary: String },
    ShutdownRequest { reason: String },
    ShutdownApproved,
}

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

    pub fn drain(&self) -> Vec<Envelope> {
        let mut msgs = {
            let mut guard = self.messages.lock();
            std::mem::take(&mut *guard)
        };

        msgs.sort_by_key(|e| match &e.payload {
            EnvelopePayload::ShutdownRequest { .. } => 0,
            EnvelopePayload::ShutdownApproved => 0,
            EnvelopePayload::TaskCompleted { .. } => 1,
            EnvelopePayload::Text { .. } => 2,
        });

        msgs
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
