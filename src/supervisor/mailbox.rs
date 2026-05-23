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
}
