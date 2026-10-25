//!Implementation of [`TaskManager`]
use super::TaskControlBlock;
use crate::sync::UPSafeCell;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use lazy_static::*;
///A array of `TaskControlBlock` that is thread-safe
pub struct TaskManager {
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
}

/// A stride scheduler.
///
/// Every task owns a `pass` value that grows by its `stride` each time it is
/// scheduled; the task with the smallest `pass` always runs next. Since
/// `stride = BIG_STRIDE / priority`, a task with a larger priority gets more
/// CPU time in the same wall-clock window.
impl TaskManager {
    ///Creat an empty TaskManager
    pub fn new() -> Self {
        Self {
            ready_queue: VecDeque::new(),
        }
    }
    /// Add process back to ready queue
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        self.ready_queue.push_back(task);
    }
    /// Take the process with the smallest pass out of the ready queue
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        if self.ready_queue.is_empty() {
            return None;
        }
        let mut selected = 0;
        let mut min_pass = usize::MAX;
        for (index, task) in self.ready_queue.iter().enumerate() {
            let inner = task.inner_exclusive_access();
            if inner.pass < min_pass {
                min_pass = inner.pass;
                selected = index;
            }
        }
        let task = self.ready_queue.remove(selected).unwrap();
        {
            let mut inner = task.inner_exclusive_access();
            inner.pass += inner.stride;
        }
        Some(task)
    }
}

lazy_static! {
    /// TASK_MANAGER instance through lazy_static!
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
}

/// Add process to ready queue
pub fn add_task(task: Arc<TaskControlBlock>) {
    //trace!("kernel: TaskManager::add_task");
    TASK_MANAGER.exclusive_access().add(task);
}

/// Take a process out of the ready queue
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    //trace!("kernel: TaskManager::fetch_task");
    TASK_MANAGER.exclusive_access().fetch()
}
