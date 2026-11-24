//! The thread pop-up's tree: the main thread at the top and every child
//! session under the session that spawned it, at any depth. The tree is
//! built from the child-session sidecars and the children the UI saw
//! start, flattened through the folds the pager keeps, and drawn a row
//! per thread.

use super::cells::{COLLAPSED_ARROW, EXPANDED_ARROW, elapsed_text};
use super::pager::keep_tail;
use super::render;
use super::sink::AgentOutcome;
use super::theme::Theme;
use crate::config::child_sessions::{ChildMeta, EffectiveStatus};
use chrono::{DateTime, FixedOffset, Utc};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::collections::{HashMap, HashSet};
use unicode_width::UnicodeWidthStr;

/// The top of the tree and the first crumb of a child's path.
pub(super) const MAIN_THREAD: &str = "main";
/// The fold key of the main thread; a session id is never empty.
const MAIN_KEY: &str = "";
/// The fewest columns a cut session id is shown in; with less room it
/// is left out.
const ID_MIN: usize = 8;
const GAP: &str = "  ";

fn status_word(status: EffectiveStatus) -> &'static str {
    match status {
        EffectiveStatus::Running => "running",
        EffectiveStatus::Completed => "completed",
        EffectiveStatus::Failed => "failed",
        EffectiveStatus::Cancelled => "cancelled",
        EffectiveStatus::Crashed => "crashed",
    }
}

fn status_style(status: EffectiveStatus, theme: &Theme) -> Style {
    match status {
        EffectiveStatus::Running => theme.thread_running,
        EffectiveStatus::Completed => theme.thread_completed,
        EffectiveStatus::Failed | EffectiveStatus::Cancelled | EffectiveStatus::Crashed => {
            theme.thread_failed
        }
    }
}

/// One thread of the tree: the main thread, or a child session with the
/// session that spawned it and when it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ThreadNode {
    /// None for the main thread.
    pub(super) session_id: Option<String>,
    pub(super) agent: String,
    status: Option<EffectiveStatus>,
    /// The spawner's session; None for a child of the root turn.
    parent_session: Option<String>,
    started: Option<DateTime<Utc>>,
    /// When a finished child ended, if that is known.
    ended: Option<DateTime<Utc>>,
}

impl ThreadNode {
    fn main() -> Self {
        Self {
            session_id: None,
            agent: MAIN_THREAD.to_string(),
            status: None,
            parent_session: None,
            started: None,
            ended: None,
        }
    }

    /// A child the child-session root lists, with its status reconciled
    /// against its process. It ended when its last run did, or failing
    /// that when its sidecar was last written.
    pub(super) fn child(meta: &ChildMeta, status: EffectiveStatus) -> Self {
        let ended = (status != EffectiveStatus::Running).then(|| {
            meta.runs
                .last()
                .and_then(|run| run.finished_at)
                .unwrap_or(meta.updated_at)
        });
        Self {
            session_id: Some(meta.session_id.clone()),
            agent: meta.agent.clone(),
            status: Some(status),
            parent_session: meta.spawner_session_id.clone(),
            started: Some(meta.created_at),
            ended,
        }
    }

    /// A child the UI saw start, which the child-session root does not
    /// list (yet), with how it ended: None while it runs. When it ended
    /// is not known, so a finished one shows no duration.
    pub(super) fn live(
        session_id: &str,
        agent: &str,
        outcome: Option<&AgentOutcome>,
        parent_session: Option<&str>,
        started: DateTime<Utc>,
    ) -> Self {
        let status = match outcome {
            None => EffectiveStatus::Running,
            Some(AgentOutcome::Completed) => EffectiveStatus::Completed,
            Some(AgentOutcome::Failed(_)) => EffectiveStatus::Failed,
            Some(AgentOutcome::Interrupted) => EffectiveStatus::Cancelled,
        };
        Self {
            session_id: Some(session_id.to_string()),
            agent: agent.to_string(),
            status: Some(status),
            parent_session: parent_session.map(str::to_string),
            started: Some(started),
            ended: None,
        }
    }

    fn key(&self) -> &str {
        self.session_id.as_deref().unwrap_or(MAIN_KEY)
    }

    /// The spawn time, then the time it has run or ran for.
    fn times(&self, now: DateTime<Utc>, offset: FixedOffset) -> Vec<String> {
        let Some(started) = self.started else {
            return Vec::new();
        };
        let mut times = vec![
            started
                .with_timezone(&offset)
                .format("%H:%M:%S")
                .to_string(),
        ];
        let end = match self.status {
            Some(EffectiveStatus::Running) => Some(now),
            _ => self.ended,
        };
        if let Some(end) = end {
            let ran = (end - started).to_std().unwrap_or_default();
            times.push(elapsed_text(ran));
        }
        times
    }
}

/// The threads as a tree, the main thread at index 0. A child whose
/// spawner is not among them, or whose spawner links lead back to it,
/// hangs under the main thread. Siblings list the running first, then
/// by spawn time, oldest first.
pub(super) struct ThreadTree {
    nodes: Vec<ThreadNode>,
    parents: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
}

impl ThreadTree {
    /// The tree of `threads`; a session listed twice keeps its first
    /// entry.
    pub(super) fn new(threads: Vec<ThreadNode>) -> Self {
        let mut nodes = vec![ThreadNode::main()];
        let mut index: HashMap<String, usize> = HashMap::new();
        for thread in threads {
            let Some(session_id) = thread.session_id.clone() else {
                continue;
            };
            if index.contains_key(&session_id) {
                continue;
            }
            index.insert(session_id, nodes.len());
            nodes.push(thread);
        }
        let linked: Vec<usize> = nodes
            .iter()
            .map(|node| {
                node.parent_session
                    .as_deref()
                    .and_then(|parent| index.get(parent).copied())
                    .unwrap_or(0)
            })
            .collect();
        let mut parents = vec![None; nodes.len()];
        for at in 1..nodes.len() {
            let mut up = linked[at];
            let mut steps = 0;
            while up != 0 && up != at && steps < nodes.len() {
                up = linked[up];
                steps += 1;
            }
            parents[at] = Some(if up == 0 { linked[at] } else { 0 });
        }
        let mut children = vec![Vec::new(); nodes.len()];
        for (at, parent) in parents.iter().enumerate() {
            if let Some(parent) = parent {
                children[*parent].push(at);
            }
        }
        for siblings in &mut children {
            siblings.sort_by_key(|&at| {
                let node = &nodes[at];
                (
                    node.status != Some(EffectiveStatus::Running),
                    node.started,
                    node.session_id.clone(),
                )
            });
        }
        Self {
            nodes,
            parents,
            children,
        }
    }

    /// How many threads there are, the main thread among them.
    pub(super) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(super) fn node(&self, at: usize) -> &ThreadNode {
        &self.nodes[at]
    }

    pub(super) fn key(&self, at: usize) -> &str {
        self.nodes[at].key()
    }

    pub(super) fn find(&self, key: &str) -> Option<usize> {
        self.nodes.iter().position(|node| node.key() == key)
    }

    pub(super) fn parent(&self, at: usize) -> Option<usize> {
        self.parents[at]
    }

    pub(super) fn first_child(&self, at: usize) -> Option<usize> {
        self.children[at].first().copied()
    }

    pub(super) fn depth(&self, at: usize) -> usize {
        std::iter::successors(self.parents[at], |&up| self.parents[up]).count()
    }

    /// The threads on show in order, each under its parent; a folded
    /// thread hides everything below it.
    pub(super) fn visible(&self, folds: &HashSet<String>) -> Vec<usize> {
        let mut shown = Vec::with_capacity(self.nodes.len());
        let mut stack = vec![0];
        while let Some(at) = stack.pop() {
            shown.push(at);
            if !folds.contains(self.key(at)) {
                stack.extend(self.children[at].iter().rev());
            }
        }
        shown
    }

    /// The thread `key` names, or its nearest ancestor on show when a
    /// fold hides it.
    pub(super) fn shown_at(&self, key: &str, folds: &HashSet<String>) -> Option<usize> {
        let at = self.find(key)?;
        let hidden_by = std::iter::successors(self.parents[at], |&up| self.parents[up])
            .filter(|&up| folds.contains(self.key(up)))
            .last();
        Some(hidden_by.unwrap_or(at))
    }

    /// The names of the threads above `session_id`, the main thread
    /// first; the main thread alone for a session not in the tree.
    pub(super) fn crumbs(&self, session_id: &str) -> Vec<String> {
        let Some(at) = self.find(session_id).filter(|&at| at != 0) else {
            return vec![MAIN_THREAD.to_string()];
        };
        let mut crumbs: Vec<String> =
            std::iter::successors(self.parents[at], |&up| self.parents[up])
                .map(|up| self.nodes[up].agent.clone())
                .collect();
        crumbs.reverse();
        crumbs
    }

    /// Where the fold arrow of row `at` starts, in columns from the left.
    pub(super) fn arrow_column(&self, at: usize) -> usize {
        2 * self.depth(at)
    }

    pub(super) fn has_children(&self, at: usize) -> bool {
        !self.children[at].is_empty()
    }

    /// Thread `at` as a row `width` columns wide: indent, fold arrow,
    /// name and state in the state's colour, the times and the session
    /// id. Short of room the id loses its start and then goes, and then
    /// the spawn time and the run time go; the arrow, name and state
    /// stay.
    pub(super) fn line(
        &self,
        at: usize,
        folds: &HashSet<String>,
        width: usize,
        now: DateTime<Utc>,
        offset: FixedOffset,
        theme: &Theme,
    ) -> Line<'static> {
        let node = &self.nodes[at];
        let arrow = match (self.has_children(at), folds.contains(node.key())) {
            (false, _) => GAP,
            (true, true) => COLLAPSED_ARROW,
            (true, false) => EXPANDED_ARROW,
        };
        let style = node
            .status
            .map_or(theme.heading, |status| status_style(status, theme));
        let name = render::plain_text(&node.agent);
        let mut spans = vec![
            Span::raw(" ".repeat(self.arrow_column(at))),
            Span::raw(arrow),
            Span::styled(name, style),
        ];
        if let Some(status) = node.status {
            spans.push(Span::raw(GAP));
            spans.push(Span::styled(status_word(status), style));
        }
        let used: usize = spans.iter().map(|span| span.content.width()).sum();
        let room = width.saturating_sub(used);
        let mut times = node.times(now, offset);
        let id = node.session_id.as_deref().map(render::plain_text);
        let times_width =
            |times: &[String]| -> usize { times.iter().map(|time| GAP.len() + time.width()).sum() };
        let id_room = room.saturating_sub(times_width(&times) + GAP.len());
        let id = id.filter(|id| id.width() <= id_room || id_room >= ID_MIN);
        if id.is_none() {
            while times_width(&times) > room {
                times.remove(0);
            }
        }
        for time in times {
            spans.push(Span::raw(GAP));
            spans.push(Span::raw(time));
        }
        if let Some(id) = id {
            spans.push(Span::raw(GAP));
            spans.push(Span::styled(keep_tail(&id, id_room), theme.faded));
        }
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 2, 3, minute, 5).unwrap()
    }

    fn thread(session_id: &str, parent: Option<&str>, running: bool, minute: u32) -> ThreadNode {
        ThreadNode::live(
            session_id,
            session_id,
            (!running).then_some(&AgentOutcome::Completed),
            parent,
            at(minute),
        )
    }

    fn keys(tree: &ThreadTree, rows: &[usize]) -> Vec<String> {
        rows.iter().map(|&at| tree.key(at).to_string()).collect()
    }

    /// Children sit under their spawner at any depth, the running ones
    /// first and each group oldest first; an unknown spawner or a loop
    /// of spawners puts a child under the main thread.
    #[test]
    fn threads_nest_under_their_spawner_running_first_then_oldest_first() {
        let tree = ThreadTree::new(vec![
            thread("done-old", None, false, 1),
            thread("run-new", None, true, 9),
            thread("run-old", None, true, 2),
            thread("deep", Some("run-new"), false, 10),
            thread("deeper", Some("deep"), true, 11),
            thread("orphan", Some("gone"), false, 3),
            thread("loop-a", Some("loop-b"), false, 4),
            thread("loop-b", Some("loop-a"), false, 5),
            thread("run-old", None, false, 0),
        ]);
        let shown = tree.visible(&HashSet::new());
        assert_eq!(
            keys(&tree, &shown),
            [
                "", "run-old", "run-new", "deep", "deeper", "done-old", "orphan", "loop-a",
                "loop-b"
            ]
        );
        let deeper = tree.find("deeper").unwrap();
        assert_eq!(tree.depth(deeper), 3);
        assert_eq!(tree.crumbs("deeper"), ["main", "run-new", "deep"]);
        assert_eq!(tree.crumbs("run-old"), ["main"]);
        assert_eq!(tree.crumbs("unknown"), ["main"]);
    }

    /// A fold hides what is below it, and a hidden thread is found at
    /// the folded ancestor.
    #[test]
    fn a_fold_hides_the_subtree_and_shown_at_finds_the_folded_ancestor() {
        let tree = ThreadTree::new(vec![
            thread("a", None, true, 1),
            thread("b", Some("a"), true, 2),
            thread("c", Some("b"), true, 3),
        ]);
        let folds: HashSet<String> = ["a".to_string()].into();
        assert_eq!(keys(&tree, &tree.visible(&folds)), ["", "a"]);
        assert_eq!(tree.shown_at("c", &folds), tree.find("a"));
        assert_eq!(tree.shown_at("c", &HashSet::new()), tree.find("c"));
        let all: HashSet<String> = [MAIN_KEY.to_string()].into();
        assert_eq!(keys(&tree, &tree.visible(&all)), [""]);
    }

    /// Short of room a row cuts the session id from its start first,
    /// then drops it, then drops the spawn time and the run time; the
    /// name and the state always stay.
    #[test]
    fn a_narrow_row_cuts_the_id_then_the_times() {
        let tree = ThreadTree::new(vec![thread("reviewer_20260102_0a1b2c", None, true, 4)]);
        let theme = Theme::fallback(true);
        let utc = FixedOffset::east_opt(0).unwrap();
        let row = |width: usize| {
            tree.line(1, &HashSet::new(), width, at(5), utc, &theme)
                .to_string()
        };
        let full =
            "    reviewer_20260102_0a1b2c  running  03:04:05  1m 0s  reviewer_20260102_0a1b2c";
        assert_eq!(row(100), full);
        assert_eq!(
            row(70),
            "    reviewer_20260102_0a1b2c  running  03:04:05  1m 0s  ...0102_0a1b2c"
        );
        assert_eq!(
            row(60),
            "    reviewer_20260102_0a1b2c  running  03:04:05  1m 0s"
        );
        assert_eq!(row(50), "    reviewer_20260102_0a1b2c  running  1m 0s");
        assert_eq!(row(10), "    reviewer_20260102_0a1b2c  running");
        let offset = FixedOffset::east_opt(2 * 3600).unwrap();
        assert!(
            tree.line(1, &HashSet::new(), 100, at(5), offset, &theme)
                .to_string()
                .contains("05:04:05")
        );
    }
}
