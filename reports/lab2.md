# 实验二：地址空间与 mmap / munmap（第 4 章 地址空间）

## 一、实验目的

1. 理解 RISC-V Sv39 分页机制：三级页表、`satp` 寄存器、`sfence.vma`，以及内核与用户地址空间如何共存。
2. 掌握按「逻辑段（`MapArea`）」管理用户地址空间的方法：`MemorySet` 负责把虚拟页映射到物理页帧。
3. 在分页机制之上实现 `sys_mmap` / `sys_munmap`，并把第 3 章的 `sys_get_time`、`sys_trace` 从「裸指针直接访问」改写成「经页表翻译后访问」，正确处理跨页、越权与未映射等情况。

## 二、实验环境

| 项目 | 版本 / 说明 |
| --- | --- |
| 主机 | Windows 11 + GNU Make 4.4.1 + Git Bash |
| 目标平台 | `riscv64gc-unknown-none-elf` |
| 工具链 | `nightly-2024-02-25`（检查器固定版本），`rust-src`、`llvm-tools-preview` |
| 模拟器 | QEMU 8.2（`qemu-system-riscv64 -machine virt`） |
| 官方检查器 | rCore-Tutorial-Checker `7d61ec5`，用户程序 rCore-Tutorial-Test `a059366` |

## 三、实验要求分析

第 4 章的模板已经具备完整的分页基础设施（`mm/address.rs`、`frame_allocator.rs`、`page_table.rs`、`memory_set.rs`），并且 `sbrk` 已经通过 `change_program_brk` 调用 `shrink_to` / `append_to` 实现。需要补的是：

1. `sys_mmap(start, len, prot)`：在用户地址空间 `start` 处映射 `len` 字节，`prot` 中 bit0/1/2 分别是 R/W/X；参数非法或区间冲突返回 -1。
2. `sys_munmap(start, len)`：解除映射；`start`、`len` 必须页对齐，否则返回 -1。
3. `sys_get_time`：`TimeVal` 可能跨越两个页，必须逐字节经页表翻译后写入。
4. `sys_trace`：读写用户地址时必须检查页表项存在性与权限；未映射地址返回 -1，只读页写入返回 -1。
5. `sys_trace` 的 `Syscall` 分支需要在 TCB 中保存每个任务的系统调用计数（与第 3 章一致）。

测试程序给出的边界条件非常明确：

```rust
// ch4_mmap3：参数校验
assert_eq!(mmap(start - len, len + 1, prot), -1);   // len 未对齐
assert_eq!(mmap(start + len + 1, len, prot), -1);  // start 未对齐
assert_eq!(mmap(start + len, len, 0), -1);         // prot = 0
assert_eq!(mmap(start + len, len, prot | 8), -1);  // prot 有非法位

// ch4_unmap2：munmap 参数校验
assert_eq!(munmap(start, len + 1), -1);
assert_eq!(munmap(start + 1, len - 1), -1);

// ch4_trace1：权限与有效性
assert_eq!(None, trace_read(isize::MAX as usize as *const _));  // 未映射
assert_eq!(None, trace_read(0x80200000 as *const _));           // 内核地址
assert_eq!(-1, trace_write(只读 mmap 的地址, 0));                // 无写权限
```

## 四、设计与实现

### 4.1 `MemorySet::mmap` / `MemorySet::munmap`

`mmap` 复用已有的「逻辑段」抽象：新建一个 `MapArea`，类型为 `MapType::Framed`（分配新的物理页帧），并把 `prot` 翻译成 `MapPermission`。映射前先做对齐检查与冲突检查（与已有 `MapArea` 的虚拟页区间求交）。

```rust
pub fn mmap(&mut self, start_va: usize, len: usize, permission: MapPermission) -> bool {
    if len == 0 || start_va % PAGE_SIZE != 0 || len % PAGE_SIZE != 0 {
        return false;
    }
    let start_vpn: VirtPageNum = VirtAddr::from(start_va).floor();
    let end_vpn: VirtPageNum = VirtAddr::from(start_va + len).ceil();
    if end_vpn <= start_vpn {
        return false;
    }
    let conflict = self.areas.iter().any(|area| {
        area.vpn_range.get_start() < end_vpn && start_vpn < area.vpn_range.get_end()
    });
    if conflict {
        return false;
    }
    self.push(
        MapArea::new(start_va.into(), (start_va + len).into(), MapType::Framed, permission),
        None,
    );
    true
}
```

`munmap` 只在「恰好存在一个与请求区间完全相同的逻辑段」时成功，这既满足测试对 `-1` 的要求，也避免把 ELF 段、用户栈、`TrapContext` 这些系统内部区域误删。

```rust
pub fn munmap(&mut self, start_va: usize, len: usize) -> bool {
    if len == 0 || start_va % PAGE_SIZE != 0 || len % PAGE_SIZE != 0 {
        return false;
    }
    let start_vpn: VirtPageNum = VirtAddr::from(start_va).floor();
    let end_vpn: VirtPageNum = VirtAddr::from(start_va + len).ceil();
    let position = self.areas.iter().position(|area| {
        area.vpn_range.get_start() == start_vpn && area.vpn_range.get_end() == end_vpn
    });
    match position {
        Some(index) => {
            let mut area = self.areas.remove(index);
            area.unmap(&mut self.page_table);   // 清空 PTE，FrameTracker 随 MapArea 一起释放
            true
        }
        None => false,
    }
}
```

`unmap` 之后 `MapArea` 被 `remove` 出来，其 `data_frames: BTreeMap<VirtPageNum, FrameTracker>` 随之析构，物理页帧自动回收，不会泄漏。

### 4.2 系统调用层的参数校验

`sys_mmap` / `sys_munmap` 把「是否页对齐、`prot` 是否合法」交给内核统一判断，并转换成 `MapPermission`：

```rust
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    if len == 0 || prot == 0 || prot & !0b111 != 0 {
        return -1;
    }
    let mut permission = MapPermission::U;
    if prot & 0b001 != 0 { permission |= MapPermission::R; }
    if prot & 0b010 != 0 { permission |= MapPermission::W; }
    if prot & 0b100 != 0 { permission |= MapPermission::X; }
    if mmap_current(start, len, permission) { 0 } else { -1 }
}
```

### 4.3 经页表翻译访问用户内存

内核把所有物理内存恒等映射在自己的地址空间里，因此「用户虚拟地址 → 物理地址」翻译完成后，直接用物理页指针即可读写。我写了一个辅助函数：

```rust
fn user_ptr(token: usize, addr: usize, for_write: bool) -> Option<*mut u8> {
    let vpn = VirtAddr::from(addr).floor();
    let pte = PageTable::from_token(token).translate(vpn)?;
    if !pte.is_valid() || !pte.readable() {
        return None;
    }
    if for_write && !pte.writable() {
        return None;
    }
    let offset = addr & (PAGE_SIZE - 1);
    let bytes = pte.ppn().get_bytes_array();
    Some(&mut bytes[offset] as *mut u8)
}
```

- `translate` 只返回叶子 PTE，中间级为 `None`，天然处理了「未映射」；
- `readable()` / `writable()` 直接对应 PTE 的 R/W 位，从而让「只读映射写入」返回 -1；
- 内核地址（如 `0x80200000`）不在用户页表里，翻译失败，返回 -1。

`sys_get_time` 则按字节写入，天然支持 `TimeVal` 跨页：

```rust
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    let token = current_user_token();
    let us = get_time_us();
    let tv = TimeVal { sec: us / 1_000_000, usec: us % 1_000_000 };
    let raw = unsafe {
        core::slice::from_raw_parts(&tv as *const TimeVal as *const u8,
                                    core::mem::size_of::<TimeVal>())
    };
    for (offset, byte) in raw.iter().enumerate() {
        match user_ptr(token, ts as usize + offset, true) {
            Some(ptr) => unsafe { ptr.write_volatile(*byte) },
            None => return -1,
        }
    }
    0
}
```

### 4.4 每个任务的系统调用计数

与第 3 章一致：在 `TaskControlBlock` 中放 `syscall_count: [usize; MAX_SYSCALL_NUM]`（`MAX_SYSCALL_NUM = 500` 由模板 `config.rs` 给出），在 `syscall()` 分发前统一计数。第 4 章的 TCB 用 `Vec<TaskControlBlock>` 在堆上组织，因此不必担心 TCB 变大带来的栈压力。

## 五、测试结果

```sh
make -C ci-user test CHAPTER=4
```

`ch2b_*`、`ch3_*`、`ch3b_*`、`ch4_*`、`ch4b_*` 全部用户程序依次运行，检查器输出（关键行）：

```
Test 04_1 OK!
Test 04_4 test OK!
Test 04_5 ummap OK!
Test 04_6 ummap2 OK!
Test trace_1 OK!
Test sbrk almost OK!
Test passed: 13/13
```

`ch4_mmap1`、`ch4_mmap2`、`ch4b_sbrk` 的「应当报错」标记没有出现，`EXPECTED_4` 全部命中，`NOT_EXPECTED_4` 全部未出现。

## 六、遇到的问题与解决

1. **`munmap` 之后重新 `mmap` 同一段地址失败**：`ch4_unmap` 中先 `mmap(start, len)`、再 `munmap(start, len)`，随后又 `mmap(start - len, len + 1)`。最初的实现只按「起点相等」找段，`munmap` 只缩减区间而不删除 `MapArea`，导致后面 `mmap` 判为冲突。改为「完全匹配才删除整段」后，`areas` 里不再残留空段，冲突判断也就正确了。
2. **跨页的 `TimeVal`**：如果直接 `*ts = TimeVal { .. }`，当 `ts` 落在页末 8 字节之前时会写到相邻的、可能未映射的页，触发内核态缺页。按字节翻译写入即可。
3. **`trace_read` 与只读映射**：一开始只检查 PTE 是否有效，没有检查 R/W 位，导致 `trace_write` 能写进只读页，`ch4_trace1` 断言失败。补上 `readable()` / `writable()` 检查后通过。
4. **页表切换与 `sfence.vma`**：`MemorySet::activate()` 写 `satp` 后必须执行 `sfence.vma`，否则 TLB 里可能残留旧映射；这部分模板已给出，未作改动。

## 七、小结

本实验把「逻辑段」这一抽象用到了 `mmap`/`munmap` 上：新增区间就是新建 `MapArea` 并 `map`，释放区间就是 `unmap` 并让 `FrameTracker` 自动归还物理帧。同时体会到，一旦有了地址空间，内核就再也不能拿用户指针直接访问——**所有涉及用户内存的系统调用都必须先翻译、再检查权限**，这也是第 4 章 `trace` 与 `get_time` 需要重写的原因。
