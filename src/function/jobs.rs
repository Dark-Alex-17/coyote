use crate::supervisor::Supervisor;

use parking_lot::RwLock;
use std::sync::Arc;

#[allow(dead_code)]
pub fn is_agent_task(supervisor: Option<&Arc<RwLock<Supervisor>>>, id: &str) -> bool {
    id.starts_with("agent_")
        || id.starts_with("graph_agent_")
        || supervisor.is_some_and(|sup| sup.read().has_agent(id))
}

pub struct RingBuf {
    buf: Vec<u8>,
    capacity: usize,
    write_pos: usize,
    total_written: u64,
}

#[allow(dead_code)]
impl RingBuf {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: Vec::new(),
            capacity,
            write_pos: 0,
            total_written: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.total_written += bytes.len() as u64;
        if self.capacity == 0 {
            return;
        }
        let src = if bytes.len() > self.capacity {
            &bytes[bytes.len() - self.capacity..]
        } else {
            bytes
        };
        for &byte in src {
            if self.buf.len() < self.capacity {
                self.buf.push(byte);
            } else {
                self.buf[self.write_pos] = byte;
            }
            self.write_pos = (self.write_pos + 1) % self.capacity;
        }
    }

    pub fn total_written(&self) -> u64 {
        self.total_written
    }

    pub fn tail(&self) -> Vec<u8> {
        if self.buf.len() < self.capacity {
            return self.buf.clone();
        }
        let mut out = Vec::with_capacity(self.capacity);
        out.extend_from_slice(&self.buf[self.write_pos..]);
        out.extend_from_slice(&self.buf[..self.write_pos]);
        out
    }
}

impl Default for RingBuf {
    fn default() -> Self {
        Self::new(64 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buf_returns_contents_below_capacity() {
        let mut buf = RingBuf::new(8);
        buf.push(b"abc");
        buf.push(b"de");
        assert_eq!(buf.tail(), b"abcde");
        assert_eq!(buf.total_written(), 5);
    }

    #[test]
    fn ring_buf_exact_fit_keeps_everything() {
        let mut buf = RingBuf::new(5);
        buf.push(b"abcde");
        assert_eq!(buf.tail(), b"abcde");
        assert_eq!(buf.total_written(), 5);
    }

    #[test]
    fn ring_buf_wrap_around_keeps_newest_bytes() {
        let mut buf = RingBuf::new(5);
        buf.push(b"abcde");
        buf.push(b"fg");
        assert_eq!(buf.tail(), b"cdefg");
        assert_eq!(buf.total_written(), 7);
    }

    #[test]
    fn ring_buf_oversize_push_keeps_last_capacity_bytes() {
        let mut buf = RingBuf::new(4);
        buf.push(b"abcdefghij");
        assert_eq!(buf.tail(), b"ghij");
        assert_eq!(buf.total_written(), 10);
    }

    #[test]
    fn ring_buf_default_capacity_is_64_kib() {
        let mut buf = RingBuf::default();
        let payload = vec![b'x'; 64 * 1024 + 1];
        buf.push(&payload);
        assert_eq!(buf.tail().len(), 64 * 1024);
        assert_eq!(buf.total_written(), 64 * 1024 + 1);
    }

    #[test]
    fn is_agent_task_matches_agent_prefixes() {
        assert!(is_agent_task(None, "agent_explore_a1b2c3d4"));
        assert!(is_agent_task(None, "graph_agent_explore_a1b2c3d4"));
        assert!(!is_agent_task(None, "job_deadbeef"));
    }

    #[test]
    fn is_agent_task_matches_registered_agents() {
        use crate::supervisor::mailbox::Inbox;
        use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult};
        use crate::utils::create_abort_signal;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let join_handle = rt.spawn(async {
            Ok(AgentResult {
                id: "a1".into(),
                agent_name: "explore".into(),
                output: String::new(),
                exit_status: AgentExitStatus::Completed,
            })
        });
        std::mem::forget(rt);
        let handle = AgentHandle {
            id: "a1".to_string(),
            agent_name: "explore".to_string(),
            depth: 1,
            inbox: Arc::new(Inbox::new()),
            abort_signal: create_abort_signal(),
            join_handle,
            child_supervisor: None,
        };
        let mut sup = Supervisor::new(4, 3);
        sup.register(handle).unwrap();
        let sup = Arc::new(RwLock::new(sup));

        assert!(is_agent_task(Some(&sup), "a1"));
        assert!(!is_agent_task(Some(&sup), "missing"));
    }
}
