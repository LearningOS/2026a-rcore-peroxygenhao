# 实验三：stride 调度、spawn 与优先级（第 5 章 进程管理）

## 一、实验目的

1. 理解进程模型：`fork` / `exec` / `waitpid` 与进程控制块（PCB）的关系，掌握 `Processor` + `TaskManager` 的调度结构。
2. 把先来先服务（FIFO）调度器改造成 **stride 调度**，使进程获得的 CPU 时间与优先级成正比。
3. 实现 `sys_set_priority`，让用户进程能够调整自己的调度优先级。
4. 实现 `sys_spawn`：直接在**新的地址空间**里创建并运行一个程序，理解它与 `fork + exec` 的区别。
5. 在进程模型下重新实现 `sys_get_time`、`sys_mmap`、`sys_munmap`。

## 二、实验环境

| 项目 | 版本 / 说明 |
| --- | --- |
| 主机 | Windows 11 + GNU Make 4.4.1 + Git Bash |
| 目标平台 | `riscv64gc-unknown-none-elf` |
| 工具链 | `nightly-2024-02-25`、`rust-src`、`llvm-tools-preview` |
| 模拟器 | QEMU 8.2（`qemu-system-riscv64 -machine virt`） |
| 官方检查器 | rCore-Tutorial-Checker `7d61ec5`，用户程序 rCore-Tutorial-Test `a059366` |

## 三、实验要求分析

第 5 章模板已经具备完整的进程模型（`TaskManager` 的就绪队列 + `Processor` 的当前进程 + `fork`/`exec`/`waitpid`），需要补的是：

1. `sys_set_priority(prio)`：
   - 合法优先级返回**新的优先级**；
   - `prio < 2` 返回 `-1`；
   - 允许 `isize::MAX` 这样极大的优先级（此时 stride 必须仍有意义）。
2. stride 调度：每个进程维护 `pass` 与 `stride`，每次被调度后 `pass += stride`，调度器总是选择 `pass` 最小的进程。
3. `sys_spawn(path)`：按 ELF 名字创建新进程，返回子进程 pid；父进程的地址空间**不受影响**（不能用 fork + exec 简单等价）。
4. `sys_get_time` / `sys_mmap` / `sys_munmap`：与第 4 章相同，但要通过 `current_task()` 拿到当前进程的 `MemorySet`。

测试程序给出的判定条件：

```rust
// ch5_setprio
assert_eq!(set_priority(10), 10);
assert_eq!(set_priority(isize::MAX), isize::MAX);
assert_eq!(set_priority(0), -1);
assert_eq!(set_priority(1), -1);
assert_eq!(set_priority(-10), -1);

// ch5_spawn0：连续 spawn 40 个 ch5_getpid，并逐个 wait 回收
// ch5_spawn1：spawn 后通过 wait / waitpid 拿到正确的退出码
// ch5_stride：spawn 6 个不同优先级的进程，各自统计 4 秒内的循环次数，
//             期望 count 与 priority 基本成正比
```

## 四、设计与实现

### 4.1 优先级与 stride 的数据结构

在 `TaskControlBlockInner` 中增加三个字段：

```rust
/// 调度优先级，越大获得越多 CPU 时间
pub priority: usize,
/// 当前优先级对应的步长：BIG_STRIDE / priority
pub stride: usize,
/// stride 调度累计的 pass 值
pub pass: usize,
```

`BIG_STRIDE = 1 << 20`、`DEFAULT_PRIORITY = 2`、`MIN_PRIORITY = 2` 放在 `config.rs`。

修改优先级时同步更新 stride，并对极大优先级做保护（`BIG_STRIDE / isize::MAX` 会得到 0，导致 `pass` 永远不涨、进程再也拿不到调度；这里取 `max(1)`）：

```rust
pub fn set_priority(&mut self, priority: usize) -> bool {
    if priority < MIN_PRIORITY {
        return false;
    }
    self.priority = priority;
    self.stride = (BIG_STRIDE / priority).max(1);
    true
}
```

`fork` 出来的子进程继承父进程的 `priority`/`stride`/`pass`（与 Linux 的优先级继承一致）；`spawn` 创建的新进程也继承父进程的优先级。

### 4.2 stride 调度器

把 `TaskManager::fetch` 从「弹出队首」改成「选出 `pass` 最小的进程」，并在选中后累加它的 `pass`：

```rust
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
```

时间复杂度 O(n)（就绪进程数量很小，没有必要维护堆）。由于 `pass` 只在被选中时增加，长时间来看每个进程获得的 CPU 时间与其 `1/stride`（即 `priority`）成正比 —— 这正是 stride 调度的不变式。

### 4.3 sys_set_priority

```rust
pub fn sys_set_priority(prio: isize) -> isize {
    if prio < 0 {
        return -1;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if inner.set_priority(prio as usize) { prio } else { -1 }
}
```

注意返回值是**新优先级**（测试用 `assert_eq!(set_priority(10), 10)` 检查），而不是旧优先级。

### 4.4 sys_spawn

`spawn` 需要「新地址空间 + 新进程」，因此不能复用 `fork`（`fork` 会拷贝父进程地址空间并让子进程从同一位置继续执行）。实现上复用 `TaskControlBlock::new`（它本来就是按 ELF 建立地址空间、分配 pid 与内核栈），再把父子关系接上：

```rust
pub fn spawn(self: &Arc<Self>, elf_data: &[u8]) -> Arc<Self> {
    let task_control_block = Arc::new(TaskControlBlock::new(elf_data));
    {
        let mut parent_inner = self.inner_exclusive_access();
        let mut child_inner = task_control_block.inner_exclusive_access();
        child_inner.parent = Some(Arc::downgrade(self));
        child_inner.priority = parent_inner.priority;
        child_inner.stride = parent_inner.stride;
        parent_inner.children.push(task_control_block.clone());
    }
    task_control_block
}
```

系统调用层只负责从用户地址空间取出路径字符串并查表：

```rust
pub fn sys_spawn(path: *const u8) -> isize {
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let task = current_task().unwrap();
        let new_task = task.spawn(data);
        let new_pid = new_task.getpid();
        add_task(new_task);          // 加入就绪队列，等调度器挑选
        new_pid as isize
    } else {
        -1
    }
}
```

`TaskControlBlock::new` 内部用 `MemorySet::from_elf` 建立全新地址空间并初始化 `TrapContext`，因此子进程从 ELF 入口开始执行，父进程的 `MemorySet` 完全不受影响。

### 4.5 sys_get_time / sys_mmap / sys_munmap

与第 4 章相同，只是当前进程换成了 `current_task()`。用户内存访问统一走 `MemorySet::translate_user`：先按逻辑段找到所属区域并检查 `U`/`R`/`W` 权限，再通过页表拿到物理页，返回可以直接解引用的内核指针。

## 五、测试结果

```sh
make -C ci-user test CHAPTER=5
```

检查器输出（关键行）：

```
Usertests: Running ch5_spawn0
Test spawn0 OK!
Usertests: Running ch5_spawn1
Test wait OK!
Test waitpid OK!
Usertests: Running ch5_setprio
Test set_priority OK!
Usertests: Running ch5_stride
priority = 5, exitcode = 12345, ratio = 2469
...
Test passed: 22/22
```

`ch2b_*`、`ch3_sleep*`、`ch4_mmap*`、`ch4_unmap*`、`ch5_*` 全部通过，`NOT_EXPECTED` 中的错误标记均未出现。

## 六、遇到的问题与解决

1. **`set_priority(isize::MAX)` 之后进程"饿死"**：`BIG_STRIDE / isize::MAX` 整数除法得到 0，`pass` 不再增长，于是这个进程一旦被调度就会永远被优先选中。用 `.max(1)` 保证 stride 至少为 1 后解决。
2. **stride 调度的计数基准**：一开始把 `pass` 的更新放在 `add_task`（进入就绪队列）时，导致同一进程在不同队列位置获得的票数不均。改为「被 `fetch` 选中时才累加 `pass`」，不变式才成立。
3. **`spawn` 后父进程崩溃**：最初直接在父进程的 `inner_exclusive_access()` 借用期内调用 `TaskControlBlock::new`，而 `new` 内部又要访问 `KERNEL_SPACE`，属于同一把锁的重入，触发 `RefCell` panic。改成先构造子进程、再进父进程的临界区接好父子关系。
4. **`spawn` 与 `fork` 的语义**：`fork + exec` 也能得到「从新程序开始执行」的效果，但 `fork` 需要先完整拷贝父地址空间，代价高且语义上子进程曾经是父进程的一份拷贝；`spawn` 直接按 ELF 建新地址空间，是更合适的语义。

## 七、小结

本实验的核心是把调度策略从 FIFO 换成 stride：只要维护好 `pass += stride` 这个不变式，优先级就能直接体现在 CPU 时间占比上。`spawn` 的实现则说明了「进程创建」与「地址空间创建」是可以分离的：`TaskControlBlock::new` 负责后者，父子链表负责前者。`set_priority` 的返回值、`prio` 的合法范围这些细节都要严格按照测试约定实现。
