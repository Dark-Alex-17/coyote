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
/// tool result of a batch. `count` is how many identical events this entry
/// stands for; `fold_into` folds repeats of a mesh event into one.
#[derive(Clone, Debug)]
pub struct SystemNotification {
    pub event: &'static str,
    pub id: String,
    pub tool_or_agent: String,
    pub status: &'static str,
    pub next_action: String,
    pub channel: Channel,
    pub count: usize,
}

impl SystemNotification {
    /// The serialised shape names `count` only when it says something, so an entry
    /// standing for one event reads as it always has.
    pub fn to_value(&self) -> Value {
        let mut value = json!({
            "event": self.event,
            "id": self.id,
            "tool_or_agent": self.tool_or_agent,
            "status": self.status,
            "next_action": self.next_action,
            "channel": self.channel.as_str(),
        });
        if self.count > 1 {
            value["count"] = json!(self.count);
        }
        value
    }

    /// Whether `other` is the same event about the same thing, reported by the same
    /// tool with the same status and next step, so that one entry carrying a count says
    /// everything two would.
    fn same_event_as(&self, other: &Self) -> bool {
        self.channel == other.channel
            && self.event == other.event
            && self.id == other.id
            && self.tool_or_agent == other.tool_or_agent
            && self.status == other.status
            && self.next_action == other.next_action
    }
}

/// Folds `note` into the held entry it repeats, when there is one, and says whether it
/// did; a folded note is already counted and must not be pushed. Both buffers that hold
/// notes under the mesh cap, the notification queue and the slot's `ModelNotes`, fold
/// through here, so a repeat is counted the same way wherever it lands. Only mesh events
/// fold: a supervisor event names a handle the model collects once, so two of them are
/// never the same event.
pub(crate) fn fold_into<'a>(
    held: impl IntoIterator<Item = &'a mut SystemNotification>,
    note: &SystemNotification,
) -> bool {
    if note.channel != Channel::Mesh {
        return false;
    }
    match held.into_iter().find(|held| held.same_event_as(note)) {
        Some(held) => {
            held.count += note.count;
            true
        }
        None => false,
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
        count: 1,
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
        count: 1,
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
        count: 1,
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

    pub fn push(&self, notification: SystemNotification) {
        self.pending.lock().notifications.push(notification);
    }

    /// Queues a mesh event under the mesh cap: once `MESH_NOTIFICATION_QUEUE_CAPACITY`
    /// mesh events are held, the oldest of them is evicted and handed back so the caller
    /// can finish whatever that event was the model's only word about. Supervisor events
    /// are never evicted, reordered or summarised; one given here is simply pushed. A
    /// mesh event identical to one already held (same event, id, tool, status and next
    /// step: everything but `count`) is folded into that entry's `count` instead: it
    /// keeps its place in the order, and nothing is pushed or evicted. `peer_reply`
    /// events name their question's id, so two distinct questions never fold.
    pub fn push_mesh(&self, notification: SystemNotification) -> Option<SystemNotification> {
        let mut pending = self.pending.lock();
        let mut evicted = None;
        if fold_into(pending.notifications.iter_mut(), &notification) {
            return None;
        }
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
                pending.dropped_mesh += oldest.count;
                evicted = Some(oldest);
            }
        }
        pending.notifications.push(notification);
        evicted
    }

    /// Everything held, in the push order of each entry's first event, followed by one
    /// summary of the mesh events that were dropped to make room since the last drain.
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

    /// Seven identical pushes drain as one entry whose `count` says seven and whose JSON
    /// names it; an entry standing for one event serialises without a `count` key.
    #[test]
    fn identical_mesh_pushes_fold_into_one_entry_with_a_count() {
        let queue = NotificationQueue::new();
        let note = || {
            mesh_notification(
                "peer_message",
                "peer:abababab",
                "mesh",
                true,
                "mesh__check_inbox".into(),
            )
        };
        for _ in 0..7 {
            assert!(queue.push_mesh(note()).is_none());
        }

        let drained = queue.drain();

        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].count, 7);
        assert_eq!(drained[0].to_value()["count"], 7);
        assert!(
            note().to_value().get("count").is_none(),
            "a lone event has no count key"
        );
        assert!(queue.drain().is_empty());
    }

    /// Folding needs the whole key to match: two replies to different questions, and two
    /// events about the same peer that point at different next steps, stay apart. A
    /// repeat of the first entry folds into it where it stands rather than at the end.
    #[test]
    fn mesh_pushes_that_differ_in_id_or_next_action_stay_separate_entries() {
        let queue = NotificationQueue::new();
        let reply = |id: &str| {
            mesh_notification(
                "peer_reply",
                id,
                "mesh",
                true,
                format!("mesh__collect --id {id}"),
            )
        };
        let about_peer = |next_action: &str| {
            mesh_notification(
                "peer_message",
                "peer:abababab",
                "mesh",
                true,
                next_action.into(),
            )
        };
        queue.push_mesh(reply("q-1"));
        queue.push_mesh(reply("q-2"));
        queue.push_mesh(about_peer("mesh__check_inbox"));
        queue.push_mesh(about_peer("read it another way"));
        queue.push_mesh(reply("q-1"));

        let drained = queue.drain();

        let summary: Vec<(&str, usize)> = drained
            .iter()
            .map(|note| (note.id.as_str(), note.count))
            .collect();
        assert_eq!(
            summary,
            [
                ("q-1", 2),
                ("q-2", 1),
                ("peer:abababab", 1),
                ("peer:abababab", 1)
            ]
        );
        assert_eq!(drained[2].next_action, "mesh__check_inbox");
        assert_eq!(drained[3].next_action, "read it another way");
    }

    /// Supervisor events each name a distinct handle, so the fold never touches them even
    /// when two look alike.
    #[test]
    fn supervisor_pushes_never_fold() {
        let queue = NotificationQueue::new();
        queue.push_mesh(job_notification("job_1", "execute_command", true));
        queue.push_mesh(job_notification("job_1", "execute_command", true));

        let drained = queue.drain();

        assert_eq!(drained.len(), 2);
        assert!(drained.iter().all(|note| note.count == 1));
    }

    /// The fold key is the whole entry but `count`: the same event about the same peer
    /// reported by another tool, or with another status, is a separate entry. A folded
    /// entry that arrives already counting (a slot buffer folded it first) keeps its
    /// count on an empty queue and adds it to the entry it repeats on a full one, so a
    /// repeat is counted once however many buffers it crossed.
    #[test]
    fn the_fold_key_covers_tool_and_status_and_counts_add_up_across_buffers() {
        let queue = NotificationQueue::new();
        let about_peer = |tool: &str, success: bool| {
            mesh_notification(
                "peer_message",
                "peer:abababab",
                tool,
                success,
                "mesh__check_inbox".into(),
            )
        };
        queue.push_mesh(about_peer("mesh", true));
        queue.push_mesh(about_peer("mesh-slot", true));
        queue.push_mesh(about_peer("mesh", false));
        queue.push_mesh(about_peer("mesh", true));
        queue.push_mesh(about_peer("mesh-slot", true));
        queue.push_mesh(about_peer("mesh", true));

        let drained = queue.drain();
        let summary: Vec<(&str, &str, usize)> = drained
            .iter()
            .map(|note| (note.tool_or_agent.as_str(), note.status, note.count))
            .collect();
        assert_eq!(
            summary,
            [
                ("mesh", "success", 3),
                ("mesh-slot", "success", 2),
                ("mesh", "failed", 1)
            ],
            "{drained:?}"
        );

        let mut counted = about_peer("mesh", true);
        counted.count = 3;
        assert!(queue.push_mesh(counted).is_none());
        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0].count, 3,
            "an entry keeps the count it arrived with"
        );
        assert_eq!(drained[0].to_value()["count"], 3);

        let mut first = about_peer("mesh", true);
        first.count = 2;
        let mut second = about_peer("mesh", true);
        second.count = 3;
        assert!(queue.push_mesh(first).is_none());
        assert!(queue.push_mesh(second).is_none());
        assert!(queue.push_mesh(about_peer("mesh", true)).is_none());
        let drained = queue.drain();
        assert_eq!(drained.len(), 1, "{drained:?}");
        assert_eq!(
            drained[0].count, 6,
            "counts add: two already folded elsewhere, three more, and one fresh event"
        );
        assert!(queue.drain().is_empty());
    }

    /// The fold runs before the cap: with the queue full of distinct mesh events, a
    /// repeat of the oldest folds into it where it stands and evicts nothing, so the
    /// drain carries no dropped summary; and a flood of one identical event past the
    /// cap is one entry whose count is the whole flood.
    #[test]
    fn a_repeat_folds_into_a_full_queue_without_evicting_and_a_flood_is_one_entry() {
        let queue = NotificationQueue::new();
        for i in 0..MESH_NOTIFICATION_QUEUE_CAPACITY {
            assert!(queue.push_mesh(mesh_event(i)).is_none());
        }

        assert!(
            queue.push_mesh(mesh_event(0)).is_none(),
            "a repeat of a held event evicts nothing"
        );
        assert!(
            queue
                .push_mesh(mesh_event(MESH_NOTIFICATION_QUEUE_CAPACITY - 1))
                .is_none(),
            "a repeat of the newest held event evicts nothing either"
        );
        let drained = queue.drain();
        assert_eq!(
            drained.len(),
            MESH_NOTIFICATION_QUEUE_CAPACITY,
            "no summary"
        );
        assert!(
            drained
                .iter()
                .all(|note| note.event != MESH_EVENTS_DROPPED_EVENT),
            "nothing was dropped"
        );
        assert_eq!((drained[0].id.as_str(), drained[0].count), ("m0", 2));
        assert_eq!(drained[1].count, 1);
        assert_eq!(
            drained[MESH_NOTIFICATION_QUEUE_CAPACITY - 1].count,
            2,
            "{:?}",
            drained[MESH_NOTIFICATION_QUEUE_CAPACITY - 1]
        );

        let flood = MESH_NOTIFICATION_QUEUE_CAPACITY * 2 + 1;
        for _ in 0..flood {
            assert!(queue.push_mesh(mesh_event(7)).is_none());
        }
        let drained = queue.drain();
        assert_eq!(drained.len(), 1, "{drained:?}");
        assert_eq!(drained[0].count, flood);
        assert_eq!(drained[0].to_value()["count"], flood);
        assert!(queue.drain().is_empty());
    }

    /// Evicting a folded entry drops every event it counted: an entry folded to three
    /// and then pushed out by a full queue of distinct events makes the drain's summary
    /// say three were dropped, not one.
    #[test]
    fn evicting_a_folded_entry_counts_every_event_it_held_as_dropped() {
        let queue = NotificationQueue::new();
        for _ in 0..3 {
            assert!(queue.push_mesh(mesh_event(0)).is_none());
        }
        for i in 1..=MESH_NOTIFICATION_QUEUE_CAPACITY {
            let evicted = queue.push_mesh(mesh_event(i));
            if i == MESH_NOTIFICATION_QUEUE_CAPACITY {
                let evicted = evicted.expect("the full queue evicts the folded entry");
                assert_eq!((evicted.id.as_str(), evicted.count), ("m0", 3));
            } else {
                assert!(evicted.is_none(), "m{i} fits");
            }
        }

        let drained = queue.drain();
        assert_eq!(drained.len(), MESH_NOTIFICATION_QUEUE_CAPACITY + 1);
        let summary = drained.last().unwrap();
        assert_eq!(summary.event, MESH_EVENTS_DROPPED_EVENT);
        assert_eq!(
            summary.next_action,
            "3 older mesh events were dropped before this drain"
        );
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
            assert!(queue.push_mesh(mesh_event(i)).is_none());
        }
        for i in MESH_NOTIFICATION_QUEUE_CAPACITY..pushed {
            let evicted = queue.push_mesh(mesh_event(i)).expect("a full queue evicts");
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
                .push_mesh(job_notification("job_first", "execute_command", true))
                .is_none()
        );
        for i in 0..MESH_NOTIFICATION_QUEUE_CAPACITY + 1 {
            queue.push_mesh(mesh_event(i));
        }
        assert!(
            queue
                .push_mesh(job_notification("job_last", "execute_command", true))
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

    /// `push` is unbounded: a mesh event pushed that way never evicts, however many are
    /// held. Only `push_mesh` applies the cap.
    #[test]
    fn plain_push_never_evicts_a_mesh_event() {
        let queue = NotificationQueue::new();
        let pushed = MESH_NOTIFICATION_QUEUE_CAPACITY + 3;
        for i in 0..pushed {
            queue.push(mesh_event(i));
        }

        let drained = queue.drain();

        assert_eq!(drained.len(), pushed);
        assert_eq!(drained[0].id, "m0");
        assert!(
            drained
                .iter()
                .all(|event| event.event != MESH_EVENTS_DROPPED_EVENT)
        );
    }

    #[test]
    fn supervisor_entries_survive_mesh_eviction_unchanged_and_in_order() {
        const EXCESS: usize = 5;
        let queue = NotificationQueue::new();
        let pushed = MESH_NOTIFICATION_QUEUE_CAPACITY + EXCESS;
        let mut supervisor_entries = Vec::new();
        for i in 0..pushed {
            if i % 7 == 0 {
                let entry = if i % 2 == 0 {
                    job_notification(&format!("job_{i}"), "execute_command", true)
                } else {
                    agent_notification(&format!("agent_{i}"), "explore", false)
                };
                supervisor_entries.push(entry.to_value());
                queue.push(entry);
            }
            queue.push_mesh(mesh_event(i));
        }

        let drained = queue.drain();

        let survivors_supervisor: Vec<Value> = drained
            .iter()
            .filter(|event| event.channel == Channel::Supervisor)
            .map(SystemNotification::to_value)
            .collect();
        assert_eq!(survivors_supervisor, supervisor_entries);
        let survivors_mesh: Vec<&str> = drained
            .iter()
            .filter(|event| {
                event.channel == Channel::Mesh && event.event != MESH_EVENTS_DROPPED_EVENT
            })
            .map(|event| event.id.as_str())
            .collect();
        let expected: Vec<String> = (EXCESS..pushed).map(|i| format!("m{i}")).collect();
        assert_eq!(
            survivors_mesh, expected,
            "exactly the oldest mesh events are gone"
        );
        assert_eq!(
            drained.len(),
            supervisor_entries.len() + MESH_NOTIFICATION_QUEUE_CAPACITY + 1
        );
    }

    #[test]
    fn drain_appends_one_mesh_channel_drop_summary_with_the_exact_count() {
        const EVICTED: usize = 9;
        let queue = NotificationQueue::new();
        for i in 0..MESH_NOTIFICATION_QUEUE_CAPACITY + EVICTED {
            queue.push_mesh(mesh_event(i));
        }

        let drained = queue.drain();

        let summaries: Vec<&SystemNotification> = drained
            .iter()
            .filter(|event| event.event == MESH_EVENTS_DROPPED_EVENT)
            .collect();
        assert_eq!(summaries.len(), 1);
        let summary = summaries[0];
        assert_eq!(summary.channel, Channel::Mesh);
        assert_eq!(
            summary.next_action,
            format!("{EVICTED} older mesh events were dropped before this drain")
        );
        assert!(std::ptr::eq(summary, drained.last().unwrap()));

        queue.push_mesh(mesh_event(0));
        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert_ne!(
            drained[0].event, MESH_EVENTS_DROPPED_EVENT,
            "the count resets with the drain"
        );
    }
}
