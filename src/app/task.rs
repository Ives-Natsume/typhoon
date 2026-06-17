#![allow(dead_code)]
use std::sync::Mutex;

pub struct TaskManager {
    current_task: Mutex<Option<Task>>,
}

impl TaskManager {
    pub fn new() -> Self {
        TaskManager {
            current_task: Mutex::new(None),
        }
    }
}

pub struct Task {
    id: u32,
    name: String,
}