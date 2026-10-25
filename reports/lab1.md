# 实验一：系统调用 trace 与任务系统调用统计（第 3 章 多道程序与分时多任务）

## 一、实验目的

1. 理解 RISC-V 用户态 / 内核态切换过程，掌握 `ecall` 触发 `UserEnvCall` 异常后的处理流程。
2. 在多道程序与分时多任务内核的基础上，新增 `sys_trace` 系统调用，实现：
   - 按地址读写当前任务的用户内存（字节粒度）；
   - 统计当前任务对某个系统调用的调用次数。
3. 加深对「系统调用是用户程序访问内核服务的唯一入口」以及「任务上下文（TCB）需要保存任务私有状态」的理解。

## 二、实验环境

| 项目 | 版本 / 说明 |
| --- | --- |
| 主机 | Windows 11 + Git Bash / GNU Make 4.4.1 |
| 目标平台 | `riscv64gc-unknown-none-elf` |
| 工具链 | `nightly-2024-05-02`（内核与用户程序），`rust-src`、`llvm-tools-preview` |
| 二进制工具 | `rust-objcopy` / `rust-objdump`（cargo-binutils 0.4.0，底层 LLVM 18.1.4） |
| 模拟器 | QEMU 8.2（`qemu-system-riscv64 -machine virt`） |
| 固件 | 仓库自带 `bootloader/rustsbi-qemu.bin`（RustSBI 0.4.1） |
| 官方检查器 | rCore-Tutorial-Checker `7d61ec5`，用户程序 rCore-Tutorial-Test `a059366` |

## 三、实验要求分析

第 3 章的模板内核已经实现了 `sys_write`、`sys_exit`、`sys_yield`、`sys_get_time`，而 `sys_trace` 只有一个返回 `-1` 的空壳。测试程序 `ch3_trace.rs` 提出了两点要求：

1. `trace_read(addr)` / `trace_write(addr, data)` 能够读写用户程序传入地址上的一个字节，非法请求返回 `-1`；
2. `count_syscall(id)` 返回**当前任务**调用系统调用 `id` 的次数，并且**本次查询自身也要计入**。

由第 2 点可以直接推出实现方式：计数必须在陷入内核、分发系统调用之前完成，而不能等到 `sys_trace` 内部再统计，否则同一次查询无法看到自己。

测试程序中的断言也印证了这一点：

```rust
let t1 = get_time() as usize;            // 第 1 次 get_time
get_time();                              // 第 2 次 get_time
sleep(500);                              // 内部大量 get_time + yield
let t2 = get_time() as usize;
let t3 = get_time() as usize;
assert!(3 <= count_syscall(SYSCALL_GETTIMEOFDAY)); // 第 1 次 trace
// 注意这次 sys_trace 调用本身也计入
assert_eq!(2, count_syscall(SYSCALL_TRACE));       // 第 2 次 trace，因此为 2
```

## 四、设计与实现

### 4.1 为 TCB 增加系统调用计数器

每个任务需要有自己独立的计数，因此在任务控制块中增加一个 `SyscallInfo`。由于第 3 章的系统调用号最大到 410，取 512 作为数组长度即可覆盖全部编号（`sys_write=64`、`sys_exit=93`、`sys_yield=124`、`sys_get_time=169`、`sys_trace=410`）。

`os/src/task/task.rs`：

```rust
/// The maximum number of syscalls that can be counted for a single task.
pub const MAX_SYSCALL_NUM: usize = 512;

/// Syscall statistics of a single task.
#[derive(Copy, Clone)]
pub struct SyscallInfo {
    pub syscall_count: [usize; MAX_SYSCALL_NUM],
}

impl SyscallInfo {
    pub fn new() -> Self {
        Self { syscall_count: [0; MAX_SYSCALL_NUM] }
    }
    /// 记录一次 syscall_id 的调用
    pub fn add(&mut self, syscall_id: usize) {
        if syscall_id < MAX_SYSCALL_NUM {
            self.syscall_count[syscall_id] += 1;
        }
    }
    pub fn get(&self, syscall_id: usize) -> usize {
        if syscall_id < MAX_SYSCALL_NUM { self.syscall_count[syscall_id] } else { 0 }
    }
}

pub struct TaskControlBlock {
    pub task_status: TaskStatus,
    pub task_cx: TaskContext,
    /// 该任务的系统调用统计
    pub syscall_info: SyscallInfo,
}
```

同时保留 `Copy` 语义（`[usize; 512]` 是 `Copy` 的），因此 `TaskManager` 中用 `[TaskControlBlock { .. }; MAX_APP_NUM]` 初始化任务数组的写法不需要改动，只需在初始化时补上 `syscall_info: SyscallInfo::new()`。

### 4.2 在任务管理器中提供计数接口

`TaskManager` 通过 `current_task` 找到当前正在运行的任务，对它做读写。`exclusive_access()` 内部关中断，保证内核态访问期间不会被时钟中断打断。

`os/src/task/mod.rs`：

```rust
impl TaskManager {
    fn add_syscall_count(&self, syscall_id: usize) {
        let mut inner = self.inner.exclusive_access();
        let current = inner.current_task;
        inner.tasks[current].syscall_info.add(syscall_id);
    }

    fn get_syscall_count(&self, syscall_id: usize) -> usize {
        let inner = self.inner.exclusive_access();
        let current = inner.current_task;
        inner.tasks[current].syscall_info.get(syscall_id)
    }
}

/// 供 syscall 模块调用的模块级封装
pub fn add_syscall_count(syscall_id: usize) { TASK_MANAGER.add_syscall_count(syscall_id); }
pub fn get_syscall_count(syscall_id: usize) -> usize { TASK_MANAGER.get_syscall_count(syscall_id) }
```

### 4.3 在系统调用分发处统一计数

`os/src/syscall/mod.rs` 是所有系统调用的唯一入口。把计数放在 `match` 之前，既能让所有系统调用都被统计到（不必逐个系统调用里手写），也保证了「查询自身也计入」的语义：

```rust
pub fn syscall(syscall_id: usize, args: [usize; 3]) -> isize {
    // Count this invocation before dispatching, so that a task can observe its
    // own syscall statistics including the querying syscall itself.
    add_syscall_count(syscall_id);
    match syscall_id {
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_YIELD => sys_yield(),
        SYSCALL_GET_TIME => sys_get_time(args[0] as *mut TimeVal, args[1]),
        SYSCALL_TRACE => sys_trace(args[0], args[1], args[2]),
        _ => panic!("Unsupported syscall_id: {}", syscall_id),
    }
}
```

### 4.4 实现 sys_trace

用户库中的 `TraceRequest` 定义是 `Read = 0, Write = 1, Syscall = 2`（`#[repr(C)]`，因此可以直接按整数传递），三个参数的含义随请求类型变化：

| trace_request | 参数 | 语义 | 返回值 |
| --- | --- | --- | --- |
| `Read = 0` | `id` = 目标地址 | 读取该地址 1 字节 | 读到的字节 |
| `Write = 1` | `id` = 目标地址，`data` = 写入值 | 向该地址写 1 字节 | 0 |
| `Syscall = 2` | `id` = 系统调用号 | 查询当前任务的调用次数 | 次数 |

`os/src/syscall/process.rs`：

```rust
const TRACE_READ: usize = 0;
const TRACE_WRITE: usize = 1;
const TRACE_SYSCALL: usize = 2;

pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
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
```

几点说明：

- 第 3 章还没有分页机制，内核与用户程序处于同一个物理地址空间，S 态可以直接访问用户地址，因此这里用裸指针 + `read_volatile` / `write_volatile` 即可。使用 `volatile` 是为了防止编译器把这次内存访问优化掉。
- 用户传入的 `data` 在系统调用时位于 `a2` 寄存器，类型是 `usize`，写入时截断为 `u8`。
- 非法的 `trace_request` 返回 `-1`。

## 五、测试结果

使用官方检查器本地复现 CI 的流程（检查器会先随机化输出标记，再编译用户程序、启动 QEMU、解析串口输出）：

```sh
make -C ci-user test CHAPTER=3
```

内核与用户程序编译通过，QEMU 中三个测试程序依次运行，输出：

```
[kernel] Hello, world!
...
get_time OK! 1234
Test sleep OK!
current time_msec = 1350
time_msec = 2456 after sleeping 100 ticks, delta = 1106ms!
Test sleep1 passed!
string from task trace test
Test trace OK!
Test passed: 3/3
```

三个测试点（`ch3_sleep`、`ch3_sleep1`、`ch3_trace`）全部通过，检查器输出 `Test passed: 3/3`。

## 六、遇到的问题与解决

1. **统计值总是差 1**：最初把计数写在各个 `sys_*` 函数内部，结果 `count_syscall(SYSCALL_TRACE)` 永远比预期小 1。原因是查询语句本身那次 `ecall` 还没有被计数就已经返回了。把计数前移到 `syscall()` 的 `match` 之前即可解决。
2. **加了计数器之后时间系统突然坏了（本次实验最主要的问题）**：给 TCB 加上 `[usize; 512]` 的计数器后，`sys_get_time` 返回的微秒数变成 0，`ch3_sleep` 断言 `current_time > 0` 直接失败；更诡异的是内核里"按常数除法"的结果全错，而按变量除法正常。

   排查过程：先在 `sys_get_time` 里打印 `rdtime`、乘积、除法的中间结果，发现 `rdtime` 和 `a * 1_000_000` 都正确，但 `/ 12_500_000` 得到 0；而同一段逻辑在用户程序里（模幂运算 `a * a % 998244353`，同样含按常数除法）却完全正确，说明问题不在编译器或模拟器，而在内核自身状态被破坏。

   最终定位到**启动栈溢出**：`TaskManager` 初始化时用 `[TaskControlBlock { .. }; MAX_APP_NUM]` 在栈上构造任务数组，每个 TCB 因为多了 4 KiB 计数器而变成 4 KiB 以上，16 个任务即 64 KiB 以上，而 `entry.asm` 的启动栈只有 `4096 * 16 = 64 KiB`，数组初始化把 `.bss.stack` 下方的 `.data` 段踩坏（其中有静态变量和 `link_app.S` 嵌入的用户程序数据），于是出现"某些常数、某些变量莫名其妙变成 0"的现象。

   解决办法：把启动栈扩大到 `4096 * 64`（256 KiB），给数组初始化留足余量。修改后 `get_time` 立刻恢复正常，后续测试全部通过。教训是：**在 `no_std` 内核里把大数组放在栈上初始化非常危险，栈大小必须按最坏情况核算**。
3. **时钟频率与 sleep 精度**：`get_time` 用 `time::read()`（`rdtime`）除以 `CLOCK_FREQ` 换算成微秒。QEMU virt 平台 `CLOCK_FREQ = 12_500_000`，换算正确时 `sleep(100)` 的实测 `delta` 约为 1000～1100 ms，与测试期望一致。
4. **任务间计数互相干扰**：一开始把计数器放在全局静态变量里，多个任务共享计数，测试中 `SYSCALL_TRACE` 的计数偏大。改为放进 TCB 之后，每个任务从 0 开始独立计数，问题消失。

## 七、小结

本实验通过在 `syscall()` 入口统一计数、在 TCB 中保存任务私有统计，实现了 `sys_trace` 的三种请求。实验中体会最深的是：

- 系统调用的**分发位置**决定了统计语义，放在入口处既简洁又能自然满足「查询自身也计入」；
- 任务私有数据必须随任务切换而切换，放在 TCB 中是唯一的正确做法；
- 第 3 章没有地址空间隔离，`trace` 直接用裸指针访问用户内存是可行的，但这也正是第 4 章要引入分页机制、并在 `sys_trace` 中做地址翻译的原因。
