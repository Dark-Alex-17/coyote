//! What each spawned child agent is doing right now, kept from its tool
//! and graph node hooks as transcript cells so its thread can be drawn
//! with the main view's tool rows while the child runs. A child's session
//! file has its messages; only this store has its calls as they happen.

use super::FULL_OUTPUT_BUDGET;
use super::cells::{self, Cell, FoldedCall, NodeStatus, TaskRow, ToolStatus};
use super::sink::{AgentOutcome, ChildNodeState};
use indexmap::IndexMap;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The most rows one child keeps; the oldest goes first.
pub(super) const CHILD_ROW_LIMIT: usize = 500;

/// How a child's tool call ended, as its finish hook reports it. The
/// call's name and arguments pair a call that has no id with its start.
pub(super) struct ToolEnd {
    pub(super) name: String,
    pub(super) args: Option<Value>,
    pub(super) ok: bool,
    pub(super) error: Option<String>,
    pub(super) duration_ms: Option<u64>,
}

/// A child's call as row text: a child's call has no narration line, so
/// it is the call's name and arguments.
pub(super) fn call_summary(name: &str, args: Option<&Value>) -> String {
    match args {
        Some(args) => format!("{name} {args}"),
        None => name.to_string(),
    }
}

/// One child's rows: its top-level tool calls and, for a graph agent, a
/// row per node it ran in start order with that node's calls folded under it.
#[derive(Default)]
pub(crate) struct ChildThread {
    name: String,
    session_id: Option<String>,
    /// How the child ended; None while it runs.
    outcome: Option<AgentOutcome>,
    rows: Vec<Cell>,
}

impl ChildThread {
    pub(super) fn rows(&self) -> &[Cell] {
        &self.rows
    }

    pub(super) fn rows_mut(&mut self) -> &mut [Cell] {
        &mut self.rows
    }

    /// Drops the cached lines of every call or node of the child yet to
    /// end, so the next frame draws its throbber and elapsed time anew;
    /// whether there was one.
    pub(super) fn forget_running_lines(&mut self) -> bool {
        let mut any = false;
        for cell in self.rows.iter_mut().filter(|cell| cell.running()) {
            cell.forget_wrapped_lines();
            any = true;
        }
        any
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

    /// The top-level call row a finish of `call_id` settles; for a call
    /// with no id, the oldest running one with the same `summary`, as
    /// calls run side by side finish in any order.
    fn tool_to_finish(&mut self, call_id: &str, summary: &str) -> Option<&mut Cell> {
        if call_id.is_empty() {
            return self.rows.iter_mut().find(|cell| {
                cell.running()
                    && matches!(cell, Cell::Tool { call_id: id, summary: text, .. }
                        if id.is_empty() && text == summary)
            });
        }
        self.rows
            .iter_mut()
            .rev()
            .find(|cell| matches!(cell, Cell::Tool { call_id: id, .. } if id == call_id))
    }

    /// Settles every call and node of the child still running as failed
    /// with `error`, the calls with the time they ran up to `now`; their
    /// finish hooks never come once the child has ended. Returns the bytes
    /// of whole output this added.
    fn settle_running(&mut self, error: &str, now: Instant) -> isize {
        let mut grew = 0;
        for cell in &mut self.rows {
            match cell {
                Cell::Tool { status, tail, .. } => {
                    grew += settle_call(status, &mut tail.full, error, now);
                }
                Cell::Node { row, .. } => {
                    grew += settle_folded_calls(&mut row.calls, error, now);
                    if matches!(row.status, NodeStatus::Running { .. }) {
                        row.status = NodeStatus::Failed(error.to_string());
                    }
                }
                _ => {}
            }
        }
        self.forget_wrapped_lines();
        grew
    }

    /// A row's state changed in place, so its cached lines are stale.
    /// Only the newest rows change, but a child's rows are few enough to
    /// drop every cache.
    fn forget_wrapped_lines(&mut self) {
        self.rows.iter_mut().for_each(Cell::forget_wrapped_lines);
    }
}

/// Settles a call still running as failed with `error`, with the time it
/// ran up to `now`; returns the bytes of whole output this added.
fn settle_call(
    status: &mut ToolStatus,
    full: &mut cells::FullOutput,
    error: &str,
    now: Instant,
) -> isize {
    if !matches!(status, ToolStatus::Running { .. }) {
        return 0;
    }
    status.finish(now, false, Some(error.to_string()), None);
    full.settle()
}

fn settle_folded_calls(calls: &mut [FoldedCall], error: &str, now: Instant) -> isize {
    calls
        .iter_mut()
        .filter_map(|FoldedCall { status, tail, .. }| {
            Some(settle_call(status.as_mut()?, &mut tail.full, error, now))
        })
        .sum()
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
    /// its agent's name and how it ended, None while it runs.
    pub(super) fn sessions(&self) -> impl Iterator<Item = (&str, &str, Option<&AgentOutcome>)> {
        self.threads.values().filter_map(|thread| {
            let session_id = thread.session_id.as_deref()?;
            Some((session_id, thread.name.as_str(), thread.outcome.as_ref()))
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
        thread.outcome = None;
    }

    /// The child ended: whatever it left running is settled as failed,
    /// with why the child ended.
    pub(super) fn agent_finished(&mut self, agent_id: &str, outcome: &AgentOutcome, now: Instant) {
        let Some(thread) = self.threads.get_mut(agent_id) else {
            return;
        };
        let error = match outcome {
            AgentOutcome::Interrupted => "interrupted",
            AgentOutcome::Failed(error) if !error.is_empty() => error,
            AgentOutcome::Failed(_) | AgentOutcome::Completed => "agent finished",
        };
        let grew = thread.settle_running(error, now);
        thread.outcome = Some(outcome.clone());
        self.account_full_output(grew, FULL_OUTPUT_BUDGET);
    }

    /// Drops every row's cached lines, for when the renderer changed.
    pub(super) fn forget_layout(&mut self) {
        self.threads
            .values_mut()
            .flat_map(|thread| thread.rows.iter_mut())
            .for_each(Cell::forget_layout);
    }

    /// Lets go of the whole output any child row laid out for the pager,
    /// as the transcript's rows do once it closes.
    pub(super) fn release_full_output_caches(&mut self) {
        for thread in self.threads.values_mut() {
            cells::release_full_output_caches(&mut thread.rows);
        }
    }

    /// Drops the thread of every finished child `listed` no longer names,
    /// with the output it held. A running child is kept even while the
    /// roster has yet to list it.
    pub(super) fn retain_listed(&mut self, listed: impl Fn(&str) -> bool) {
        let mut freed = 0;
        self.threads.retain(|agent_id, thread| {
            let keep = thread.outcome.is_none() || listed(agent_id);
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
                    thread
                        .rows
                        .push(Cell::node(node_id.clone(), String::new(), now));
                }
                if let Some(row) = thread.node_row(&node_id) {
                    row.calls
                        .push(FoldedCall::running(call_id, name, args, now));
                }
                thread.forget_wrapped_lines();
            }
            None => {
                let summary = call_summary(&name, args.as_ref());
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
            name,
            args,
            ok,
            error,
            duration_ms,
        } = end;
        let grew = match node {
            Some(node_id) => {
                let Some(call) = thread.node_row(node_id).and_then(|row| {
                    if call_id.is_empty() {
                        row.calls.iter_mut().find(|call| {
                            call.call_id.is_empty()
                                && call.name == name
                                && call.args == args
                                && matches!(call.status, Some(ToolStatus::Running { .. }))
                        })
                    } else {
                        row.calls
                            .iter_mut()
                            .rev()
                            .find(|call| call.call_id == call_id)
                    }
                }) else {
                    return;
                };
                if let Some(status) = &mut call.status {
                    status.finish(now, ok, error, duration_ms);
                }
                call.tail.full.settle()
            }
            None => {
                let summary = call_summary(&name, args.as_ref());
                let Some(Cell::Tool { status, tail, .. }) =
                    thread.tool_to_finish(call_id, &summary)
                else {
                    return;
                };
                status.finish(now, ok, error, duration_ms);
                tail.full.settle()
            }
        };
        thread.forget_wrapped_lines();
        self.account_full_output(grew, FULL_OUTPUT_BUDGET);
    }

    /// Output of a call that ran inside `node`, or at the child's top
    /// level for None; the node tells apart calls the provider gave no id.
    pub(super) fn tool_output(
        &mut self,
        agent_id: &str,
        node: Option<&str>,
        call_id: &str,
        chunk: &[u8],
    ) {
        if chunk.is_empty() {
            return;
        }
        let Some(thread) = self.threads.get_mut(agent_id) else {
            return;
        };
        let Some(cell) = thread.rows.iter_mut().rev().find(|cell| {
            let in_place = match (&**cell, node) {
                (Cell::Node { node_id, .. }, Some(node)) => node_id == node,
                (Cell::Node { .. }, None) | (_, Some(_)) => false,
                (_, None) => true,
            };
            in_place && cell.takes_output_of(call_id)
        }) else {
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
        now: Instant,
    ) {
        let Some(thread) = self.threads.get_mut(agent_id) else {
            return;
        };
        let status = match state {
            ChildNodeState::Started => {
                // A call can report its node before the node's own start
                // does; the row it opened then is this start's.
                if let Some(Cell::Node {
                    node_type: placeholder,
                    ..
                }) = thread.rows.iter_mut().rev().find(|cell| {
                    matches!(cell, Cell::Node { node_id: id, node_type, .. }
                        if *id == node_id && node_type.is_empty() && cell.running())
                }) {
                    *placeholder = node_type;
                    thread.forget_wrapped_lines();
                    return;
                }
                thread.rows.push(Cell::node(node_id, node_type, now));
                self.cap_rows(agent_id);
                return;
            }
            ChildNodeState::Completed { duration_ms, route } => NodeStatus::Completed {
                elapsed: duration_ms.map(Duration::from_millis),
                route,
            },
            ChildNodeState::Failed { error } => NodeStatus::Failed(error),
        };
        // The node's calls whose finish never came end with it.
        let error = match &status {
            NodeStatus::Failed(error) if !error.is_empty() => error.clone(),
            _ => "node finished".to_string(),
        };
        let grew = thread.node_row(&node_id).map_or(0, |row| {
            row.status = status;
            settle_folded_calls(&mut row.calls, &error, now)
        });
        thread.forget_wrapped_lines();
        self.account_full_output(grew, FULL_OUTPUT_BUDGET);
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
