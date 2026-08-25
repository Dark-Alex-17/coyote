use fmt::{Debug, Formatter};
use serde_json::{Value, json};
use std::fmt;

/// One background-task completion event, delivered to the context that
/// started the task by merging a `system_notifications` entry onto the last
/// tool result of a batch.
#[derive(Clone)]
pub struct SystemNotification {
    pub event: &'static str,
    pub id: String,
    pub tool_or_agent: String,
    pub status: &'static str,
    pub next_action: String,
}

impl SystemNotification {
    pub fn to_value(&self) -> Value {
        json!({
            "event": self.event,
            "id": self.id,
            "tool_or_agent": self.tool_or_agent,
            "status": self.status,
            "next_action": self.next_action,
        })
    }
}

pub fn job_notification(id: &str, tool: &str, success: bool) -> SystemNotification {
    SystemNotification {
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
    SystemNotification {
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
        assert_eq!(event.event, "job_failed");
        assert_eq!(event.status, "failed");
        assert_eq!(event.next_action, "job__collect --id job_a1b2 for output");
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
        assert_eq!(event.event, "agent_failed");
        assert_eq!(event.status, "failed");
        assert_eq!(
            event.next_action,
            "agent__collect --id agent_explore_a1b2 for output"
        );
    }

    #[test]
    fn drain_empties_queue_and_preserves_order() {
        let queue = NotificationQueue::new();
        queue.push(job_notification("job_1", "execute_command", true));
        queue.push(job_notification("job_2", "execute_command", false));

        let drained = queue.drain();

        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].id, "job_1");
        assert_eq!(drained[1].id, "job_2");
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn drain_on_empty_queue_is_a_noop() {
        let queue = NotificationQueue::default();
        assert!(queue.drain().is_empty());
    }
}
