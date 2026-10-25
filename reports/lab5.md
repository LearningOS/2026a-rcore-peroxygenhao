# 实验五：死锁检测（第 8 章 并发与同步）

## 一、实验目的

1. 理解内核态「线程 + 进程」两级结构：同一进程的多个线程共享地址空间、文件描述符表与资源表，各有独立的用户栈、内核栈与 `TrapContext`。
2. 掌握用阻塞 / 唤醒实现的内核同步原语：自旋锁 `MutexSpin`、阻塞锁 `MutexBlocking`、信号量 `Semaphore`、条件变量 `Condvar`。
3. 实现 **死锁检测**：在阻塞型互斥锁与信号量上维护「谁持有资源、谁在等什么」的等待图，发现环时向用户返回 `-0xdead` 而不是永久阻塞。
4. 通过 `ch8_deadlock_mutex1` / `ch8_deadlock_sem1` / `ch8_deadlock_sem2` 三个测试，验证「能发现真死锁、不误报假死锁」。

## 二、实验环境

| 项目 | 版本 / 说明 |
| --- | --- |
| 主机 | Windows 11 + GNU Make 4.4.1 + Git Bash |
| 目标平台 | `riscv64gc-unknown-none-elf` |
| 工具链 | `nightly-2024-02-25`、`rust-src`、`llvm-tools-preview` |
| 模拟器 | QEMU 8.2（`qemu-system-riscv64 -machine virt` + `virtio-blk-device`） |
| 官方检查器 | rCore-Tutorial-Checker `7d61ec5`，用户程序 rCore-Tutorial-Test `a059366` |

## 三、实验要求分析

第 8 章模板已经实现了线程创建/等待（`thread_create` / `gettid` / `waittid`）、阻塞型与自旋型互斥锁、信号量、条件变量、管道等功能，**唯一缺失的是死锁检测** —— `sys_enable_deadlock_detect` 只是一个返回 `-1` 的空壳。

三个测试程序把语义约定得非常清楚：

```rust
// ch8_deadlock_mutex1：同一个线程第二次 lock 同一把阻塞锁
enable_deadlock_detect(true);
let mid = mutex_blocking_create();
assert_eq!(mutex_lock(mid), 0);
assert_eq!(mutex_lock(mid), -0xdead);   // 自我死锁，必须被检测出来

// ch8_deadlock_sem1：三个线程互相持有并请求对方的资源，构成环
//   资源 sem1=1, sem2=2, sem3=1
//   tid1 持有 sem2，请求 sem1
//   tid2 持有 sem1、sem2，请求 sem3
//   tid3 持有 sem3，请求 sem2
// 期望：至少有一个线程的 waittid 返回值不为 0（即检测到死锁后以 -1 退出）

// ch8_deadlock_sem2：资源足够，不存在环
// 期望：所有线程正常退出，failed == 0（不能误报）
```

因此检测必须做到两件事：**真环要发现，无环不能误报**。

## 四、设计与实现

### 4.1 全局开关

死锁检测是一个全局策略，用一个原子布尔量即可（单核内核，`Relaxed` 足够）：

```rust
/// 检测到死锁时返回给用户态的错误码
pub const DEADLOCK_ERROR: isize = -0xdead;

static DEADLOCK_DETECT: AtomicBool = AtomicBool::new(false);

pub fn set_deadlock_detect(enabled: bool) { DEADLOCK_DETECT.store(enabled, Ordering::Relaxed); }
pub fn deadlock_detect_enabled() -> bool { DEADLOCK_DETECT.load(Ordering::Relaxed) }
```

`sys_enable_deadlock_detect(enabled)` 只负责翻转开关。

### 4.2 阻塞互斥锁的自我死锁

`MutexBlocking` 记录当前持有者的 tid。同一线程再次 `lock` 自己已持有的锁时，无论等多久都不可能成功，直接返回 `DEADLOCK_ERROR`：

```rust
pub struct MutexBlockingInner {
    locked: bool,
    holder: Option<usize>,          // 持有者 tid，用于死锁检测
    wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

fn lock(&self) -> isize {
    let tid = current_tid();
    let mut inner = self.inner.exclusive_access();
    if inner.locked {
        if deadlock_detect_enabled() && inner.holder == Some(tid) {
            return DEADLOCK_ERROR;   // 自我死锁
        }
        inner.wait_queue.push_back(current_task().unwrap());
        drop(inner);
        block_current_and_run_next();
        self.inner.exclusive_access().holder = Some(tid);  // 解锁者把锁移交给我们
    } else {
        inner.locked = true;
        inner.holder = Some(tid);
    }
    0
}
```

为此 `Mutex` trait 的 `lock` 从 `fn lock(&self)` 改成了 `fn lock(&self) -> isize`，`MutexSpin` 恒返回 0，`Condvar::wait` 中的重新加锁用 `let _ = mutex.lock();` 忽略结果（能从 `wait` 返回说明这把锁本来就在自己手里，不可能自锁）。

### 4.3 信号量的等待图

信号量与互斥锁不同：一个信号量可以被**多个**线程同时持有若干个单位（`count > 1` 时），所以「谁持有」是一张计数表，而不是单个持有者。在 `SemaphoreInner` 中增加：

```rust
pub struct SemaphoreInner {
    pub count: isize,
    pub wait_queue: VecDeque<Arc<TaskControlBlock>>,
    /// 谁持有本信号量的资源：(tid, 持有数量)
    pub holders: Vec<(usize, usize)>,
}
```

`down` 成功时记录持有者，`up` 时把调用者自己的一份还掉（调用者不在表里就跳过，例如主线程 `up` 一个自己没 `down` 过的栅栏信号量）。

`down` 在**将要阻塞**时做检测：

```rust
pub fn down(&self) -> isize {
    let tid = current_tid();
    let acquired = { /* count > 0 就立刻拿走并记账 */ };
    if acquired { return 0; }
    // 将要阻塞：检查阻塞是否会形成环
    if deadlock_detect_enabled() && would_deadlock(tid, self) {
        return DEADLOCK_ERROR;
    }
    {  // 负的 count 表示「欠一个单位」，对应的 up 会唤醒我们
        let mut inner = self.inner.exclusive_access();
        inner.count -= 1;
        inner.wait_queue.push_back(current_task().unwrap());
    }
    block_current_and_run_next();
    take_from(&mut self.inner.exclusive_access().holders, tid);  // 唤醒后正式持有
    0
}
```

### 4.4 等待图上的 DFS

等待图的两类边分别是：**信号量的 holders**（谁占着资源）和 **wait_queue**（谁在等这个资源）。检测时从「当前线程想拿的信号量」出发：

```rust
fn would_deadlock(tid: usize, sem: &Semaphore) -> bool {
    let sems = all_semaphores();          // 本进程的全部信号量
    visit(sem, tid, &sems, &mut Vec::new())
}

fn visit(sem: &Semaphore, tid: usize, sems: &[Arc<Semaphore>], visited: &mut Vec<usize>) -> bool {
    let id = sem as *const Semaphore as usize;
    if visited.contains(&id) { return false; }
    visited.push(id);
    // 注意：先 clone 出 holders 再释放 borrow，避免再次访问同一个 UPSafeCell 造成 panic
    let holders = sem.inner.exclusive_access().holders.clone();
    for (holder, count) in holders {
        if count == 0 { continue; }
        if holder == tid { return true; }              // 绕回请求者：成环
        for next in sems.iter() {
            if waits_for(next.as_ref(), holder) && visit(next.as_ref(), tid, sems, visited) {
                return true;                            // 顺着「持有者也在等别的资源」继续找
            }
        }
    }
    false
}
```

`waits_for(sem, tid)` 就是扫描 `sem.wait_queue` 里是否有该 tid 的任务。整个算法复杂度是 O(信号量数 × 持有者数)，而进程内的信号量通常只有个位数，完全可以接受。

### 4.5 与用户态约定的返回值

`sys_mutex_lock` / `sys_semaphore_down` 直接把内核里的返回值透传出去：

```rust
pub fn sys_semaphore_down(sem_id: usize) -> isize {
    ...
    sem.down()      // 0 或 DEADLOCK_ERROR(-0xdead)
}
```

用户库 `semaphore_down` / `mutex_lock` 返回 `isize`，测试程序据此判断：

```rust
if semaphore_down(sem_id) == -0xdead {
    sem_dealloc(gettid() as usize);
    println!("Deadlock detected. Test 08_sem1 failed!");
    exit(-1);
}
```

## 五、测试结果

```sh
make -C ci-user test CHAPTER=8
```

`ch8_usertest` 依次 fork + exec 全部 23 个测试，检查器输出（关键行）：

```
thread 1 running
thread 2 running
thread 3 running
deadlock test semaphore 1 OK!
deadlock test mutex 1 OK!
deadlock test semaphore 2 OK!
mpsc_sem passed!
sync_sem passed!
philosopher dining problem with mutex test passed!
race adder using spin mutex test passed!
threads test passed!
threads with arg test passed!
test_condvar passed!
pipetest passed!
file_test passed!
Test passed: 25/25
```

其中 `ch8_deadlock_sem1` 里能看到某个线程返回 `-0xdead` 后打印 `Deadlock detected. Test 08_sem1 failed!` 并以 `-1` 退出，而 `ch8_deadlock_sem2` 全部线程正常退出 —— 说明检测器既发现了真环，也没有误报。

## 六、遇到的问题与解决

1. **两个信号量死锁测试"卡住"，看起来像检测没生效**（本次实验最费时间的问题）：`ch8_deadlock_sem1/2` 只打印出 `thread 1/2/3 running` 就再无输出，既没有检测信息也没有 `exited`。一开始以为是等待图算法写错了，往内核里加调试输出后才发现：三个线程都正确地阻塞在同步栅栏信号量（`count` 变成 -3）上，而**主线程根本没有执行到 `sleep()` 之后的 `semaphore_up`**。

   继续追下去，问题出在用户库的 `sleep(ms)`：它是「`while get_time() < start + ms { yield_() }`」的忙等形式，而这一章的 `sys_get_time` 还是模板里返回 `-1` 的空壳。于是 `get_time()` 恒为 -1，`-1 < -1+500` 永远成立，主线程在死循环里不停 `yield`，永远不会去释放栅栏。把 `sys_get_time` 按第 5 章的方式实现（经页表逐字段写入 `TimeVal`）之后，两个测试立刻通过。

   这件事的教训是：**表面上"死锁检测失效"，实际是测试程序自身的驱动程序跑不下去**；排查时先确认被测行为真的被执行到了，再怀疑算法。
2. **死锁检测把普通信号量测试也弄崩了**：第一版 `down()` 里把「将要阻塞」处理成「不扣 count」，结果 `up()` 时 `count` 从 0 变成 1，`count <= 0` 的唤醒条件不成立，等待者永远不会被唤醒，`mpsc_sem`、`sync_sem` 等测试全部挂死。恢复「阻塞时先 `count -= 1`（负值表示欠账）、`up` 时 `count += 1` 并在 `count <= 0` 时唤醒一个」的经典语义后解决。
3. **检测开关必须是"每进程"的**：最初用一个全局 `AtomicBool`，`ch8_deadlock_*` 打开之后再也没关，后续 `ch8b_*` 的测试就在开着检测的状态下跑生产者/消费者这类"信号量只用来通知、并不表示占有"的模式，`holders` 计数被反复 `down` 越记越大，导致大量误报。改成在 `ProcessControlBlockInner` 里放一个 `deadlock_detect` 字段后，开关随进程生命周期自然隔离，误报消失。
4. **`RefCell` 重入 panic**：在持有 `self.inner` 借用的情况下调用 `would_deadlock`，DFS 里又要访问同一个 `Semaphore` 的 `inner`，触发 `already borrowed` panic；调试打印时还因为一次 `println!` 里写了两次 `exclusive_access()` 复现了同类错误。解决办法是所有需要再次访问同一个 `UPSafeCell` 的地方都先**取出值（clone）**、立即结束借用。
5. **`Mutex` trait 的签名**：要把「自我死锁」信息带回用户态，只能让 `lock` 有返回值。改动波及 `MutexSpin`、`Condvar::wait` 两处调用点，但换来的是语义清晰：内核同步原语直接返回错误码，系统调用层只做透传。
6. **误报的排查**：`ch8_deadlock_sem2` 一开始会误判成死锁，原因是 `holders` 里保留了「数量已经减到 0」的陈旧条目。在 DFS 中跳过 `count == 0` 的条目（并在 `up` 时把持有数减到 0 而不删除条目）后误报消失。

## 七、小结

死锁检测的本质是**在资源的持有关系与等待关系构成的图上找环**。互斥锁的持有者唯一，所以只需要一个 tid 就能发现自我死锁；信号量可以部分持有，因此需要维护计数表，并在 `down` 将要阻塞的那一刻做一次 DFS。这个实验也说明了同步原语的状态机（`count` 的正负、唤醒与移交）必须严格自洽，任何一个小改动都可能让「等不到唤醒」的 bug 以别的形式暴露出来。
