//! Process management syscalls
use crate::mm::MapPermission;
use crate::task::{
    change_program_brk, exit_current_and_run_next, get_syscall_count, mmap_current, munmap_current,
    suspend_current_and_run_next, translate_user_addr,
};
use crate::timer::get_time_us;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// Read the byte at a user address through the current address space.
const TRACE_READ: usize = 0;
/// Write one byte to a user address through the current address space.
const TRACE_WRITE: usize = 1;
/// Query how many times a syscall has been invoked by the current task.
const TRACE_SYSCALL: usize = 2;

/// Resolve `addr` of the current task's user space into a kernel pointer.
///
/// The translation goes through the logical areas of the task's `MemorySet`, so
/// kernel-only mappings (trampoline, trap context) and read-only areas are
/// rejected exactly like the hardware would. `None` means the access is not
/// allowed.
fn user_ptr(addr: usize, for_write: bool) -> Option<*mut u8> {
    translate_user_addr(addr, for_write).map(|pa| pa as *mut u8)
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("kernel: sys_exit with code {}", exit_code);
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
///
/// The two words are written one byte at a time, so a `TimeVal` that straddles
/// a page boundary is still handled correctly.
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    let tv = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };
    let raw = unsafe {
        core::slice::from_raw_parts(
            &tv as *const TimeVal as *const u8,
            core::mem::size_of::<TimeVal>(),
        )
    };
    for (offset, byte) in raw.iter().enumerate() {
        match user_ptr(ts as usize + offset, true) {
            Some(ptr) => unsafe { ptr.write_volatile(*byte) },
            None => return -1,
        }
    }
    0
}

/// trace syscall: read or write one byte of the task's memory, or report how
/// many times the task has invoked a syscall.
///
/// * `trace_request` selects the operation; for `Read`/`Write` `id` is the user
///   address and `data` the byte to write, and for `Syscall` `id` is the syscall
///   id to query.
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    match trace_request {
        TRACE_READ => match user_ptr(id, false) {
            Some(ptr) => unsafe { ptr.read_volatile() as isize },
            None => -1,
        },
        TRACE_WRITE => match user_ptr(id, true) {
            Some(ptr) => {
                unsafe { ptr.write_volatile(data as u8) };
                0
            }
            None => -1,
        },
        TRACE_SYSCALL => get_syscall_count(id) as isize,
        _ => -1,
    }
}

/// Map `len` bytes at `start` with protection `prot` (`1 = R`, `2 = W`, `4 = X`).
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel: sys_mmap");
    if len == 0 || prot == 0 || prot & !0b111 != 0 {
        return -1;
    }
    let mut permission = MapPermission::U;
    if prot & 0b001 != 0 {
        permission |= MapPermission::R;
    }
    if prot & 0b010 != 0 {
        permission |= MapPermission::W;
    }
    if prot & 0b100 != 0 {
        permission |= MapPermission::X;
    }
    if mmap_current(start, len, permission) {
        0
    } else {
        -1
    }
}

/// Unmap the area that covers exactly `[start, start + len)`.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    if munmap_current(start, len) {
        0
    } else {
        -1
    }
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
