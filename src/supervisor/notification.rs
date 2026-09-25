use fmt::{Debug, Formatter};
use log::debug;
use serde_json::{Value, json};
use std::fmt;

/// Who vouches for an event when the queue is drained. `Supervisor` events name a handle
/// the model may still collect, so they are dropped once that handle is gone. `Mesh`
/// events are delivered unconditionally: the idle-time driver may push into a queue
/// whose supervisor has since been replaced or nulled, so filtering them by handle
/// would lose events that still matter. The trade-off is that a `next_action` on a mesh
/// event is best-effort and may name a handle that is already gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Supervisor,
    Mesh,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Supervisor => "supervisor",
            Channel::Mesh => "mesh",
        }
    }
}

/// One background-task completion event, delivered to the context that
/// started the task by merging a `system_notifications` entry onto the last
/// tool result of a batch.
#[derive(Clone, Debug)]
pub struct SystemNotification {
    pub event: &'static str,
    pub id: String,
    pub tool_or_agent: String,
    pub status: &'static str,
    pub next_action: String,
    pub channel: Channel,
}

impl SystemNotification {
    pub fn to_value(&self) -> Value {
        json!({
            "event": self.event,
            "id": self.id,
            "tool_or_agent": self.tool_or_agent,
            "status": self.status,
            "next_action": self.next_action,
            "channel": self.channel.as_str(),
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
        channel: Channel::Supervisor,
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
        channel: Channel::Supervisor,
    }
}

pub fn mesh_notification(
    event: &'static str,
    id: &str,
    tool_or_agent: &str,
    success: bool,
    next_action: String,
) -> SystemNotification {
    SystemNotification {
        event,
        id: id.to_string(),
        tool_or_agent: tool_or_agent.to_string(),
        status: if success { "success" } else { "failed" },
        next_action,
        channel: Channel::Mesh,
    }
}

/// Mesh events a queue holds before the oldest are dropped. Supervisor events are not
/// capped here: each one is backed by a registered handle, which bounds them already.
pub const MESH_NOTIFICATION_QUEUE_CAPACITY: usize = 64;
/// Event name of the one-line summary that stands in for mesh events dropped on the
/// way to the model, whichever bound dropped them.
pub const MESH_EVENTS_DROPPED_EVENT: &str = "mesh_events_dropped";

/// The summary that stands in for `dropped` mesh events. It reports a loss the model
/// cannot act on, so its status is informational rather than a failure. `reporter` names
/// the bound that dropped them; `where_lost` completes "N older mesh events were dropped".
pub fn mesh_events_dropped(dropped: usize, reporter: &str, where_lost: &str) -> SystemNotification {
    let text = if dropped == 1 {
        format!("1 older mesh event was dropped {where_lost}")
    } else {
        format!("{dropped} older mesh events were dropped {where_lost}")
    };
    mesh_notification(MESH_EVENTS_DROPPED_EVENT, "mesh", reporter, true, text)
}

/// Completion events for background work started by ONE context. Unlike the
/// escalation queue (shared, root-owned), every context owns a fresh queue:
/// a queue shared between parent and child would race their drains and
/// deliver one context's events into the other's transcript.
pub struct NotificationQueue {
    pending: parking_lot::Mutex<Pending>,
}

#[derive(Default)]
struct Pending {
    notifications: Vec<SystemNotification>,
    dropped_mesh: usize,
}

impl NotificationQueue {
    pub fn new() -> Self {
        Self {
            pending: parking_lot::Mutex::new(Pending::default()),
        }
    }

    /// Queues the event. Returns the mesh event evicted to make room, if any, so the
    /// caller can finish whatever that event was the model's only word about.
    pub fn push(&self, notification: SystemNotification) -> Option<SystemNotification> {
        let mut pending = self.pending.lock();
        let mut evicted = None;
        if notification.channel == Channel::Mesh {
            let mesh_held = pending
                .notifications
                .iter()
                .filter(|held| held.channel == Channel::Mesh)
                .count();
            if mesh_held >= MESH_NOTIFICATION_QUEUE_CAPACITY
                && let Some(oldest) = pending
                    .notifications
                    .iter()
                    .position(|held| held.channel == Channel::Mesh)
            {
                let oldest = pending.notifications.remove(oldest);
                debug!(
                    "Notification queue evicted mesh event '{}' at the cap of {MESH_NOTIFICATION_QUEUE_CAPACITY}",
                    oldest.id
                );
                evicted = Some(oldest);
                pending.dropped_mesh += 1;
            }
        }
        pending.notifications.push(notification);
        evicted
    }

    /// Everything held, in push order, followed by one summary of the mesh events that
    /// were dropped to make room since the last drain.
    pub fn drain(&self) -> Vec<SystemNotification> {
        let Pending {
            mut notifications,
            dropped_mesh,
        } = std::mem::take(&mut *self.pending.lock());
        if dropped_mesh > 0 {
            notifications.push(mesh_events_dropped(
                dropped_mesh,
                "notification-queue",
                "before this drain",
            ));
        }
        notifications
    }
}

impl Default for NotificationQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for NotificationQueue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let count = self.pending.lock().notifications.len();
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
                "channel": "supervisor",
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
                "channel": "supervisor",
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
    fn mesh_notification_shape() {
        let event = mesh_notification(
            "mesh_agent_completed",
            "agent_envoy_a1b2",
            "envoy",
            true,
            "agent__collect --id agent_envoy_a1b2 for output".into(),
        );
        assert_eq!(event.channel, Channel::Mesh);
        assert_eq!(
            event.to_value(),
            json!({
                "event": "mesh_agent_completed",
                "id": "agent_envoy_a1b2",
                "tool_or_agent": "envoy",
                "status": "success",
                "next_action": "agent__collect --id agent_envoy_a1b2 for output",
                "channel": "mesh",
            })
        );
        let failed = mesh_notification("mesh_agent_failed", "id", "envoy", false, String::new());
        assert_eq!(failed.status, "failed");
    }

    /// The serialised shape the model already reads keeps its keys in their order, and
    /// `channel` is appended after them. `json!` equality is order-blind, so the wire
    /// order is pinned through the object's key sequence.
    #[test]
    fn channel_is_the_last_key_and_the_others_keep_their_order() {
        let events = [
            job_notification("job_a1b2", "execute_command", true),
            agent_notification("agent_explore_a1b2", "explore", false),
            mesh_notification("mesh_message", "deadbeef", "peer", true, "read it".into()),
        ];
        for event in events {
            let value = event.to_value();
            let keys: Vec<&str> = value
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                [
                    "event",
                    "id",
                    "tool_or_agent",
                    "status",
                    "next_action",
                    "channel"
                ],
                "{}",
                event.event
            );
            assert!(
                value
                    .to_string()
                    .ends_with(&format!("\"channel\":\"{}\"}}", event.channel.as_str()))
            );
        }
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

    fn mesh_event(i: usize) -> SystemNotification {
        mesh_notification(
            "mesh_message",
            &format!("m{i}"),
            "peer",
            true,
            "read it".into(),
        )
    }

    #[test]
    fn mesh_pushes_past_capacity_drop_the_oldest_and_summarise_on_drain() {
        let queue = NotificationQueue::new();
        let pushed = MESH_NOTIFICATION_QUEUE_CAPACITY + 6;
        for i in 0..MESH_NOTIFICATION_QUEUE_CAPACITY {
            assert!(queue.push(mesh_event(i)).is_none());
        }
        for i in MESH_NOTIFICATION_QUEUE_CAPACITY..pushed {
            let evicted = queue.push(mesh_event(i)).expect("a full queue evicts");
            assert_eq!(
                evicted.id,
                format!("m{}", i - MESH_NOTIFICATION_QUEUE_CAPACITY)
            );
        }

        let drained = queue.drain();

        assert_eq!(drained.len(), MESH_NOTIFICATION_QUEUE_CAPACITY + 1);
        let survivors: Vec<&str> = drained[..MESH_NOTIFICATION_QUEUE_CAPACITY]
            .iter()
            .map(|event| event.id.as_str())
            .collect();
        let expected: Vec<String> = (6..pushed).map(|i| format!("m{i}")).collect();
        assert_eq!(survivors, expected);
        let summary = drained.last().unwrap();
        assert_eq!(summary.event, MESH_EVENTS_DROPPED_EVENT);
        assert_eq!(summary.channel, Channel::Mesh);
        assert_eq!(summary.tool_or_agent, "notification-queue");
        assert_eq!(summary.status, "success", "a drop summary is not a failure");
        assert_eq!(
            summary.next_action,
            "6 older mesh events were dropped before this drain"
        );
        assert!(
            queue.drain().is_empty(),
            "the drop count resets with the drain"
        );
    }

    #[test]
    fn supervisor_events_are_never_dropped_by_the_mesh_cap() {
        let queue = NotificationQueue::new();
        assert!(
            queue
                .push(job_notification("job_first", "execute_command", true))
                .is_none()
        );
        for i in 0..MESH_NOTIFICATION_QUEUE_CAPACITY + 1 {
            queue.push(mesh_event(i));
        }
        assert!(
            queue
                .push(job_notification("job_last", "execute_command", true))
                .is_none(),
            "a supervisor event evicts nothing even from a full mesh queue"
        );

        let drained = queue.drain();

        assert_eq!(drained.len(), MESH_NOTIFICATION_QUEUE_CAPACITY + 3);
        assert_eq!(drained[0].id, "job_first");
        assert_eq!(drained[1].id, "m1", "m0 was the oldest mesh event");
        assert_eq!(drained[drained.len() - 2].id, "job_last");
        assert_eq!(drained.last().unwrap().event, MESH_EVENTS_DROPPED_EVENT);
        assert_eq!(
            drained.last().unwrap().next_action,
            "1 older mesh event was dropped before this drain"
        );
    }
}
