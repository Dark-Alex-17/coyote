use std::future::Future;
use tokio::task::{AbortHandle, JoinHandle};

pub struct TaskCancelGuard(Vec<AbortHandle>);

impl TaskCancelGuard {
    pub fn new<T>(tasks: &[JoinHandle<T>]) -> Self {
        Self(tasks.iter().map(|task| task.abort_handle()).collect())
    }

    pub fn spawn_one<F>(fut: F) -> Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Self(vec![tokio::spawn(fut).abort_handle()])
    }

    #[cfg(test)]
    pub fn is_finished(&self) -> bool {
        self.0.iter().all(AbortHandle::is_finished)
    }
}

impl Drop for TaskCancelGuard {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}
