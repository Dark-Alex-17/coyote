use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Blocked,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: TaskStatus,
    pub owner: Option<String>,
    pub blocked_by: HashSet<String>,
    pub blocks: HashSet<String>,
}

impl TaskNode {
    pub fn new(id: String, subject: String, description: String) -> Self {
        Self {
            id,
            subject,
            description,
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: HashSet::new(),
            blocks: HashSet::new(),
        }
    }

    pub fn is_runnable(&self) -> bool {
        self.status == TaskStatus::Pending && self.blocked_by.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct TaskQueue {
    tasks: HashMap<String, TaskNode>,
    next_id: usize,
}

impl TaskQueue {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn create(&mut self, subject: String, description: String) -> String {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let task = TaskNode::new(id.clone(), subject, description);
        self.tasks.insert(id.clone(), task);
        id
    }

    pub fn add_dependency(&mut self, task_id: &str, blocked_by: &str) -> Result<(), String> {
        if task_id == blocked_by {
            return Err("A task cannot depend on itself".into());
        }
        if !self.tasks.contains_key(blocked_by) {
            return Err(format!("Dependency task '{blocked_by}' does not exist"));
        }
        if !self.tasks.contains_key(task_id) {
            return Err(format!("Task '{task_id}' does not exist"));
        }

        if self.would_create_cycle(task_id, blocked_by) {
            return Err(format!(
                "Adding dependency {task_id} -> {blocked_by} would create a cycle"
            ));
        }

        if let Some(task) = self.tasks.get_mut(task_id) {
            task.blocked_by.insert(blocked_by.to_string());
            task.status = TaskStatus::Blocked;
        }
        if let Some(blocker) = self.tasks.get_mut(blocked_by) {
            blocker.blocks.insert(task_id.to_string());
        }
        Ok(())
    }

    pub fn complete(&mut self, task_id: &str) -> Vec<String> {
        let mut newly_runnable = Vec::new();

        let dependents: Vec<String> = self
            .tasks
            .get(task_id)
            .map(|t| t.blocks.iter().cloned().collect())
            .unwrap_or_default();

        if let Some(task) = self.tasks.get_mut(task_id) {
            task.status = TaskStatus::Completed;
        }

        for dep_id in &dependents {
            if let Some(dep) = self.tasks.get_mut(dep_id) {
                dep.blocked_by.remove(task_id);
                if dep.blocked_by.is_empty() && dep.status == TaskStatus::Blocked {
                    dep.status = TaskStatus::Pending;
                    newly_runnable.push(dep_id.clone());
                }
            }
        }

        newly_runnable
    }

    pub fn fail(&mut self, task_id: &str) {
        if let Some(task) = self.tasks.get_mut(task_id) {
            task.status = TaskStatus::Failed;
        }
    }

    pub fn claim(&mut self, task_id: &str, owner: &str) -> bool {
        if let Some(task) = self.tasks.get_mut(task_id) {
            if task.is_runnable() && task.owner.is_none() {
                task.owner = Some(owner.to_string());
                task.status = TaskStatus::InProgress;
                return true;
            }
        }
        false
    }

    pub fn runnable_tasks(&self) -> Vec<&TaskNode> {
        self.tasks.values().filter(|t| t.is_runnable()).collect()
    }

    pub fn get(&self, task_id: &str) -> Option<&TaskNode> {
        self.tasks.get(task_id)
    }

    pub fn list(&self) -> Vec<&TaskNode> {
        let mut tasks: Vec<&TaskNode> = self.tasks.values().collect();
        tasks.sort_by_key(|t| t.id.parse::<usize>().unwrap_or(0));
        tasks
    }

    // DFS cycle detection: would adding task_id -> blocked_by create a cycle?
    // A cycle exists if blocked_by can reach task_id through existing dependencies.
    fn would_create_cycle(&self, task_id: &str, blocked_by: &str) -> bool {
        let mut visited = HashSet::new();
        let mut stack = vec![blocked_by.to_string()];

        while let Some(current) = stack.pop() {
            if current == task_id {
                return true;
            }
            if visited.insert(current.clone()) {
                if let Some(task) = self.tasks.get(&current) {
                    for dep in &task.blocked_by {
                        stack.push(dep.clone());
                    }
                }
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_list() {
        let mut queue = TaskQueue::new();
        let id1 = queue.create("Research".into(), "Research auth patterns".into());
        let id2 = queue.create("Implement".into(), "Write the code".into());

        assert_eq!(id1, "1");
        assert_eq!(id2, "2");
        assert_eq!(queue.list().len(), 2);
    }

    #[test]
    fn test_dependency_and_completion() {
        let mut queue = TaskQueue::new();
        let id1 = queue.create("Step 1".into(), "".into());
        let id2 = queue.create("Step 2".into(), "".into());

        queue.add_dependency(&id2, &id1).unwrap();

        assert!(queue.get(&id1).unwrap().is_runnable());
        assert!(!queue.get(&id2).unwrap().is_runnable());
        assert_eq!(queue.get(&id2).unwrap().status, TaskStatus::Blocked);

        let unblocked = queue.complete(&id1);
        assert_eq!(unblocked, vec![id2.clone()]);
        assert!(queue.get(&id2).unwrap().is_runnable());
    }

    #[test]
    fn test_fan_in_dependency() {
        let mut queue = TaskQueue::new();
        let id1 = queue.create("A".into(), "".into());
        let id2 = queue.create("B".into(), "".into());
        let id3 = queue.create("C (needs A and B)".into(), "".into());

        queue.add_dependency(&id3, &id1).unwrap();
        queue.add_dependency(&id3, &id2).unwrap();

        assert!(!queue.get(&id3).unwrap().is_runnable());

        let unblocked = queue.complete(&id1);
        assert!(unblocked.is_empty());
        assert!(!queue.get(&id3).unwrap().is_runnable());

        let unblocked = queue.complete(&id2);
        assert_eq!(unblocked, vec![id3.clone()]);
        assert!(queue.get(&id3).unwrap().is_runnable());
    }

    #[test]
    fn test_cycle_detection() {
        let mut queue = TaskQueue::new();
        let id1 = queue.create("A".into(), "".into());
        let id2 = queue.create("B".into(), "".into());

        queue.add_dependency(&id2, &id1).unwrap();
        let result = queue.add_dependency(&id1, &id2);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cycle"));
    }

    #[test]
    fn test_self_dependency_rejected() {
        let mut queue = TaskQueue::new();
        let id1 = queue.create("A".into(), "".into());
        let result = queue.add_dependency(&id1, &id1);
        assert!(result.is_err());
    }

    #[test]
    fn test_claim() {
        let mut queue = TaskQueue::new();
        let id1 = queue.create("Task".into(), "".into());

        assert!(queue.claim(&id1, "worker-1"));
        assert!(!queue.claim(&id1, "worker-2"));
        assert_eq!(queue.get(&id1).unwrap().status, TaskStatus::InProgress);
    }
}
