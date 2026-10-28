//! Interactive Todo Tracker for Varynth: live task tracking across turns,
//! TUI progress widget, and agent tool execution.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub id: usize,
    pub text: String,
    pub done: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TodoStore {
    pub next_id: usize,
    pub items: Vec<TodoItem>,
}

impl TodoStore {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            items: Vec::new(),
        }
    }

    pub fn load_or_new(path: &Path) -> Self {
        if let Ok(bytes) = fs::read(path) {
            if let Ok(store) = serde_json::from_slice::<Self>(&bytes) {
                return store;
            }
        }
        Self::new()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(self)?;
        fs::write(path, data)?;
        Ok(())
    }

    pub fn add(&mut self, text: &str) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(TodoItem {
            id,
            text: text.trim().to_string(),
            done: false,
            created_at: Utc::now(),
        });
        id
    }

    pub fn set_done(&mut self, id: usize, done: bool) -> bool {
        if let Some(item) = self.items.iter_mut().find(|it| it.id == id) {
            item.done = done;
            true
        } else {
            false
        }
    }

    pub fn toggle(&mut self, id: usize) -> bool {
        if let Some(item) = self.items.iter_mut().find(|it| it.id == id) {
            item.done = !item.done;
            true
        } else {
            false
        }
    }

    pub fn remove(&mut self, id: usize) -> bool {
        let initial_len = self.items.len();
        self.items.retain(|it| it.id != id);
        self.items.len() < initial_len
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.next_id = 1;
    }

    pub fn progress(&self) -> (usize, usize) {
        let done = self.items.iter().filter(|it| it.done).count();
        (done, self.items.len())
    }

    pub fn summary_badge(&self) -> Option<String> {
        let (done, total) = self.progress();
        if total == 0 {
            None
        } else {
            let pct = (done * 100) / total;
            Some(format!("todos: {done}/{total} ({pct}%)"))
        }
    }

    pub fn format_list(&self) -> String {
        if self.items.is_empty() {
            return "No tasks in the todo list. Use `/todo add <task>` to create one.".to_string();
        }
        let (done, total) = self.progress();
        let mut out = format!("📋 Tasks ({done}/{total} completed):\n");
        for it in &self.items {
            let mark = if it.done { "✔" } else { " " };
            out.push_str(&format!("  [{mark}] #{:<2} {}\n", it.id, it.text));
        }
        out
    }
}
