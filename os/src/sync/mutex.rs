//! Mutex (spin-like and blocking(sleep))

use super::{deadlock_detect_enabled, UPSafeCell, DEADLOCK_ERROR};
use crate::task::TaskControlBlock;
use crate::task::{block_current_and_run_next, suspend_current_and_run_next};
use crate::task::{current_task, wakeup_task};
use alloc::{collections::VecDeque, sync::Arc};

/// Mutex trait
pub trait Mutex: Sync + Send {
    /// Lock the mutex, returning [`DEADLOCK_ERROR`] when the lock cannot ever
    /// be acquired (a self deadlock detected by the deadlock detector)
    fn lock(&self) -> isize;
    /// Unlock the mutex
    fn unlock(&self);
}

/// Spinlock Mutex struct
pub struct MutexSpin {
    locked: UPSafeCell<bool>,
}

impl MutexSpin {
    /// Create a new spinlock mutex
    pub fn new() -> Self {
        Self {
            locked: unsafe { UPSafeCell::new(false) },
        }
    }
}

impl Mutex for MutexSpin {
    /// Lock the spinlock mutex
    fn lock(&self) -> isize {
        trace!("kernel: MutexSpin::lock");
        loop {
            let mut locked = self.locked.exclusive_access();
            if *locked {
                drop(locked);
                suspend_current_and_run_next();
                continue;
            } else {
                *locked = true;
                return 0;
            }
        }
    }

    fn unlock(&self) {
        trace!("kernel: MutexSpin::unlock");
        let mut locked = self.locked.exclusive_access();
        *locked = false;
    }
}

/// Blocking Mutex struct
pub struct MutexBlocking {
    inner: UPSafeCell<MutexBlockingInner>,
}

pub struct MutexBlockingInner {
    locked: bool,
    /// The thread currently owning the mutex, used for deadlock detection
    holder: Option<usize>,
    wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl MutexBlocking {
    /// Create a new blocking mutex
    pub fn new() -> Self {
        trace!("kernel: MutexBlocking::new");
        Self {
            inner: unsafe {
                UPSafeCell::new(MutexBlockingInner {
                    locked: false,
                    holder: None,
                    wait_queue: VecDeque::new(),
                })
            },
        }
    }
}

/// The thread id of the running task
fn current_tid() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid
}

impl Mutex for MutexBlocking {
    /// lock the blocking mutex
    fn lock(&self) -> isize {
        trace!("kernel: MutexBlocking::lock");
        let tid = current_tid();
        let mut mutex_inner = self.inner.exclusive_access();
        if mutex_inner.locked {
            // A task asking again for a mutex it already owns can never make
            // progress: report the deadlock instead of blocking forever.
            if deadlock_detect_enabled() && mutex_inner.holder == Some(tid) {
                return DEADLOCK_ERROR;
            }
            mutex_inner.wait_queue.push_back(current_task().unwrap());
            drop(mutex_inner);
            block_current_and_run_next();
            // the unlocking thread handed the mutex over to us
            self.inner.exclusive_access().holder = Some(tid);
        } else {
            mutex_inner.locked = true;
            mutex_inner.holder = Some(tid);
        }
        0
    }

    /// unlock the blocking mutex
    fn unlock(&self) {
        trace!("kernel: MutexBlocking::unlock");
        let mut mutex_inner = self.inner.exclusive_access();
        assert!(mutex_inner.locked);
        if let Some(waking_task) = mutex_inner.wait_queue.pop_front() {
            // ownership is transferred, `holder` is updated by the woken task
            wakeup_task(waking_task);
        } else {
            mutex_inner.locked = false;
            mutex_inner.holder = None;
        }
    }
}
