pub mod escalation;
pub mod mailbox;
pub mod notification;
pub mod taskqueue;

use crate::function::jobs::RingBuf;
use crate::utils::AbortSignal;
use fmt::{Debug, Formatter};
use mailbox::Inbox;
use parking_lot::{Mutex, RwLock};
use taskqueue::TaskQueue;

use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;
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
    pub child_supervisor: Option<Arc<RwLock<Supervisor>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Completed,
    Failed,
}

pub struct JobState {
    pub status: JobStatus,
    pub pgid: Option<i32>,
}

pub struct JobResult {
    pub output: Value,
    pub exit_code: Option<i32>,
    pub output_bytes_captured: u64,
}

pub struct JobHandle {
    pub id: String,
    pub tool: String,
    pub started_at: Instant,
    pub join_handle: JoinHandle<Result<JobResult>>,
    pub abort_signal: AbortSignal,
    pub state: Arc<Mutex<JobState>>,
    pub output_buf: Arc<Mutex<RingBuf>>,
    pub no_change_checks: u32,
    pub last_check_state: Option<(JobStatus, u64)>,
}

impl JobHandle {
    // pgid == child pid under process_group(0); after wait() reaps the child
    // the pid can be recycled, so never kill unless pgid is still set.
    fn kill_process_group(&self) {
        #[cfg(unix)]
        if let Some(pgid) = self.state.lock().pgid {
            unsafe {
                libc::killpg(pgid, libc::SIGTERM);
            }
        }
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        self.kill_process_group();
        self.join_handle.abort();
    }
}

pub enum TaskHandle {
    Agent(AgentHandle),
    Job(JobHandle),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Agent,
    Job,
}

impl From<AgentHandle> for TaskHandle {
    fn from(handle: AgentHandle) -> Self {
        Self::Agent(handle)
    }
}

impl From<JobHandle> for TaskHandle {
    fn from(handle: JobHandle) -> Self {
        Self::Job(handle)
    }
}

pub struct Supervisor {
    handles: HashMap<String, TaskHandle>,
    task_queue: TaskQueue,
    max_concurrent: usize,
    max_depth: usize,
    max_concurrent_jobs: usize,
}

impl Supervisor {
    pub fn new(max_concurrent: usize, max_depth: usize) -> Self {
        Self {
            handles: HashMap::new(),
            task_queue: TaskQueue::new(),
            max_concurrent,
            max_depth,
            max_concurrent_jobs: 0,
        }
    }

    pub fn with_max_concurrent_jobs(mut self, max_concurrent_jobs: usize) -> Self {
        self.max_concurrent_jobs = max_concurrent_jobs;
        self
    }

    fn agent(&self, id: &str) -> Option<&AgentHandle> {
        match self.handles.get(id) {
            Some(TaskHandle::Agent(handle)) => Some(handle),
            _ => None,
        }
    }

    fn agents(&self) -> impl Iterator<Item = &AgentHandle> {
        self.handles.values().filter_map(|handle| match handle {
            TaskHandle::Agent(handle) => Some(handle),
            TaskHandle::Job(_) => None,
        })
    }

    pub fn job(&self, id: &str) -> Option<&JobHandle> {
        match self.handles.get(id) {
            Some(TaskHandle::Job(handle)) => Some(handle),
            _ => None,
        }
    }

    pub fn job_mut(&mut self, id: &str) -> Option<&mut JobHandle> {
        match self.handles.get_mut(id) {
            Some(TaskHandle::Job(handle)) => Some(handle),
            _ => None,
        }
    }

    pub fn jobs(&self) -> impl Iterator<Item = &JobHandle> {
        self.handles.values().filter_map(|handle| match handle {
            TaskHandle::Job(handle) => Some(handle),
            TaskHandle::Agent(_) => None,
        })
    }

    pub fn active_count(&self) -> usize {
        self.agents().count()
    }

    pub fn effective_active_count(&self) -> usize {
        self.agents()
            .filter(|h| !h.join_handle.is_finished())
            .count()
    }

    pub fn active_job_count(&self) -> usize {
        self.handles
            .values()
            .filter(|h| matches!(h, TaskHandle::Job(job) if !job.join_handle.is_finished()))
            .count()
    }

    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    pub fn max_depth(&self) -> usize {
        self.max_depth
    }

    pub fn max_concurrent_jobs(&self) -> usize {
        self.max_concurrent_jobs
    }

    pub fn task_queue(&self) -> &TaskQueue {
        &self.task_queue
    }

    pub fn task_queue_mut(&mut self) -> &mut TaskQueue {
        &mut self.task_queue
    }

    pub fn register(&mut self, handle: impl Into<TaskHandle>) -> Result<()> {
        match handle.into() {
            TaskHandle::Agent(handle) => {
                if self.effective_active_count() >= self.max_concurrent {
                    bail!(
                        "Cannot spawn agent: at capacity ({}/{})",
                        self.effective_active_count(),
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
                self.handles
                    .insert(handle.id.clone(), TaskHandle::Agent(handle));
            }
            TaskHandle::Job(handle) => {
                if self.active_job_count() >= self.max_concurrent_jobs {
                    bail!(
                        "Cannot start job: at capacity ({}/{})",
                        self.active_job_count(),
                        self.max_concurrent_jobs
                    );
                }
                self.handles
                    .insert(handle.id.clone(), TaskHandle::Job(handle));
            }
        }
        Ok(())
    }

    pub fn is_finished(&self, id: &str) -> Option<bool> {
        self.agent(id).map(|h| h.join_handle.is_finished())
    }

    pub fn take(&mut self, id: &str) -> Option<AgentHandle> {
        self.agent(id)?;
        match self.handles.remove(id) {
            Some(TaskHandle::Agent(handle)) => Some(handle),
            _ => None,
        }
    }

    pub fn take_job(&mut self, id: &str) -> Option<JobHandle> {
        if !self.has_job(id) {
            return None;
        }
        match self.handles.remove(id) {
            Some(TaskHandle::Job(handle)) => Some(handle),
            _ => None,
        }
    }

    pub fn has_job(&self, id: &str) -> bool {
        matches!(self.handles.get(id), Some(TaskHandle::Job(_)))
    }

    pub fn has_agent(&self, id: &str) -> bool {
        self.agent(id).is_some()
    }

    pub fn inbox(&self, id: &str) -> Option<&Arc<Inbox>> {
        self.agent(id).map(|h| &h.inbox)
    }

    pub fn abort_signal_for(&self, id: &str) -> Option<AbortSignal> {
        self.agent(id).map(|h| h.abort_signal.clone())
    }

    pub fn list_agents(&self) -> Vec<(&str, &str)> {
        self.agents()
            .map(|h| (h.id.as_str(), h.agent_name.as_str()))
            .collect()
    }

    pub fn list_tasks(&self) -> Vec<(&str, TaskKind, bool)> {
        self.handles
            .values()
            .map(|handle| match handle {
                TaskHandle::Agent(agent) => (
                    agent.id.as_str(),
                    TaskKind::Agent,
                    agent.join_handle.is_finished(),
                ),
                TaskHandle::Job(job) => (
                    job.id.as_str(),
                    TaskKind::Job,
                    job.join_handle.is_finished(),
                ),
            })
            .collect()
    }

    pub fn cancel_all(&self) {
        for handle in self.handles.values() {
            match handle {
                TaskHandle::Agent(agent) => agent.abort_signal.set_ctrlc(),
                TaskHandle::Job(job) => {
                    job.abort_signal.set_ctrlc();
                    job.kill_process_group();
                }
            }
        }
    }

    pub fn cancel_recursive(&self) {
        for handle in self.handles.values() {
            match handle {
                TaskHandle::Agent(agent) => {
                    agent.abort_signal.set_ctrlc();
                    if let Some(child_sup) = agent.child_supervisor.as_ref() {
                        child_sup.read().cancel_recursive();
                    }
                }
                TaskHandle::Job(job) => {
                    job.abort_signal.set_ctrlc();
                    job.kill_process_group();
                }
            }
        }
    }
}

impl Debug for Supervisor {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Supervisor")
            .field("active_agents", &self.active_count())
            .field("max_concurrent", &self.max_concurrent)
            .field("max_depth", &self.max_depth)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::create_abort_signal;
    use anyhow::Error;
    use std::mem;
    use tokio::runtime::Builder;

    fn make_handle(id: &str, agent_name: &str, depth: usize) -> AgentHandle {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
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
            child_supervisor: None,
        }
    }

    fn make_job(id: &str, abort_signal: AbortSignal) -> JobHandle {
        // Keep the runtime alive so the spawned task is never polled and the
        // job counts as running for capacity checks.
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let join_handle = rt.spawn(async {
            Ok(JobResult {
                output: Value::Null,
                exit_code: Some(0),
                output_bytes_captured: 0,
            })
        });
        mem::forget(rt);
        JobHandle {
            id: id.to_string(),
            tool: "execute_command".to_string(),
            started_at: Instant::now(),
            join_handle,
            abort_signal,
            state: Arc::new(Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            })),
            output_buf: Arc::new(Mutex::new(RingBuf::default())),
            no_change_checks: 0,
            last_check_state: None,
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
        // Keep the runtime alive in this scope so the spawned task is never
        // polled (current_thread only polls inside block_on), keeping
        // join_handle.is_finished() == false and the slot occupied.
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let join_handle = rt.spawn(async {
            Ok::<AgentResult, Error>(AgentResult {
                id: "done".into(),
                agent_name: "test".into(),
                output: "result".into(),
                exit_status: AgentExitStatus::Completed,
            })
        });
        let running_handle = AgentHandle {
            id: "a1".to_string(),
            agent_name: "explore".to_string(),
            depth: 1,
            inbox: Arc::new(Inbox::new()),
            abort_signal: create_abort_signal(),
            join_handle,
            child_supervisor: None,
        };
        let mut sup = Supervisor::new(1, 3);
        sup.register(running_handle).unwrap();
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

    #[test]
    fn cancel_recursive_aborts_nested_supervisors() {
        let child_sig = create_abort_signal();
        let mut child_handle = make_handle("c1", "worker", 2);
        child_handle.abort_signal = child_sig.clone();
        let mut child_sup = Supervisor::new(4, 3);
        child_sup.register(child_handle).unwrap();

        let parent_sig = create_abort_signal();
        let mut parent_handle = make_handle("a1", "explore", 1);
        parent_handle.abort_signal = parent_sig.clone();
        parent_handle.child_supervisor = Some(Arc::new(RwLock::new(child_sup)));
        let mut sup = Supervisor::new(4, 3);
        sup.register(parent_handle).unwrap();

        sup.cancel_recursive();

        assert!(parent_sig.aborted());
        assert!(child_sig.aborted());
    }

    #[test]
    fn job_registration_rejects_when_job_capacity_zero() {
        let mut sup = Supervisor::new(4, 3);

        let result = sup.register(make_job("j1", create_abort_signal()));

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at capacity"));
    }

    #[test]
    fn job_registration_rejects_at_job_capacity() {
        let mut sup = Supervisor::new(4, 3).with_max_concurrent_jobs(1);
        sup.register(make_job("j1", create_abort_signal())).unwrap();

        let result = sup.register(make_job("j2", create_abort_signal()));

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at capacity"));
    }

    #[test]
    fn job_capacity_is_independent_of_agent_capacity() {
        let mut sup = Supervisor::new(1, 3).with_max_concurrent_jobs(1);

        sup.register(make_job("j1", create_abort_signal())).unwrap();
        sup.register(make_handle("a1", "explore", 1)).unwrap();

        assert_eq!(sup.active_job_count(), 1);
        assert_eq!(sup.active_count(), 1);
        assert_eq!(sup.max_concurrent_jobs(), 1);
    }

    #[test]
    fn agent_accessors_ignore_jobs() {
        let mut sup = Supervisor::new(4, 3).with_max_concurrent_jobs(2);

        sup.register(make_job("j1", create_abort_signal())).unwrap();

        assert_eq!(sup.active_count(), 0);
        assert_eq!(sup.effective_active_count(), 0);
        assert!(sup.list_agents().is_empty());
        assert_eq!(sup.is_finished("j1"), None);
        assert!(sup.inbox("j1").is_none());
        assert!(sup.abort_signal_for("j1").is_none());
        assert!(sup.take("j1").is_none());
        assert!(sup.has_job("j1"));
        assert!(!sup.has_agent("j1"));
        assert_eq!(sup.active_job_count(), 1);
    }

    #[test]
    fn take_job_removes_job_but_not_agents() {
        let mut sup = Supervisor::new(4, 3).with_max_concurrent_jobs(2);

        sup.register(make_job("j1", create_abort_signal())).unwrap();
        sup.register(make_handle("a1", "explore", 1)).unwrap();

        assert!(sup.take_job("a1").is_none());
        assert!(sup.has_agent("a1"));
        assert!(sup.take_job("j1").is_some());
        assert_eq!(sup.active_job_count(), 0);
    }

    #[test]
    fn cancel_recursive_aborts_jobs() {
        let sig = create_abort_signal();
        let mut sup = Supervisor::new(4, 3).with_max_concurrent_jobs(1);
        sup.register(make_job("j1", sig.clone())).unwrap();

        sup.cancel_recursive();

        assert!(sig.aborted());
    }

    #[test]
    fn cancel_all_aborts_jobs() {
        let sig = create_abort_signal();
        let mut sup = Supervisor::new(4, 3).with_max_concurrent_jobs(1);
        sup.register(make_job("j1", sig.clone())).unwrap();

        sup.cancel_all();

        assert!(sig.aborted());
    }
}
