pub mod mailbox;
pub mod taskqueue;

use crate::utils::AbortSignal;
use mailbox::Inbox;
use taskqueue::TaskQueue;

use anyhow::{Result, bail};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentExitStatus {
    Completed,
    Cancelled,
    Failed(String),
}

pub struct AgentResult {
    pub id: String,
    pub agent_name: String,
    pub output: String,
    pub exit_status: AgentExitStatus,
}

pub struct AgentHandle {
    pub id: String,
    pub agent_name: String,
    pub depth: usize,
    pub inbox: Arc<Inbox>,
    pub abort_signal: AbortSignal,
    pub join_handle: JoinHandle<Result<AgentResult>>,
}

/// Lives as an `Arc<parking_lot::RwLock<Supervisor>>` alongside GlobalConfig,
/// NOT inside it — avoids adding lock contention to the shared Config.
pub struct Supervisor {
    handles: HashMap<String, AgentHandle>,
    task_queue: TaskQueue,
    max_concurrent: usize,
    max_depth: usize,
}

impl Supervisor {
    pub fn new(max_concurrent: usize, max_depth: usize) -> Self {
        Self {
            handles: HashMap::new(),
            task_queue: TaskQueue::new(),
            max_concurrent,
            max_depth,
        }
    }

    pub fn active_count(&self) -> usize {
        self.handles.len()
    }

    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    pub fn max_depth(&self) -> usize {
        self.max_depth
    }

    pub fn task_queue(&self) -> &TaskQueue {
        &self.task_queue
    }

    pub fn task_queue_mut(&mut self) -> &mut TaskQueue {
        &mut self.task_queue
    }

    pub fn register(&mut self, handle: AgentHandle) -> Result<()> {
        if self.handles.len() >= self.max_concurrent {
            bail!(
                "Cannot spawn agent: at capacity ({}/{})",
                self.handles.len(),
                self.max_concurrent
            );
        }
        if handle.depth > self.max_depth {
            bail!(
                "Cannot spawn agent: max depth exceeded ({}/{})",
                handle.depth,
                self.max_depth
            );
        }
        self.handles.insert(handle.id.clone(), handle);
        Ok(())
    }

    pub fn is_finished(&self, id: &str) -> Option<bool> {
        self.handles.get(id).map(|h| h.join_handle.is_finished())
    }

    pub fn take_if_finished(&mut self, id: &str) -> Option<AgentHandle> {
        if self
            .handles
            .get(id)
            .is_some_and(|h| h.join_handle.is_finished())
        {
            self.handles.remove(id)
        } else {
            None
        }
    }

    pub fn take(&mut self, id: &str) -> Option<AgentHandle> {
        self.handles.remove(id)
    }

    pub fn inbox(&self, id: &str) -> Option<&Arc<Inbox>> {
        self.handles.get(id).map(|h| &h.inbox)
    }

    pub fn list_agents(&self) -> Vec<(&str, &str)> {
        self.handles
            .values()
            .map(|h| (h.id.as_str(), h.agent_name.as_str()))
            .collect()
    }

    pub fn cancel_all(&self) {
        for handle in self.handles.values() {
            handle.abort_signal.set_ctrlc();
        }
    }
}
