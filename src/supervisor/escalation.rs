use fmt::{Debug, Formatter};
use log::debug;
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

/// A question waiting in the queue, without the channel its answer goes
/// back on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEscalation {
    pub id: String,
    pub from_agent_id: String,
    pub from_agent_name: String,
    pub question: String,
    pub options: Option<Vec<String>>,
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

    /// Withdraws every question asked by one of `agent_ids`, for askers
    /// that are being cancelled and will never read an answer.
    pub fn remove_for_agents(&self, agent_ids: &[&str]) {
        self.pending.lock().retain(|_, r| {
            let keep = !agent_ids.contains(&r.from_agent_id.as_str());
            if !keep {
                debug!(
                    "Withdrew escalation '{}' of cancelled agent '{}'",
                    r.id, r.from_agent_id
                );
            }
            keep
        });
    }

    /// The requests still waiting for an answer; drops the ones whose
    /// asker stopped waiting.
    pub fn pending(&self) -> Vec<PendingEscalation> {
        self.prune_closed()
            .values()
            .map(|r| PendingEscalation {
                id: r.id.clone(),
                from_agent_id: r.from_agent_id.clone(),
                from_agent_name: r.from_agent_name.clone(),
                question: r.question.clone(),
                options: r.options.clone(),
            })
            .collect()
    }

    pub fn pending_summary(&self) -> Vec<Value> {
        self.pending()
            .into_iter()
            .map(|r| {
                let mut entry = json!({
                    "escalation_id": r.id,
                    "from_agent_id": r.from_agent_id,
                    "from_agent_name": r.from_agent_name,
                    "question": r.question,
                });
                if let Some(options) = r.options {
                    entry["options"] = json!(options);
                }
                entry
            })
            .collect()
    }

    /// Whether any request still waits for an answer; drops the ones
    /// whose asker stopped waiting.
    pub fn has_pending(&self) -> bool {
        !self.prune_closed().is_empty()
    }

    /// Drops the requests whose asker has stopped waiting, so no caller
    /// offers a question nobody can receive the answer to, and hands back
    /// the locked map of those left.
    fn prune_closed(&self) -> parking_lot::MutexGuard<'_, HashMap<String, EscalationRequest>> {
        let mut pending = self.pending.lock();
        pending.retain(|_, r| {
            let open = !r.reply_tx.is_closed();
            if !open {
                debug!(
                    "Dropped escalation '{}': agent '{}' stopped waiting",
                    r.id, r.from_agent_id
                );
            }
            open
        });
        pending
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
    fn pending_lists_each_request_and_the_summary_says_the_same() {
        let queue = EscalationQueue::new();
        let (req, _rx) = make_request("esc_1", "agent_x", "What to do?");
        queue.submit(req);
        let (tx, _options_rx) = oneshot::channel();
        queue.submit(EscalationRequest {
            id: "esc_2".into(),
            from_agent_id: "a".into(),
            from_agent_name: "agent".into(),
            question: "[select] Pick one".into(),
            options: Some(vec!["A".into(), "B".into()]),
            reply_tx: tx,
        });
        let mut pending = queue.pending();
        pending.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            pending[1],
            PendingEscalation {
                id: "esc_2".into(),
                from_agent_id: "a".into(),
                from_agent_name: "agent".into(),
                question: "[select] Pick one".into(),
                options: Some(vec!["A".into(), "B".into()]),
            }
        );
        assert_eq!(pending[0].options, None);

        let mut summary = queue.pending_summary();
        summary.sort_by_key(|entry| entry["escalation_id"].as_str().map(str::to_string));
        assert_eq!(
            summary,
            [
                json!({
                    "escalation_id": "esc_1",
                    "from_agent_id": "agent_x",
                    "from_agent_name": pending[0].from_agent_name,
                    "question": "What to do?",
                }),
                json!({
                    "escalation_id": "esc_2",
                    "from_agent_id": "a",
                    "from_agent_name": "agent",
                    "question": "[select] Pick one",
                    "options": ["A", "B"],
                }),
            ]
        );
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
    fn a_request_whose_asker_is_gone_is_no_longer_pending() {
        let queue = EscalationQueue::new();
        let (gone, gone_rx) = make_request("esc_gone", "a", "Anyone there?");
        queue.submit(gone);
        let (waiting, _waiting_rx) = make_request("esc_waiting", "b", "Still here?");
        queue.submit(waiting);
        drop(gone_rx);

        let ids: Vec<String> = queue.pending().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["esc_waiting"]);
        assert_eq!(queue.pending_summary().len(), 1);
        assert!(queue.take("esc_gone").is_none());
        assert!(queue.has_pending());
    }

    #[test]
    fn remove_for_agents_withdraws_only_the_named_askers_questions() {
        let queue = EscalationQueue::new();
        let (first, _first_rx) = make_request("esc_c1", "agent_c", "First?");
        queue.submit(first);
        let (second, _second_rx) = make_request("esc_c2", "agent_c", "Second?");
        queue.submit(second);
        let (other, _other_rx) = make_request("esc_o", "agent_o", "Other?");
        queue.submit(other);

        queue.remove_for_agents(&["agent_c", "agent_gone"]);

        let ids: Vec<String> = queue.pending().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["esc_o"]);
    }

    #[test]
    fn has_pending_drops_a_request_whose_asker_is_gone() {
        let queue = EscalationQueue::new();
        let (req, rx) = make_request("esc_1", "a", "question");
        queue.submit(req);
        drop(rx);
        assert!(!queue.has_pending());
        assert!(queue.pending.lock().is_empty());
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
