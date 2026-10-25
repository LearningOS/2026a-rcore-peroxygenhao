//! Synchronization and interior mutability primitives

mod condvar;
mod mutex;
mod semaphore;
mod up;

pub use condvar::Condvar;
pub use mutex::{Mutex, MutexBlocking, MutexSpin};
pub use semaphore::Semaphore;
pub use up::UPSafeCell;

/// The error returned to user space when a deadlock is detected
pub const DEADLOCK_ERROR: isize = -0xdead;

/// Turn the deadlock detector on or off for the current process.
///
/// The switch is per process, so one program enabling the detector does not
/// change the behaviour of every other program on the system.
pub fn set_deadlock_detect(enabled: bool) {
    crate::task::current_process()
        .inner_exclusive_access()
        .deadlock_detect = enabled;
}

/// Whether the deadlock detector is enabled for the current process
pub fn deadlock_detect_enabled() -> bool {
    crate::task::current_process()
        .inner_exclusive_access()
        .deadlock_detect
}
