//! Semaphore

use crate::sync::{deadlock_detect_enabled, UPSafeCell, DEADLOCK_ERROR};
use crate::task::{block_current_and_run_next, current_task, wakeup_task, TaskControlBlock};
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

/// semaphore structure
pub struct Semaphore {
    /// semaphore inner
    pub inner: UPSafeCell<SemaphoreInner>,
}

pub struct SemaphoreInner {
    pub count: isize,
    pub wait_queue: VecDeque<Arc<TaskControlBlock>>,
    /// Who holds resources of this semaphore: `(tid, how many units)`.
    ///
    /// This is the "owner" half of the wait-for graph used by the deadlock
    /// detector; the wait queue is the other half.
    pub holders: Vec<(usize, usize)>,
}

impl Semaphore {
    /// Create a new semaphore
    pub fn new(res_count: usize) -> Self {
        trace!("kernel: Semaphore::new");
        Self {
            inner: unsafe {
                UPSafeCell::new(SemaphoreInner {
                    count: res_count as isize,
                    wait_queue: VecDeque::new(),
                    holders: Vec::new(),
                })
            },
        }
    }

    /// up operation of semaphore
    pub fn up(&self) {
        trace!("kernel: Semaphore::up");
        let tid = current_tid();
        let detecting = deadlock_detect_enabled();
        let mut inner = self.inner.exclusive_access();
        inner.count += 1;
        // the caller gives back one unit it held, if any
        if detecting {
            if let Some(entry) = inner.holders.iter_mut().find(|(id, _)| *id == tid) {
                if entry.1 > 0 {
                    entry.1 -= 1;
                }
            }
        }
        if inner.count <= 0 {
            if let Some(task) = inner.wait_queue.pop_front() {
                wakeup_task(task);
            }
        }
    }

    /// down operation of semaphore
    ///
    /// Returns `DEADLOCK_ERROR` when waiting for this semaphore would close a
    /// wait-for cycle and the deadlock detector is enabled.
    pub fn down(&self) -> isize {
        trace!("kernel: Semaphore::down");
        let tid = current_tid();
        let detecting = deadlock_detect_enabled();
        {
            let mut inner = self.inner.exclusive_access();
            if inner.count > 0 {
                inner.count -= 1;
                if detecting {
                    take_from(&mut inner.holders, tid);
                }
                return 0;
            }
        }
        // we would block: check whether blocking closes a cycle
        if detecting && would_deadlock(tid, self) {
            return DEADLOCK_ERROR;
        }
        {
            let mut inner = self.inner.exclusive_access();
            // A negative count records that this task is owed one unit; the
            // matching `up` brings the count back and wakes us.
            inner.count -= 1;
            inner.wait_queue.push_back(current_task().unwrap());
        }
        block_current_and_run_next();
        // The unit handed over by `up` was never owned by the caller, so this
        // task does not become a *holder* here: `holders` tracks only units
        // taken from the initial quota, which is what the wait-for graph needs.
        0
    }
}

/// Record that `tid` holds one more unit of this semaphore
fn take_from(holders: &mut Vec<(usize, usize)>, tid: usize) {
    if let Some(entry) = holders.iter_mut().find(|(id, _)| *id == tid) {
        entry.1 += 1;
    } else {
        holders.push((tid, 1));
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

/// Look for a wait-for cycle that would be closed if `tid` waited for `sem`.
///
/// The graph is "holder of a resource → the semaphore the holder is blocked on".
/// Reaching `tid` again means the request can never be satisfied.
fn would_deadlock(tid: usize, sem: &Semaphore) -> bool {
    let sems = all_semaphores();
    visit(sem, tid, &sems, &mut Vec::new(), 0)
}

fn visit(
    sem: &Semaphore,
    tid: usize,
    sems: &[Arc<Semaphore>],
    visited: &mut Vec<usize>,
    depth: usize,
) -> bool {
    let id = sem as *const Semaphore as usize;
    if visited.contains(&id) {
        return false;
    }
    visited.push(id);
    let holders = sem.inner.exclusive_access().holders.clone();
    if depth == 0 {
        // A task that already occupies *every* unit of the resource it asks for
        // can never make progress either.
        let total: usize = holders.iter().map(|(_, c)| *c).sum();
        let mine: usize = holders
            .iter()
            .filter(|(h, _)| *h == tid)
            .map(|(_, c)| *c)
            .sum();
        if total > 0 && mine == total {
            return true;
        }
    }
    for (holder, count) in holders {
        if count == 0 {
            continue;
        }
        if holder == tid {
            if depth > 0 {
                // Closing the cycle: another blocked thread holds resources only
                // this requester could release.
                return true;
            }
            continue;
        }
        for next in sems.iter() {
            if waits_for(next.as_ref(), holder) && visit(next.as_ref(), tid, sems, visited, depth + 1)
            {
                return true;
            }
        }
    }
    false
}

/// Whether the thread `tid` is blocked on `sem`
fn waits_for(sem: &Semaphore, tid: usize) -> bool {
    let inner = sem.inner.exclusive_access();
    inner.wait_queue.iter().any(|task| {
        task.inner_exclusive_access()
            .res
            .as_ref()
            .map(|res| res.tid == tid)
            .unwrap_or(false)
    })
}

/// Every semaphore of the current process, for the wait-for graph
fn all_semaphores() -> Vec<Arc<Semaphore>> {
    let process = crate::task::current_process();
    let process_inner = process.inner_exclusive_access();
    process_inner
        .semaphore_list
        .iter()
        .filter_map(|sem| sem.as_ref().map(Arc::clone))
        .collect()
}
