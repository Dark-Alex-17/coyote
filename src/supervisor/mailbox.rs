use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

pub struct PeerEntry {
    pub label: String,
    pub inbox: Arc<Inbox>,
    pub finished: bool,
}

/// A pre-provisioned teammate identity: the routable peer id and the inbox
/// registered for it in a `PeerRegistry`.
pub type PeerAssignment = (String, Arc<Inbox>);

/// Messaging-only directory of concurrent sibling agents ("teammates").
/// Entries carry no lifecycle semantics: peers are never supervised,
/// checked, or collected. The registry only routes `agent__send_message`
/// and answers roster queries, and it dies when the fan-out's Arcs drop.
#[derive(Default)]
pub struct PeerRegistry {
    entries: parking_lot::RwLock<IndexMap<String, PeerEntry>>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, id: String, label: String, inbox: Arc<Inbox>) {
        let replaced = self.entries.write().insert(
            id.clone(),
            PeerEntry {
                label,
                inbox,
                finished: false,
            },
        );
        debug_assert!(
            replaced.is_none(),
            "peer id '{id}' registered twice; short-uuid collision or duplicate insert"
        );
    }

    pub fn get(&self, id: &str) -> Option<Arc<Inbox>> {
        self.entries
            .read()
            .get(id)
            .map(|entry| Arc::clone(&entry.inbox))
    }

    pub fn mark_finished(&self, id: &str) {
        if let Some(entry) = self.entries.write().get_mut(id) {
            entry.finished = true;
        }
    }

    pub fn is_finished(&self, id: &str) -> bool {
        self.entries
            .read()
            .get(id)
            .is_some_and(|entry| entry.finished)
    }

    pub fn roster(&self) -> Vec<(String, String)> {
        self.entries
            .read()
            .iter()
            .map(|(id, entry)| (id.clone(), entry.label.clone()))
            .collect()
    }
}

pub fn graph_agent_id(agent_name: &str) -> String {
    let short_uuid = &uuid::Uuid::new_v4().to_string()[..8];
    format!("graph_agent_{agent_name}_{short_uuid}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn text_envelope(from: &str, to: &str, content: &str) -> Envelope {
        Envelope {
            from: from.to_string(),
            to: to.to_string(),
            payload: EnvelopePayload::Text {
                content: content.to_string(),
            },
            timestamp: Utc::now(),
        }
    }

    fn task_completed_envelope(from: &str, to: &str) -> Envelope {
        Envelope {
            from: from.to_string(),
            to: to.to_string(),
            payload: EnvelopePayload::TaskCompleted {
                task_id: "t1".into(),
                summary: "done".into(),
            },
            timestamp: Utc::now(),
        }
    }

    fn shutdown_request_envelope(from: &str, to: &str) -> Envelope {
        Envelope {
            from: from.to_string(),
            to: to.to_string(),
            payload: EnvelopePayload::ShutdownRequest {
                reason: "all done".into(),
            },
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn inbox_new_is_empty() {
        let inbox = Inbox::new();
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn inbox_default_is_empty() {
        let inbox = Inbox::default();
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn deliver_and_drain() {
        let inbox = Inbox::new();
        inbox.deliver(text_envelope("a", "b", "hello"));
        let msgs = inbox.drain();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, "a");
    }

    #[test]
    fn drain_empties_inbox() {
        let inbox = Inbox::new();
        inbox.deliver(text_envelope("a", "b", "hello"));
        inbox.drain();
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn drain_orders_shutdown_before_task_before_text() {
        let inbox = Inbox::new();
        inbox.deliver(text_envelope("a", "b", "msg"));
        inbox.deliver(task_completed_envelope("a", "b"));
        inbox.deliver(shutdown_request_envelope("a", "b"));

        let msgs = inbox.drain();
        assert_eq!(msgs.len(), 3);
        assert!(matches!(
            msgs[0].payload,
            EnvelopePayload::ShutdownRequest { .. }
        ));
        assert!(matches!(
            msgs[1].payload,
            EnvelopePayload::TaskCompleted { .. }
        ));
        assert!(matches!(msgs[2].payload, EnvelopePayload::Text { .. }));
    }

    #[test]
    fn clone_preserves_messages() {
        let inbox = Inbox::new();
        inbox.deliver(text_envelope("a", "b", "hello"));
        let cloned = inbox.clone();
        let msgs = cloned.drain();
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn clone_is_independent() {
        let inbox = Inbox::new();
        inbox.deliver(text_envelope("a", "b", "hello"));
        let cloned = inbox.clone();
        inbox.deliver(text_envelope("a", "b", "second"));
        let original_msgs = inbox.drain();
        let cloned_msgs = cloned.drain();
        assert_eq!(original_msgs.len(), 2);
        assert_eq!(cloned_msgs.len(), 1);
    }

    #[test]
    fn multiple_deliveries() {
        let inbox = Inbox::new();
        for i in 0..5 {
            inbox.deliver(text_envelope("a", "b", &format!("msg {i}")));
        }
        assert_eq!(inbox.drain().len(), 5);
    }

    #[test]
    fn peer_registry_get_returns_inserted_inbox() {
        let registry = PeerRegistry::new();
        let inbox = Arc::new(Inbox::new());
        registry.insert("p1".into(), "branch[0]".into(), Arc::clone(&inbox));

        let found = registry.get("p1").expect("inserted peer should resolve");
        assert!(Arc::ptr_eq(&found, &inbox));
    }

    #[test]
    fn peer_registry_get_unknown_id_is_none() {
        let registry = PeerRegistry::new();
        registry.insert("p1".into(), "branch[0]".into(), Arc::new(Inbox::new()));

        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn peer_registry_mark_finished_flips_state() {
        let registry = PeerRegistry::new();
        registry.insert("p1".into(), "branch[0]".into(), Arc::new(Inbox::new()));

        assert!(!registry.is_finished("p1"));
        registry.mark_finished("p1");
        assert!(registry.is_finished("p1"));
    }

    #[test]
    fn peer_registry_mark_finished_unknown_id_is_noop() {
        let registry = PeerRegistry::new();
        registry.mark_finished("missing");
        assert!(!registry.is_finished("missing"));
    }

    #[test]
    fn peer_registry_roster_preserves_insertion_order() {
        let registry = PeerRegistry::new();
        registry.insert("p1".into(), "branch[0]".into(), Arc::new(Inbox::new()));
        registry.insert("p2".into(), "branch[1]".into(), Arc::new(Inbox::new()));

        assert_eq!(
            registry.roster(),
            vec![
                ("p1".to_string(), "branch[0]".to_string()),
                ("p2".to_string(), "branch[1]".to_string()),
            ]
        );
    }
}
