//! Process management syscalls
use alloc::sync::Arc;

use crate::{
    config::PAGE_SIZE,
    loader::get_app_data_by_name,
    mm::{translated_refmut, translated_str, MapPermission, VirtAddr},
    task::{
        add_task, current_task, current_user_token, exit_current_and_run_next,
        suspend_current_and_run_next,
    },
    timer::get_time_us,
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// Resolve `addr` of the current task's user space into a kernel pointer.
///
/// The translation goes through the logical areas of the task's `MemorySet`, so
/// kernel-only mappings (trampoline, trap context) and read-only areas are
/// rejected exactly like the hardware would. `None` means the access is not
/// allowed.
fn user_ptr(addr: usize, for_write: bool) -> Option<*mut u8> {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    inner
        .memory_set
        .translate_user(VirtAddr(addr), for_write)
        .map(|pa| (pa.0 + addr % PAGE_SIZE) as *mut u8)
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("kernel:pid[{}] sys_exit", current_task().unwrap().pid.0);
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel:pid[{}] sys_yield", current_task().unwrap().pid.0);
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    trace!("kernel: sys_getpid pid:{}", current_task().unwrap().pid.0);
    current_task().unwrap().pid.0 as isize
}

pub fn sys_fork() -> isize {
    trace!("kernel:pid[{}] sys_fork", current_task().unwrap().pid.0);
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;
    // modify trap context of new_task, because it returns immediately after switching
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx.x[10] = 0;
    // add new task to scheduler
    add_task(new_task);
    new_pid as isize
}

pub fn sys_exec(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_exec", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let task = current_task().unwrap();
        task.exec(data);
        0
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    trace!("kernel::pid[{}] sys_waitpid [{}]", current_task().unwrap().pid.0, pid);
    let task = current_task().unwrap();
    // find a child process

    // ---- access current PCB exclusively
    let mut inner = task.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // ++++ temporarily access child PCB exclusively
        p.inner_exclusive_access().is_zombie() && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after being removed from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child PCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code;
        // ++++ release child PCB
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
    }
    // ---- release current PCB automatically
}

/// get time with second and microsecond
///
/// The value is written one byte at a time, so a `TimeVal` that straddles a page
/// boundary is still handled correctly.
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel:pid[{}] sys_get_time", current_task().unwrap().pid.0);
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

/// Map `len` bytes at `start` with protection `prot` (`1 = R`, `2 = W`, `4 = X`).
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel:pid[{}] sys_mmap", current_task().unwrap().pid.0);
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
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if inner.memory_set.mmap(start, len, permission) {
        0
    } else {
        -1
    }
}

/// Unmap the area that covers exactly `[start, start + len)`.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_munmap", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if inner.memory_set.munmap(start, len) {
        0
    } else {
        -1
    }
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel:pid[{}] sys_sbrk", current_task().unwrap().pid.0);
    if let Some(old_brk) = current_task().unwrap().change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

/// Create a new process running the application `path`.
///
/// Unlike `fork` + `exec`, the child starts in a brand new address space and the
/// parent keeps running with its own memory untouched.
pub fn sys_spawn(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_spawn", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let task = current_task().unwrap();
        let new_task = task.spawn(data);
        let new_pid = new_task.getpid();
        add_task(new_task);
        new_pid as isize
    } else {
        -1
    }
}

/// Set the scheduling priority of the current process.
///
/// Returns the new priority on success, or `-1` when it is below the minimum.
pub fn sys_set_priority(prio: isize) -> isize {
    trace!(
        "kernel:pid[{}] sys_set_priority",
        current_task().unwrap().pid.0
    );
    if prio < 0 {
        return -1;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if inner.set_priority(prio as usize) {
        prio
    } else {
        -1
    }
}
