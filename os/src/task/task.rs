//! Types related to task management

use super::TaskContext;

/// The maximum number of syscalls that can be counted for a single task.
pub const MAX_SYSCALL_NUM: usize = 512;

/// Syscall statistics of a single task.
///
/// Every syscall is counted here when it is dispatched by the kernel, so that a
/// user program can query how many times each syscall has been issued by the
/// currently running task.
#[derive(Copy, Clone)]
pub struct SyscallInfo {
    /// number of invocations of each syscall, indexed by syscall id
    pub syscall_count: [usize; MAX_SYSCALL_NUM],
}

impl SyscallInfo {
    /// Create an all-zero statistics record.
    pub fn new() -> Self {
        Self {
            syscall_count: [0; MAX_SYSCALL_NUM],
        }
    }

    /// Record one more invocation of the syscall with id `syscall_id`.
    pub fn add(&mut self, syscall_id: usize) {
        if syscall_id < MAX_SYSCALL_NUM {
            self.syscall_count[syscall_id] += 1;
        }
    }

    /// Number of invocations of the syscall with id `syscall_id`.
    pub fn get(&self, syscall_id: usize) -> usize {
        if syscall_id < MAX_SYSCALL_NUM {
            self.syscall_count[syscall_id]
        } else {
            0
        }
    }
}

impl Default for SyscallInfo {
    fn default() -> Self {
        Self::new()
    }
}

/// The task control block (TCB) of a task.
#[derive(Copy, Clone)]
pub struct TaskControlBlock {
    /// The task status in it's lifecycle
    pub task_status: TaskStatus,
    /// The task context
    pub task_cx: TaskContext,
    /// Syscall statistics of this task
    pub syscall_info: SyscallInfo,
}

/// The status of a task
#[derive(Copy, Clone, PartialEq)]
pub enum TaskStatus {
    /// uninitialized
    UnInit,
    /// ready to run
    Ready,
    /// running
    Running,
    /// exited
    Exited,
}
