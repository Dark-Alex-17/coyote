//! What each spawned child agent is doing right now, kept from its tool
//! and graph node hooks as transcript cells so its thread can be drawn
//! with the main view's tool rows while the child runs. A child's session
//! file has its messages; only this store has its calls as they happen.

use super::FULL_OUTPUT_BUDGET;
use super::cells::{self, Cell, FoldedCall, NodeStatus, TaskRow, ToolStatus};
use super::sink::ChildNodeState;
use indexmap::IndexMap;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The most rows one child keeps; the oldest goes first.
pub(super) const CHILD_ROW_LIMIT: usize = 500;

/// How a child's tool call ended, as its finish hook reports it.
pub(super) struct ToolEnd {
    pub(super) ok: bool,
    pub(super) error: Option<String>,
    pub(super) duration_ms: Option<u64>,
}

/// One child's rows: its top-level tool calls and, for a graph agent, a
/// row per node it ran in start order with that node's calls folded under it.
#[derive(Default)]
pub(crate) struct ChildThread {
    name: String,
    session_id: Option<String>,
    finished: bool,
    rows: Vec<Cell>,
}

impl ChildThread {
    pub(super) fn rows(&self) -> &[Cell] {
        &self.rows
    }

    pub(super) fn rows_mut(&mut self) -> &mut [Cell] {
        &mut self.rows
    }

    /// Whether a call or a node of the child has yet to end.
    pub(super) fn running(&self) -> bool {
        self.rows.iter().any(|cell| match cell {
            Cell::Tool { status, .. } => matches!(status, ToolStatus::Running { .. }),
            Cell::Node { row, .. } => row.status == NodeStatus::Running,
            _ => false,
        })
    }

    /// The newest row of node `node_id`; a node a graph loops back to has
    /// a row per visit.
    fn node_row(&mut self, node_id: &str) -> Option<&mut TaskRow<NodeStatus>> {
        self.rows.iter_mut().rev().find_map(|cell| match cell {
            Cell::Node {
                node_id: id, row, ..
            } if id == node_id => Some(row),
            _ => None,
        })
    }

    /// A row's state changed in place, so its cached lines are stale.
    /// Only the newest rows change, but a child's rows are few enough to
    /// drop every cache.
    fn forget_wrapped_lines(&mut self) {
        self.rows.iter_mut().for_each(Cell::forget_wrapped_lines);
    }
}

/// Every live child thread by agent id, in the order the children
/// started, and which child each session id names.
#[derive(Default)]
pub(crate) struct ChildLive {
    threads: IndexMap<String, ChildThread>,
    sessions: HashMap<String, String>,
    full_output_bytes: usize,
}

impl ChildLive {
    #[cfg(test)]
    pub(crate) fn by_agent(&self, agent_id: &str) -> Option<&ChildThread> {
        self.threads.get(agent_id)
    }

    pub(crate) fn by_session(&self, session_id: &str) -> Option<&ChildThread> {
        self.threads.get(self.sessions.get(session_id)?)
    }

    pub(crate) fn by_session_mut(&mut self, session_id: &str) -> Option<&mut ChildThread> {
        self.threads.get_mut(self.sessions.get(session_id)?)
    }

    /// Whether `session_id` is the session of child `agent_id`.
    pub(super) fn session_is_of(&self, session_id: &str, agent_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .is_some_and(|id| id == agent_id)
    }

    /// Each child that keeps a session, in start order: its session id,
    /// its agent's name and whether it finished.
    pub(super) fn sessions(&self) -> impl Iterator<Item = (&str, &str, bool)> {
        self.threads.values().filter_map(|thread| {
            let session_id = thread.session_id.as_deref()?;
            Some((session_id, thread.name.as_str(), thread.finished))
        })
    }

    /// Opens the thread of a child that just started. Only a started
    /// child gets one, so a grandchild's or an agent node's events, which
    /// no roster row leads to, are dropped instead of piling up.
    pub(super) fn agent_started(
        &mut self,
        agent_id: String,
        name: String,
        session_id: Option<String>,
    ) {
        if let Some(session_id) = &session_id {
            self.sessions.insert(session_id.clone(), agent_id.clone());
        }
        let thread = self.threads.entry(agent_id).or_default();
        thread.name = name;
        thread.session_id = session_id;
        thread.finished = false;
    }

    pub(super) fn agent_finished(&mut self, agent_id: &str) {
        if let Some(thread) = self.threads.get_mut(agent_id) {
            thread.finished = true;
        }
    }

    /// Drops every row's cached lines, for when the renderer changed.
    pub(super) fn forget_layout(&mut self) {
        self.threads
            .values_mut()
            .flat_map(|thread| thread.rows.iter_mut())
            .for_each(Cell::forget_layout);
    }

    /// Drops the thread of every finished child `listed` no longer names,
    /// with the output it held. A running child is kept even while the
    /// roster has yet to list it.
    pub(super) fn retain_listed(&mut self, listed: impl Fn(&str) -> bool) {
        let mut freed = 0;
        self.threads.retain(|agent_id, thread| {
            let keep = !thread.finished || listed(agent_id);
            if !keep {
                freed += thread
                    .rows
                    .iter()
                    .map(Cell::full_output_bytes)
                    .sum::<usize>();
            }
            keep
        });
        let threads = &self.threads;
        self.sessions
            .retain(|_, agent_id| threads.contains_key(agent_id));
        self.full_output_bytes = self.full_output_bytes.saturating_sub(freed);
    }

    pub(super) fn tool_started(
        &mut self,
        agent_id: &str,
        node: Option<String>,
        call_id: String,
        name: String,
        args: Option<Value>,
        now: Instant,
    ) {
        let Some(thread) = self.threads.get_mut(agent_id) else {
            return;
        };
        match node {
            Some(node_id) => {
                if thread.node_row(&node_id).is_none() {
                    thread.rows.push(Cell::node(node_id.clone(), String::new()));
                }
                if let Some(row) = thread.node_row(&node_id) {
                    row.calls.push(FoldedCall::running(call_id, name, now));
                }
                thread.forget_wrapped_lines();
            }
            None => {
                // A child's call has no narration line, so the row's text
                // is its name and arguments.
                let summary = match args {
                    Some(args) => format!("{name} {args}"),
                    None => name,
                };
                thread.rows.push(Cell::tool(call_id, summary, now));
            }
        }
        self.cap_rows(agent_id);
    }

    pub(super) fn tool_finished(
        &mut self,
        agent_id: &str,
        node: Option<&str>,
        call_id: &str,
        end: ToolEnd,
        now: Instant,
    ) {
        let Some(thread) = self.threads.get_mut(agent_id) else {
            return;
        };
        let ToolEnd {
            ok,
            error,
            duration_ms,
        } = end;
        let grew =
            match node {
                Some(node_id) => {
                    let Some(call) = thread.node_row(node_id).and_then(|row| {
                        row.calls
                            .iter_mut()
                            .rev()
                            .find(|call| call.call_id == call_id)
                    }) else {
                        return;
                    };
                    if let Some(status) = &mut call.status {
                        status.finish(now, ok, error, duration_ms);
                    }
                    call.tail.full.settle()
                }
                None => {
                    let Some(Cell::Tool { status, tail, .. }) = thread.rows.iter_mut().rev().find(
                        |cell| matches!(cell, Cell::Tool { call_id: id, .. } if id == call_id),
                    ) else {
                        return;
                    };
                    status.finish(now, ok, error, duration_ms);
                    tail.full.settle()
                }
            };
        thread.forget_wrapped_lines();
        self.account_full_output(grew, FULL_OUTPUT_BUDGET);
    }

    pub(super) fn tool_output(&mut self, agent_id: &str, call_id: &str, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let Some(thread) = self.threads.get_mut(agent_id) else {
            return;
        };
        let Some(cell) = thread
            .rows
            .iter_mut()
            .rev()
            .find(|cell| cell.takes_output_of(call_id))
        else {
            return;
        };
        cell.forget_wrapped_lines();
        let Some(tail) = cell.output_for(call_id) else {
            return;
        };
        let text = cells::append_output(tail, chunk);
        let grew = tail.full.push(&text);
        self.account_full_output(grew, FULL_OUTPUT_BUDGET);
    }

    pub(super) fn node(
        &mut self,
        agent_id: &str,
        node_id: String,
        node_type: String,
        state: ChildNodeState,
    ) {
        let Some(thread) = self.threads.get_mut(agent_id) else {
            return;
        };
        let status = match state {
            ChildNodeState::Started => {
                thread.rows.push(Cell::node(node_id, node_type));
                self.cap_rows(agent_id);
                return;
            }
            ChildNodeState::Completed { duration_ms, route } => NodeStatus::Completed {
                elapsed: duration_ms.map(Duration::from_millis),
                route,
            },
            ChildNodeState::Failed { error } => NodeStatus::Failed(error),
        };
        if let Some(row) = thread.node_row(&node_id) {
            row.status = status;
        }
        thread.forget_wrapped_lines();
    }

    fn cap_rows(&mut self, agent_id: &str) {
        let Some(thread) = self.threads.get_mut(agent_id) else {
            return;
        };
        let excess = thread.rows.len().saturating_sub(CHILD_ROW_LIMIT);
        let freed: usize = thread
            .rows
            .drain(..excess)
            .map(|cell| cell.full_output_bytes())
            .sum();
        self.full_output_bytes = self.full_output_bytes.saturating_sub(freed);
    }

    /// As the transcript's own accounting: counts `delta` more bytes of
    /// whole output, then gives up the oldest calls' whole output, the
    /// oldest child's first, until the total fits `budget`.
    pub(super) fn account_full_output(&mut self, delta: isize, budget: usize) {
        self.full_output_bytes = self.full_output_bytes.saturating_add_signed(delta);
        for cell in self
            .threads
            .values_mut()
            .flat_map(|thread| thread.rows.iter_mut())
        {
            let excess = self.full_output_bytes.saturating_sub(budget);
            if excess == 0 {
                break;
            }
            let freed = cell.evict_full_outputs(excess);
            self.full_output_bytes = self.full_output_bytes.saturating_sub(freed);
        }
    }

    #[cfg(test)]
    pub(super) fn full_output_bytes(&self) -> usize {
        self.full_output_bytes
    }
}
