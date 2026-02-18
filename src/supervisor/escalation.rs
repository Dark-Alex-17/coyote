use fmt::{Debug, Formatter};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use tokio::sync::oneshot;
use uuid::Uuid;

pub struct EscalationRequest {
    pub id: String,
    pub from_agent_id: String,
    pub from_agent_name: String,
    pub question: String,
    pub options: Option<Vec<String>>,
    pub reply_tx: oneshot::Sender<String>,
}

pub struct EscalationQueue {
    pending: parking_lot::Mutex<HashMap<String, EscalationRequest>>,
}

impl EscalationQueue {
    pub fn new() -> Self {
        Self {
            pending: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    pub fn submit(&self, request: EscalationRequest) -> String {
        let id = request.id.clone();
        self.pending.lock().insert(id.clone(), request);
        id
    }

    pub fn take(&self, escalation_id: &str) -> Option<EscalationRequest> {
        self.pending.lock().remove(escalation_id)
    }

    pub fn pending_summary(&self) -> Vec<Value> {
        self.pending
            .lock()
            .values()
            .map(|r| {
                let mut entry = json!({
                    "escalation_id": r.id,
                    "from_agent_id": r.from_agent_id,
                    "from_agent_name": r.from_agent_name,
                    "question": r.question,
                });
                if let Some(ref options) = r.options {
                    entry["options"] = json!(options);
                }
                entry
            })
            .collect()
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.lock().is_empty()
    }
}

impl Default for EscalationQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for EscalationQueue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let count = self.pending.lock().len();
        f.debug_struct("EscalationQueue")
            .field("pending_count", &count)
            .finish()
    }
}

pub fn new_escalation_id() -> String {
    let short = &Uuid::new_v4().to_string()[..8];
    format!("esc_{short}")
}
