use fmt::{Debug, Formatter};
use serde_json::{Value, json};
use std::fmt;

pub(crate) const USER_MESSAGE_EVENT: &str = "user_message";

/// One entry of the `system_notifications` channel, delivered to the context
/// that owns the queue by merging onto the last tool result of a batch.
#[derive(Clone)]
pub enum SystemNotification {
    /// A background task reached a terminal state.
    Task {
        event: &'static str,
        id: String,
        tool_or_agent: String,
        status: &'static str,
        next_action: String,
    },
    /// Text the user typed while a turn was running, handed to the model at
    /// the next tool-round boundary.
    #[allow(dead_code)] // TASK-315
    UserMessage { text: String },
}

impl SystemNotification {
    pub fn to_value(&self) -> Value {
        match self {
            Self::Task {
                event,
                id,
                tool_or_agent,
                status,
                next_action,
            } => json!({
                "event": event,
                "id": id,
                "tool_or_agent": tool_or_agent,
                "status": status,
                "next_action": next_action,
            }),
            Self::UserMessage { text } => json!({
                "event": USER_MESSAGE_EVENT,
                "text": text,
            }),
        }
    }

    #[cfg(test)]
    pub fn event(&self) -> &'static str {
        match self {
            Self::Task { event, .. } => event,
            Self::UserMessage { .. } => USER_MESSAGE_EVENT,
        }
    }

    /// The task id a notification points the model at; a user message has none.
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Task { id, .. } => Some(id),
            Self::UserMessage { .. } => None,
        }
    }
}

pub fn job_notification(id: &str, tool: &str, success: bool) -> SystemNotification {
    SystemNotification::Task {
        event: if success {
            "job_completed"
        } else {
            "job_failed"
        },
        id: id.to_string(),
        tool_or_agent: tool.to_string(),
        status: if success { "success" } else { "failed" },
        next_action: format!("job__collect --id {id} for output"),
    }
}

pub fn agent_notification(id: &str, agent_name: &str, success: bool) -> SystemNotification {
    SystemNotification::Task {
        event: if success {
            "agent_completed"
        } else {
            "agent_failed"
        },
        id: id.to_string(),
        tool_or_agent: agent_name.to_string(),
        status: if success { "success" } else { "failed" },
        next_action: format!("agent__collect --id {id} for output"),
    }
}

/// Completion events for background work started by ONE context. Unlike the
/// escalation queue (shared, root-owned), every context owns a fresh queue:
/// a queue shared between parent and child would race their drains and
/// deliver one context's events into the other's transcript.
pub struct NotificationQueue {
    pending: parking_lot::Mutex<Vec<SystemNotification>>,
}

impl NotificationQueue {
    pub fn new() -> Self {
        Self {
            pending: parking_lot::Mutex::new(Vec::new()),
        }
    }

    pub fn push(&self, notification: SystemNotification) {
        self.pending.lock().push(notification);
    }

    pub fn drain(&self) -> Vec<SystemNotification> {
        std::mem::take(&mut *self.pending.lock())
    }
}

impl Default for NotificationQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for NotificationQueue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let count = self.pending.lock().len();
        f.debug_struct("NotificationQueue")
            .field("pending_count", &count)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_notification_success_shape() {
        let event = job_notification("job_a1b2", "execute_command", true);
        assert_eq!(
            event.to_value(),
            json!({
                "event": "job_completed",
                "id": "job_a1b2",
                "tool_or_agent": "execute_command",
                "status": "success",
                "next_action": "job__collect --id job_a1b2 for output",
            })
        );
    }

    #[test]
    fn job_notification_failure_shape() {
        let event = job_notification("job_a1b2", "execute_command", false);
        assert_eq!(event.event(), "job_failed");
        assert_eq!(event.to_value()["status"], "failed");
        assert_eq!(
            event.to_value()["next_action"],
            "job__collect --id job_a1b2 for output"
        );
    }

    #[test]
    fn agent_notification_success_shape() {
        let event = agent_notification("agent_explore_a1b2", "explore", true);
        assert_eq!(
            event.to_value(),
            json!({
                "event": "agent_completed",
                "id": "agent_explore_a1b2",
                "tool_or_agent": "explore",
                "status": "success",
                "next_action": "agent__collect --id agent_explore_a1b2 for output",
            })
        );
    }

    #[test]
    fn agent_notification_failure_shape() {
        let event = agent_notification("agent_explore_a1b2", "explore", false);
        assert_eq!(event.event(), "agent_failed");
        assert_eq!(event.to_value()["status"], "failed");
        assert_eq!(
            event.to_value()["next_action"],
            "agent__collect --id agent_explore_a1b2 for output"
        );
    }

    #[test]
    fn user_message_shape() {
        let event = SystemNotification::UserMessage {
            text: "keep going".to_string(),
        };
        assert_eq!(event.event(), "user_message");
        assert_eq!(event.id(), None);
        assert_eq!(
            serde_json::to_string(&event.to_value()).unwrap(),
            r#"{"event":"user_message","text":"keep going"}"#
        );
    }

    #[test]
    fn drain_empties_queue_and_preserves_order() {
        let queue = NotificationQueue::new();
        queue.push(job_notification("job_1", "execute_command", true));
        queue.push(job_notification("job_2", "execute_command", false));

        let drained = queue.drain();

        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].id(), Some("job_1"));
        assert_eq!(drained[1].id(), Some("job_2"));
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn drain_on_empty_queue_is_a_noop() {
        let queue = NotificationQueue::default();
        assert!(queue.drain().is_empty());
    }
}
