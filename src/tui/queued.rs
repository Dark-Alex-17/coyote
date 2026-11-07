//! Lines typed while a turn runs. A prompt is offered to the model at its
//! next tool round through the context's notification queue; whatever the
//! model did not get, and every command, is submitted on its own once the
//! turn ends.

use super::DIM;
use crate::events::Event;
use crate::supervisor::notification::{NotificationQueue, SystemNotification};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

/// Entries the preview strip shows under its header; older ones are only
/// counted.
const PREVIEW_ENTRIES: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QueuedLine {
    pub(super) text: String,
    /// A command never goes to the model; it waits for the turn to end.
    pub(super) command: bool,
}

/// Offers `text` to the running turn's model.
pub(super) fn queue_user_message(queue: &NotificationQueue, text: &str) {
    queue.push(SystemNotification::UserMessage {
        text: text.to_string(),
    });
    crate::events::bus().publish(Event::QueuedInput {
        text: text.to_string(),
    });
}

/// Puts the prompts still waiting on the queue of the turn about to run.
pub(super) fn carry_over(queue: &NotificationQueue, queued: &[QueuedLine]) {
    for line in queued.iter().filter(|line| !line.command) {
        queue.push(SystemNotification::UserMessage {
            text: line.text.clone(),
        });
    }
}

/// Takes the user messages off `queue` and leaves the task notifications
/// on it for the next turn.
pub(super) fn take_user_messages(queue: &NotificationQueue) -> Vec<String> {
    queue
        .take_where(|notification| !matches!(notification, SystemNotification::UserMessage { .. }))
        .into_iter()
        .filter_map(|notification| match notification {
            SystemNotification::UserMessage { text } => Some(text),
            SystemNotification::Task { .. } => None,
        })
        .collect()
}

/// Takes the newest copy of `text` off `queue`; false when the driver had
/// already handed it to the model.
pub(super) fn unqueue_user_message(queue: &NotificationQueue, text: &str) -> bool {
    queue
        .take_last_where(|notification| match notification {
            SystemNotification::UserMessage { text: t } => t == text,
            SystemNotification::Task { .. } => false,
        })
        .is_some()
}

/// Drops the prompts the turn's driver handed to the model. Drains are
/// whole and in order, so the prompts still held appear in `unsent` in
/// order, after anything an interrupt dropped from the strip but not yet
/// from the queue; the match walks both from the newest end and skips
/// those stale leading entries. The commands, never pushed, all stay.
pub(super) fn reconcile_queue(queued: &mut Vec<QueuedLine>, unsent: &[String]) {
    let mut unsent = unsent.iter().rev().peekable();
    let mut kept: Vec<QueuedLine> = Vec::with_capacity(queued.len());
    for line in queued.drain(..).rev() {
        if line.command {
            kept.push(line);
            continue;
        }
        if unsent.peek().is_some_and(|text| **text == line.text) {
            unsent.next();
            kept.push(line);
        }
    }
    kept.reverse();
    *queued = kept;
}

/// The header plus the newest entries, up to the cap.
pub(super) fn preview_rows(queued: &[QueuedLine]) -> u16 {
    if queued.is_empty() {
        return 0;
    }
    1 + u16::try_from(queued.len().min(PREVIEW_ENTRIES)).unwrap_or(u16::MAX)
}

/// The strip above the composer: `pending: N`, then the newest entries'
/// first lines, all dim.
pub(super) struct QueuedPreview<'a>(pub(super) &'a [QueuedLine]);

impl Widget for QueuedPreview<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() || self.0.is_empty() {
            return;
        }
        let shown = self.0.len().min(PREVIEW_ENTRIES);
        let mut lines = vec![Line::from(Span::styled(
            format!("pending: {}", self.0.len()),
            DIM,
        ))];
        lines.extend(self.0[self.0.len() - shown..].iter().map(|line| {
            let first = line.text.lines().next().unwrap_or_default();
            Line::from(Span::styled(first.to_string(), DIM))
        }));
        Paragraph::new(lines).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(text: &str) -> QueuedLine {
        QueuedLine {
            text: text.to_string(),
            command: false,
        }
    }

    fn command(text: &str) -> QueuedLine {
        QueuedLine {
            text: text.to_string(),
            command: true,
        }
    }

    fn texts(queued: &[QueuedLine]) -> Vec<&str> {
        queued.iter().map(|line| line.text.as_str()).collect()
    }

    #[test]
    fn the_unsent_suffix_stays_and_the_consumed_prefix_goes() {
        let mut queued = vec![prompt("a"), prompt("b"), prompt("c")];
        reconcile_queue(&mut queued, &["c".to_string()]);
        assert_eq!(texts(&queued), ["c"]);
    }

    #[test]
    fn nothing_left_on_the_queue_means_everything_was_sent() {
        let mut queued = vec![prompt("a"), prompt("b"), prompt("c")];
        reconcile_queue(&mut queued, &[]);
        assert!(queued.is_empty());
    }

    #[test]
    fn stale_leading_entries_on_the_queue_do_not_hide_the_held_prompt() {
        let mut queued = vec![prompt("c")];
        reconcile_queue(
            &mut queued,
            &["a".to_string(), "b".to_string(), "c".to_string()],
        );
        assert_eq!(texts(&queued), ["c"]);
    }

    #[test]
    fn a_held_command_survives_in_its_place() {
        let mut queued = vec![prompt("a"), command(".cmd"), prompt("b")];
        reconcile_queue(&mut queued, &["b".to_string()]);
        assert_eq!(texts(&queued), [".cmd", "b"]);
        let mut queued = vec![prompt("a"), command(".cmd"), prompt("b")];
        reconcile_queue(&mut queued, &[]);
        assert_eq!(texts(&queued), [".cmd"]);
    }

    #[test]
    fn taking_user_messages_leaves_task_notifications_for_the_next_turn() {
        let queue = NotificationQueue::new();
        queue.push(SystemNotification::UserMessage { text: "a".into() });
        queue.push(crate::supervisor::notification::job_notification(
            "job-1", "tool", true,
        ));
        queue.push(SystemNotification::UserMessage { text: "b".into() });
        assert_eq!(take_user_messages(&queue), ["a", "b"]);
        let left = queue.drain();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id(), Some("job-1"));
    }

    #[test]
    fn unqueueing_takes_the_newest_copy_and_reports_a_gone_one() {
        let queue = NotificationQueue::new();
        carry_over(&queue, &[prompt("a"), command(".info"), prompt("b")]);
        assert!(unqueue_user_message(&queue, "b"));
        assert_eq!(take_user_messages(&queue), ["a"]);
        assert!(!unqueue_user_message(&queue, "b"));
    }

    #[test]
    fn the_preview_counts_everything_and_shows_the_newest_three() {
        let queued: Vec<QueuedLine> = ["one", "two\nmore", "three", "four"]
            .into_iter()
            .map(prompt)
            .collect();
        assert_eq!(preview_rows(&[]), 0);
        assert_eq!(preview_rows(&queued[..1]), 2);
        assert_eq!(preview_rows(&queued), 4);
        let area = Rect::new(0, 0, 12, 4);
        let mut buf = Buffer::empty(area);
        QueuedPreview(&queued).render(area, &mut buf);
        let rows: Vec<String> = (0..4)
            .map(|y| {
                (0..12)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        assert_eq!(rows, ["pending: 4", "two", "three", "four"]);
    }
}
