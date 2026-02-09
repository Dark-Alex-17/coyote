use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TodoStatus {
    Pending,
    Done,
}

impl TodoStatus {
    fn icon(&self) -> &'static str {
        match self {
            TodoStatus::Pending => "○",
            TodoStatus::Done => "✓",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: usize,
    #[serde(alias = "description")]
    pub desc: String,
    pub done: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TodoList {
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub todos: Vec<TodoItem>,
}

impl TodoList {
    pub fn new(goal: &str) -> Self {
        Self {
            goal: goal.to_string(),
            todos: Vec::new(),
        }
    }

    pub fn add(&mut self, task: &str) -> usize {
        let id = self.todos.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        self.todos.push(TodoItem {
            id,
            desc: task.to_string(),
            done: false,
        });
        id
    }

    pub fn mark_done(&mut self, id: usize) -> bool {
        if let Some(item) = self.todos.iter_mut().find(|t| t.id == id) {
            item.done = true;
            true
        } else {
            false
        }
    }

    pub fn has_incomplete(&self) -> bool {
        self.todos.iter().any(|item| !item.done)
    }

    pub fn is_empty(&self) -> bool {
        self.todos.is_empty()
    }

    pub fn render_for_model(&self) -> String {
        let mut lines = Vec::new();
        if !self.goal.is_empty() {
            lines.push(format!("Goal: {}", self.goal));
        }
        lines.push(format!(
            "Progress: {}/{} completed",
            self.completed_count(),
            self.todos.len()
        ));
        for item in &self.todos {
            let status = if item.done {
                TodoStatus::Done
            } else {
                TodoStatus::Pending
            };
            lines.push(format!("  {} {}. {}", status.icon(), item.id, item.desc));
        }
        lines.join("\n")
    }

    pub fn incomplete_count(&self) -> usize {
        self.todos.iter().filter(|item| !item.done).count()
    }

    pub fn completed_count(&self) -> usize {
        self.todos.iter().filter(|item| item.done).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_and_add() {
        let mut list = TodoList::new("Map Labs");
        assert_eq!(list.add("Discover"), 1);
        assert_eq!(list.add("Map columns"), 2);
        assert_eq!(list.todos.len(), 2);
        assert!(list.has_incomplete());
    }

    #[test]
    fn test_mark_done() {
        let mut list = TodoList::new("Test");
        list.add("Task 1");
        list.add("Task 2");
        assert!(list.mark_done(1));
        assert!(!list.mark_done(99));
        assert_eq!(list.completed_count(), 1);
        assert_eq!(list.incomplete_count(), 1);
    }

    #[test]
    fn test_empty_list() {
        let list = TodoList::default();
        assert!(!list.has_incomplete());
        assert!(list.is_empty());
    }

    #[test]
    fn test_all_done() {
        let mut list = TodoList::new("Test");
        list.add("Done task");
        list.mark_done(1);
        assert!(!list.has_incomplete());
    }

    #[test]
    fn test_render_for_model() {
        let mut list = TodoList::new("Map Labs");
        list.add("Discover");
        list.add("Map");
        list.mark_done(1);
        let rendered = list.render_for_model();
        assert!(rendered.contains("Goal: Map Labs"));
        assert!(rendered.contains("Progress: 1/2 completed"));
        assert!(rendered.contains("✓ 1. Discover"));
        assert!(rendered.contains("○ 2. Map"));
    }

    #[test]
    fn test_serialization_roundtrip() {
        let mut list = TodoList::new("Roundtrip");
        list.add("Step 1");
        list.add("Step 2");
        list.mark_done(1);
        let json = serde_json::to_string(&list).unwrap();
        let deserialized: TodoList = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.goal, "Roundtrip");
        assert_eq!(deserialized.todos.len(), 2);
        assert!(deserialized.todos[0].done);
        assert!(!deserialized.todos[1].done);
    }
}
