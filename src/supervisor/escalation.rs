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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(
        id: &str,
        agent_id: &str,
        question: &str,
    ) -> (EscalationRequest, oneshot::Receiver<String>) {
        let (tx, rx) = oneshot::channel();
        let req = EscalationRequest {
            id: id.to_string(),
            from_agent_id: agent_id.to_string(),
            from_agent_name: "test-agent".to_string(),
            question: question.to_string(),
            options: None,
            reply_tx: tx,
        };
        (req, rx)
    }

    #[test]
    fn queue_default_has_no_pending() {
        let queue = EscalationQueue::default();
        assert!(!queue.has_pending());
    }

    #[test]
    fn submit_and_has_pending() {
        let queue = EscalationQueue::new();
        let (req, _rx) = make_request("esc_1", "agent_1", "What color?");
        queue.submit(req);
        assert!(queue.has_pending());
    }

    #[test]
    fn submit_returns_id() {
        let queue = EscalationQueue::new();
        let (req, _rx) = make_request("esc_42", "agent_1", "question");
        let id = queue.submit(req);
        assert_eq!(id, "esc_42");
    }

    #[test]
    fn take_removes_request() {
        let queue = EscalationQueue::new();
        let (req, _rx) = make_request("esc_1", "agent_1", "question");
        queue.submit(req);
        let taken = queue.take("esc_1");
        assert!(taken.is_some());
        assert!(!queue.has_pending());
    }

    #[test]
    fn take_nonexistent_returns_none() {
        let queue = EscalationQueue::new();
        assert!(queue.take("esc_missing").is_none());
    }

    #[test]
    fn pending_summary_contains_fields() {
        let queue = EscalationQueue::new();
        let (req, _rx) = make_request("esc_1", "agent_x", "What to do?");
        queue.submit(req);
        let summary = queue.pending_summary();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0]["escalation_id"], "esc_1");
        assert_eq!(summary[0]["from_agent_id"], "agent_x");
        assert_eq!(summary[0]["question"], "What to do?");
    }

    #[test]
    fn pending_summary_includes_options_when_present() {
        let queue = EscalationQueue::new();
        let (tx, _rx) = oneshot::channel();
        let req = EscalationRequest {
            id: "esc_1".into(),
            from_agent_id: "a".into(),
            from_agent_name: "agent".into(),
            question: "Pick one".into(),
            options: Some(vec!["A".into(), "B".into()]),
            reply_tx: tx,
        };
        queue.submit(req);
        let summary = queue.pending_summary();
        assert!(summary[0].get("options").is_some());
    }

    #[test]
    fn pending_summary_empty_when_no_requests() {
        let queue = EscalationQueue::new();
        assert!(queue.pending_summary().is_empty());
    }

    #[test]
    fn reply_reaches_receiver() {
        let queue = EscalationQueue::new();
        let (req, rx) = make_request("esc_1", "a", "question");
        queue.submit(req);
        let taken = queue.take("esc_1").unwrap();
        taken.reply_tx.send("the answer".into()).unwrap();
        assert_eq!(rx.blocking_recv().unwrap(), "the answer");
    }

    #[test]
    fn new_escalation_id_has_prefix() {
        let id = new_escalation_id();
        assert!(id.starts_with("esc_"));
        assert!(id.len() > 4);
    }

    #[test]
    fn new_escalation_id_unique() {
        let id1 = new_escalation_id();
        let id2 = new_escalation_id();
        assert_ne!(id1, id2);
    }
}
