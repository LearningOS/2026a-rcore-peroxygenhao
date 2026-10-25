//! Process management syscalls
use crate::{
    task::{exit_current_and_run_next, get_syscall_count, suspend_current_and_run_next},
    timer::get_time_us,
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// Read a byte at the user supplied address.
const TRACE_READ: usize = 0;
/// Write a byte to the user supplied address.
const TRACE_WRITE: usize = 1;
/// Query how many times a syscall has been invoked by the current task.
const TRACE_SYSCALL: usize = 2;

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("[kernel] Application exited with code {}", exit_code);
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// get time with second and microsecond
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    unsafe {
        *ts = TimeVal {
            sec: us / 1_000_000,
            usec: us % 1_000_000,
        };
    }
    0
}

/// trace syscall: read / write a byte of the task's memory, or report the number
/// of invocations of a syscall issued by the current task.
///
/// * `trace_request` tells what to do, and the meaning of `id` / `data` depends
///   on it: for `Read` and `Write`, `id` is the target address and `data` is the
///   byte to store; for `Syscall`, `id` is the syscall id to query.
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    match trace_request {
        TRACE_READ => unsafe { (id as *const u8).read_volatile() as isize },
        TRACE_WRITE => {
            unsafe { (id as *mut u8).write_volatile(data as u8) };
            0
        }
        TRACE_SYSCALL => get_syscall_count(id) as isize,
        _ => -1,
    }
}
