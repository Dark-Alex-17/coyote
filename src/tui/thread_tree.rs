//! The tree the thread and tasks pop-ups show: items nested under the
//! item that started them, at any depth, flattened through the folds the
//! pop-up keeps and drawn a row per item. The thread pop-up's items are
//! the main thread at the top and every child session under the session
//! that spawned it.

use super::cells::{COLLAPSED_ARROW, EXPANDED_ARROW, elapsed_text};
use super::help::MARGIN;
use super::pager::keep_tail;
use super::render;
use super::sink::AgentOutcome;
use super::theme::Theme;
use crate::config::child_sessions::{ChildMeta, EffectiveStatus};
use chrono::{DateTime, FixedOffset, Utc};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::collections::{HashMap, HashSet};
use unicode_width::UnicodeWidthStr;

/// The top of the tree and the first crumb of a child's path.
pub(super) const MAIN_THREAD: &str = "main";
/// The fold key of the main thread; a session id is never empty.
const MAIN_KEY: &str = "";
/// The fewest columns a cut id is shown in; with less room it is left
/// out.
const ID_MIN: usize = 8;
const GAP: &str = "  ";
/// A tree pop-up's width: three quarters of the frame within these
/// bounds, and never past a column's margin on each side.
const POPUP_MIN_WIDTH: u16 = 60;
const POPUP_MAX_WIDTH: u16 = 100;

/// A tree pop-up `rows` rows tall plus its border, centred over `body`,
/// which spans the frame's width, and kept a cell clear of its edges.
pub(super) fn popup_area(body: Rect, rows: usize) -> Rect {
    let room = body.inner(MARGIN);
    let width = u16::try_from(u32::from(body.width) * 3 / 4)
        .unwrap_or(u16::MAX)
        .clamp(POPUP_MIN_WIDTH, POPUP_MAX_WIDTH)
        .min(body.width.saturating_sub(2));
    let height = u16::try_from(rows.saturating_add(2))
        .unwrap_or(u16::MAX)
        .min(room.height);
    Rect::new(
        body.x + (body.width.saturating_sub(width)) / 2,
        room.y + (room.height - height) / 2,
        width,
        height,
    )
}

/// The first row shown in a window `height` rows tall that keeps row
/// `highlight` in view.
pub(super) fn window_start(highlight: usize, height: usize) -> usize {
    highlight.saturating_sub(height.saturating_sub(1))
}

pub(super) fn toggle_fold(folds: &mut HashSet<String>, key: &str) {
    if !folds.remove(key) {
        folds.insert(key.to_string());
    }
}

/// Which colour a row's state is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatusClass {
    Running,
    Completed,
    Failed,
}

impl StatusClass {
    fn style(self, theme: &Theme) -> Style {
        match self {
            StatusClass::Running => theme.thread_running,
            StatusClass::Completed => theme.thread_completed,
            StatusClass::Failed => theme.thread_failed,
        }
    }
}

/// What a tree row is drawn and placed from.
pub(super) trait TreeItem {
    /// Unique among the items; folds and the highlight are kept by it.
    fn key(&self) -> &str;
    /// The key of the item this one sits under; None for the top.
    fn parent_key(&self) -> Option<&str>;
    fn label(&self) -> &str;
    /// The state's word and colour; None for a row without a state.
    fn status(&self) -> Option<(&'static str, StatusClass)>;
    fn started(&self) -> Option<DateTime<Utc>>;
    /// When a finished item ended, if that is known.
    fn ended(&self) -> Option<DateTime<Utc>>;
    /// Shown faded at the end of the row, and cut first.
    fn id(&self) -> Option<&str>;
    /// Drawn in the throbber's style just before the state's word.
    fn throbber(&self) -> Option<&str> {
        None
    }

    fn running(&self) -> bool {
        matches!(self.status(), Some((_, StatusClass::Running)))
    }
}

/// Items as a tree. An item whose parent is not among them, or whose
/// parent links lead back to it, sits under the root, or at the top
/// when there is none. Siblings list the running first, then by start
/// time, oldest first, and those with no start time last.
pub(super) struct Tree<T> {
    nodes: Vec<T>,
    parents: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
    top: Vec<usize>,
}

impl<T: TreeItem> Tree<T> {
    /// The tree of `items` under `root`, which is index 0 when given; an
    /// item whose key is taken already is dropped.
    pub(super) fn build(root: Option<T>, items: Vec<T>) -> Self {
        let has_root = root.is_some();
        let mut nodes: Vec<T> = root.into_iter().collect();
        let mut index: HashMap<String, usize> = nodes
            .iter()
            .enumerate()
            .map(|(at, node)| (node.key().to_string(), at))
            .collect();
        for item in items {
            if index.contains_key(item.key()) {
                continue;
            }
            index.insert(item.key().to_string(), nodes.len());
            nodes.push(item);
        }
        let first = usize::from(has_root);
        let fallback = has_root.then_some(0);
        let linked: Vec<Option<usize>> = nodes
            .iter()
            .enumerate()
            .map(|(at, node)| {
                node.parent_key()
                    .and_then(|parent| index.get(parent).copied())
                    .filter(|&parent| at >= first && parent != at)
            })
            .collect();
        let mut parents = vec![None; nodes.len()];
        for at in first..nodes.len() {
            let mut up = linked[at];
            let mut steps = 0;
            while let Some(step) = up {
                if step == at || steps >= nodes.len() {
                    break;
                }
                up = linked[step];
                steps += 1;
            }
            parents[at] = if up.is_none() {
                linked[at].or(fallback)
            } else {
                fallback
            };
        }
        let mut children = vec![Vec::new(); nodes.len()];
        let mut top = Vec::new();
        for (at, parent) in parents.iter().enumerate() {
            match parent {
                Some(parent) => children[*parent].push(at),
                None => top.push(at),
            }
        }
        let order = |at: &usize| {
            let node = &nodes[*at];
            (
                !node.running(),
                node.started().is_none(),
                node.started(),
                node.key().to_string(),
            )
        };
        for siblings in &mut children {
            siblings.sort_by_key(order);
        }
        if !has_root {
            top.sort_by_key(order);
        }
        Self {
            nodes,
            parents,
            children,
            top,
        }
    }

    /// How many items there are, the root among them.
    pub(super) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(super) fn node(&self, at: usize) -> &T {
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

    /// The items on show in order, each under its parent; a folded item
    /// hides everything below it.
    pub(super) fn visible(&self, folds: &HashSet<String>) -> Vec<usize> {
        let mut shown = Vec::with_capacity(self.nodes.len());
        let mut stack: Vec<usize> = self.top.iter().rev().copied().collect();
        while let Some(at) = stack.pop() {
            shown.push(at);
            if !folds.contains(self.key(at)) {
                stack.extend(self.children[at].iter().rev());
            }
        }
        shown
    }

    /// The item `key` names, or its nearest ancestor on show when a fold
    /// hides it.
    pub(super) fn shown_at(&self, key: &str, folds: &HashSet<String>) -> Option<usize> {
        let at = self.find(key)?;
        let hidden_by = std::iter::successors(self.parents[at], |&up| self.parents[up])
            .filter(|&up| folds.contains(self.key(up)))
            .last();
        Some(hidden_by.unwrap_or(at))
    }

    /// Where the fold arrow of row `at` starts, in columns from the left.
    pub(super) fn arrow_column(&self, at: usize) -> usize {
        2 * self.depth(at)
    }

    pub(super) fn has_children(&self, at: usize) -> bool {
        !self.children[at].is_empty()
    }

    /// Item `at` as a row `width` columns wide: indent, fold arrow, name
    /// and state in the state's colour, the times and the id. Short of
    /// room the id loses its start and then goes, and then the start time
    /// and the run time go; the arrow, name and state stay.
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
        let status = node.status();
        let style = status.map_or(theme.heading, |(_, class)| class.style(theme));
        let mut spans = vec![
            Span::raw(" ".repeat(self.arrow_column(at))),
            Span::raw(arrow),
            Span::styled(render::plain_text(node.label()), style),
        ];
        if let Some((word, _)) = status {
            spans.push(Span::raw(GAP));
            if let Some(frame) = node.throbber() {
                spans.push(Span::styled(format!("{frame} "), theme.throbber));
            }
            spans.push(Span::styled(word, style));
        }
        let used: usize = spans.iter().map(|span| span.content.width()).sum();
        let room = width.saturating_sub(used);
        let mut times = times(node, now, offset);
        let id = node.id().map(render::plain_text);
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

/// The start time, then the time the item has run or ran for.
fn times(node: &impl TreeItem, now: DateTime<Utc>, offset: FixedOffset) -> Vec<String> {
    let Some(started) = node.started() else {
        return Vec::new();
    };
    let mut times = vec![
        started
            .with_timezone(&offset)
            .format("%H:%M:%S")
            .to_string(),
    ];
    let end = if node.running() {
        Some(now)
    } else {
        node.ended()
    };
    if let Some(end) = end {
        let ran = (end - started).to_std().unwrap_or_default();
        times.push(elapsed_text(ran));
    }
    times
}

fn status_word(status: EffectiveStatus) -> &'static str {
    match status {
        EffectiveStatus::Running => "running",
        EffectiveStatus::Completed => "completed",
        EffectiveStatus::Failed => "failed",
        EffectiveStatus::Cancelled => "cancelled",
        EffectiveStatus::Crashed => "crashed",
    }
}

fn status_class(status: EffectiveStatus) -> StatusClass {
    match status {
        EffectiveStatus::Running => StatusClass::Running,
        EffectiveStatus::Completed => StatusClass::Completed,
        EffectiveStatus::Failed | EffectiveStatus::Cancelled | EffectiveStatus::Crashed => {
            StatusClass::Failed
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
}

impl TreeItem for ThreadNode {
    fn key(&self) -> &str {
        self.session_id.as_deref().unwrap_or(MAIN_KEY)
    }

    fn parent_key(&self) -> Option<&str> {
        self.parent_session.as_deref()
    }

    fn label(&self) -> &str {
        &self.agent
    }

    fn status(&self) -> Option<(&'static str, StatusClass)> {
        self.status
            .map(|status| (status_word(status), status_class(status)))
    }

    fn started(&self) -> Option<DateTime<Utc>> {
        self.started
    }

    fn ended(&self) -> Option<DateTime<Utc>> {
        self.ended
    }

    fn id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

/// The threads, the main thread at index 0 and a child of an unknown
/// spawner under it.
pub(super) type ThreadTree = Tree<ThreadNode>;

impl ThreadTree {
    /// The tree of `threads`; a session listed twice keeps its first
    /// entry.
    pub(super) fn new(threads: Vec<ThreadNode>) -> Self {
        Tree::build(Some(ThreadNode::main()), threads)
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

    fn keys<T: TreeItem>(tree: &Tree<T>, rows: &[usize]) -> Vec<String> {
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

    struct Item {
        key: &'static str,
        parent: Option<&'static str>,
        started: Option<u32>,
        running: bool,
    }

    impl TreeItem for Item {
        fn key(&self) -> &str {
            self.key
        }

        fn parent_key(&self) -> Option<&str> {
            self.parent
        }

        fn label(&self) -> &str {
            self.key
        }

        fn status(&self) -> Option<(&'static str, StatusClass)> {
            Some(if self.running {
                ("running", StatusClass::Running)
            } else {
                ("finished", StatusClass::Completed)
            })
        }

        fn started(&self) -> Option<DateTime<Utc>> {
            self.started.map(at)
        }

        fn ended(&self) -> Option<DateTime<Utc>> {
            None
        }

        fn id(&self) -> Option<&str> {
            None
        }
    }

    /// Without a root the items with no known parent are the top rows,
    /// sorted like siblings, and an item with no start time goes last in
    /// its group.
    #[test]
    fn a_tree_without_a_root_lists_its_orphans_at_the_top() {
        let item = |key, parent, started, running| Item {
            key,
            parent,
            started,
            running,
        };
        let tree = Tree::build(
            None,
            vec![
                item("unstarted", None, None, true),
                item("done", None, Some(1), false),
                item("late", None, Some(9), true),
                item("early", None, Some(2), true),
                item("under", Some("late"), Some(10), true),
                item("lost", Some("gone"), Some(3), false),
            ],
        );
        assert_eq!(
            keys(&tree, &tree.visible(&HashSet::new())),
            ["early", "late", "under", "unstarted", "done", "lost"]
        );
        assert_eq!(tree.parent(tree.find("under").unwrap()), tree.find("late"));
        assert_eq!(tree.depth(tree.find("early").unwrap()), 0);
        let folds: HashSet<String> = ["late".to_string()].into();
        assert_eq!(
            keys(&tree, &tree.visible(&folds)),
            ["early", "late", "unstarted", "done", "lost"]
        );
    }
}
