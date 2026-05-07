pub mod escalation;
pub mod mailbox;
pub mod taskqueue;

use crate::utils::AbortSignal;
use fmt::{Debug, Formatter};
use mailbox::Inbox;
use taskqueue::TaskQueue;

use anyhow::{Result, bail};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentExitStatus {
    Completed,
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

impl Debug for Supervisor {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Supervisor")
            .field("active_agents", &self.handles.len())
            .field("max_concurrent", &self.max_concurrent)
            .field("max_depth", &self.max_depth)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::create_abort_signal;

    fn make_handle(id: &str, agent_name: &str, depth: usize) -> AgentHandle {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let join_handle = rt.spawn(async {
            Ok(AgentResult {
                id: "done".into(),
                agent_name: "test".into(),
                output: "result".into(),
                exit_status: AgentExitStatus::Completed,
            })
        });
        AgentHandle {
            id: id.to_string(),
            agent_name: agent_name.to_string(),
            depth,
            inbox: Arc::new(Inbox::new()),
            abort_signal: create_abort_signal(),
            join_handle,
        }
    }

    #[test]
    fn supervisor_new_empty() {
        let sup = Supervisor::new(4, 3);
        assert_eq!(sup.active_count(), 0);
        assert_eq!(sup.max_concurrent(), 4);
        assert_eq!(sup.max_depth(), 3);
    }

    #[test]
    fn supervisor_register_increments_count() {
        let mut sup = Supervisor::new(4, 3);
        sup.register(make_handle("a1", "explore", 1)).unwrap();
        assert_eq!(sup.active_count(), 1);
    }

    #[test]
    fn supervisor_register_rejects_at_capacity() {
        let mut sup = Supervisor::new(1, 3);
        sup.register(make_handle("a1", "explore", 1)).unwrap();
        let result = sup.register(make_handle("a2", "coder", 1));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at capacity"));
    }

    #[test]
    fn supervisor_register_rejects_exceeding_depth() {
        let mut sup = Supervisor::new(4, 2);
        let result = sup.register(make_handle("a1", "explore", 3));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max depth"));
    }

    #[test]
    fn supervisor_register_allows_at_max_depth() {
        let mut sup = Supervisor::new(4, 2);
        sup.register(make_handle("a1", "explore", 2)).unwrap();
        assert_eq!(sup.active_count(), 1);
    }

    #[test]
    fn supervisor_take_removes_handle() {
        let mut sup = Supervisor::new(4, 3);
        sup.register(make_handle("a1", "explore", 1)).unwrap();
        let taken = sup.take("a1");
        assert!(taken.is_some());
        assert_eq!(sup.active_count(), 0);
    }

    #[test]
    fn supervisor_take_nonexistent_returns_none() {
        let mut sup = Supervisor::new(4, 3);
        assert!(sup.take("missing").is_none());
    }

    #[test]
    fn supervisor_list_agents() {
        let mut sup = Supervisor::new(4, 3);
        sup.register(make_handle("a1", "explore", 1)).unwrap();
        sup.register(make_handle("a2", "coder", 1)).unwrap();
        let list = sup.list_agents();
        assert_eq!(list.len(), 2);
        let ids: Vec<&str> = list.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&"a1"));
        assert!(ids.contains(&"a2"));
    }

    #[test]
    fn supervisor_inbox_returns_handle_inbox() {
        let mut sup = Supervisor::new(4, 3);
        sup.register(make_handle("a1", "explore", 1)).unwrap();
        assert!(sup.inbox("a1").is_some());
        assert!(sup.inbox("missing").is_none());
    }

    #[test]
    fn supervisor_task_queue_accessible() {
        let mut sup = Supervisor::new(4, 3);
        let id = sup
            .task_queue_mut()
            .create("task".into(), "desc".into(), None, None);
        assert!(!id.is_empty());
        assert_eq!(sup.task_queue().list().len(), 1);
    }

    #[test]
    fn agent_exit_status_equality() {
        assert_eq!(AgentExitStatus::Completed, AgentExitStatus::Completed);
        assert_ne!(
            AgentExitStatus::Completed,
            AgentExitStatus::Failed("err".into())
        );
        assert_eq!(
            AgentExitStatus::Failed("x".into()),
            AgentExitStatus::Failed("x".into())
        );
    }
}
