# 实验四：文件系统系统调用 fstat / linkat / unlinkat（第 6 章 文件系统与 I/O）

## 一、实验目的

1. 理解 rCore 的 `easy-fs` 磁盘布局：超级块、inode 位图、inode 区、数据位图、数据区，以及 `DiskInode` / `DirEntry` 的按字节序列化方式。
2. 掌握「文件系统层（`easy-fs`）↔ VFS 层（`OSInode` / `File` trait）↔ 系统调用层」三层结构的分工。
3. 实现 `sys_fstat`、`sys_linkat`、`sys_unlinkat` 三个系统调用，并在 `easy-fs` 中补上对应的 `stat` / `link` / `unlink` 能力。
4. 通过硬链接计数（`nlink`）实现「多个目录项共享一个 inode」，并在链接数归零时回收数据块和 inode。

## 二、实验环境

| 项目 | 版本 / 说明 |
| --- | --- |
| 主机 | Windows 11 + GNU Make 4.4.1 + Git Bash |
| 目标平台 | `riscv64gc-unknown-none-elf` |
| 工具链 | `nightly-2024-02-25`、`rust-src`、`llvm-tools-preview` |
| 模拟器 | QEMU 8.2（`qemu-system-riscv64 -machine virt`，附带 `virtio-blk-device` 磁盘） |
| 文件系统镜像 | `easy-fs-fuse` 把用户程序 ELF 打包成 `fs.img` |
| 官方检查器 | rCore-Tutorial-Checker `7d61ec5`，用户程序 rCore-Tutorial-Test `a059366` |

## 三、实验要求分析

第 6 章模板已经具备完整的文件系统框架：块设备驱动（`virtio_blk`）、块缓存（`get_block_cache`）、`easy-fs` 的 inode/目录/读写、内核侧的 `OSInode` 与 `fd_table`、以及 `openat`/`close`/`read`/`write`。需要补的是：

| 系统调用 | 语义 | 关键要求 |
| --- | --- | --- |
| `sys_fstat(fd, stat)` | 取已打开文件的元数据 | `mode` 区分文件/目录，`nlink` 为硬链接数 |
| `sys_linkat(old, new)` | 为 `old` 建一个名为 `new` 的硬链接 | 不复制数据，只新增目录项并增加 `nlink` |
| `sys_unlinkat(name)` | 删除名为 `name` 的目录项 | `nlink` 减一；为 0 时释放数据块与 inode |

测试程序把细节写得很明确：

```rust
// ch6_file1：新建文件的 nlink 必须是 1，mode 必须是 StatMode::FILE
assert_eq!(stat.mode, StatMode::FILE);
assert_eq!(stat.nlink, 1);

// ch6_file2：link 三次后 nlink == 4；unlink 原文件后仍可通过链接读数据，
//            且 ino/dev 保持一致，nlink 依次递减
// ch6_file3：反复 create + 写 50 次 + unlink 十轮，
//            要求 unlink 真正回收空间，否则后续 create 会失败
```

## 四、设计与实现

### 4.1 给 DiskInode 增加硬链接计数

硬链接的本质是「多个目录项指向同一个 inode」，因此链接计数必须持久化在磁盘 inode 上：

```rust
/// A disk inode
#[repr(C)]
pub struct DiskInode {
    pub size: u32,
    /// number of directory entries that point at this inode (hard links)
    pub nlink: u32,
    pub direct: [u32; INODE_DIRECT_COUNT],
    pub indirect1: u32,
    pub indirect2: u32,
    type_: DiskInodeType,
}

pub fn initialize(&mut self, type_: DiskInodeType) {
    self.size = 0;
    self.nlink = 1;      // 新建的 inode 自带一个目录项
    ...
}
```

`DiskInode` 变大后每块能放的 inode 数会变化，但 `get_disk_inode_pos` 用 `size_of::<DiskInode>()` 动态计算，`easy-fs-fuse` 与内核又是用同一份代码，所以磁盘布局始终自洽。

### 4.2 inode 号与目录项定位

`fstat` 需要返回 inode 号，而 `Inode` 只保存了 `(block_id, block_offset)`，因此要在 `EasyFileSystem` 中补上反查函数，并补上 inode 回收：

```rust
/// 由 (block_id, block_offset) 反推 inode id
pub fn get_inode_id(&self, block_id: u32, block_offset: usize) -> u32 {
    let inode_size = core::mem::size_of::<DiskInode>();
    let inodes_per_block = (BLOCK_SZ / inode_size) as u32;
    (block_id - self.inode_area_start_block) * inodes_per_block
        + (block_offset / inode_size) as u32
}

/// 回收 inode id
pub fn dealloc_inode(&mut self, inode_id: u32) {
    self.inode_bitmap.dealloc(&self.block_device, inode_id as usize)
}
```

### 4.3 easy-fs：stat / link / unlink

```rust
/// 统计信息：(inode id, 硬链接数, 是否目录)
pub fn stat(&self) -> (u32, u32, bool) {
    let fs = self.fs.lock();
    let inode_id = fs.get_inode_id(self.block_id as u32, self.block_offset);
    let (nlink, is_dir) = self.read_disk_inode(|inode| (inode.nlink, inode.is_dir()));
    (inode_id, nlink, is_dir)
}

pub fn link(&self, name: &str, inode_id: u32) -> Option<()> {
    let mut fs = self.fs.lock();
    // 名字不能已存在
    if self.read_disk_inode(|inode| self.find_inode_id(name, inode)).is_some() {
        return None;
    }
    // 目标 inode 的链接数 +1
    let (block_id, offset) = fs.get_disk_inode_pos(inode_id);
    get_block_cache(block_id as usize, Arc::clone(&self.block_device))
        .lock()
        .modify(offset, |inode: &mut DiskInode| inode.nlink += 1);
    // 在目录末尾追加一个目录项
    self.modify_disk_inode(|dir| {
        let file_count = (dir.size as usize) / DIRENT_SZ;
        let new_size = (file_count + 1) * DIRENT_SZ;
        self.increase_size(new_size as u32, dir, &mut fs);
        let dirent = DirEntry::new(name, inode_id);
        dir.write_at(file_count * DIRENT_SZ, dirent.as_bytes(), &self.block_device);
    });
    block_cache_sync_all();
    Some(())
}
```

`unlink` 要做三件事：找到目录项 → 减计数（归零则 `clear_size` 释放数据块并 `dealloc_inode`）→ 用「最后一个目录项覆盖被删目录项」的方式删除，并把目录大小减少一个 `DIRENT_SZ`：

```rust
pub fn unlink(&self, name: &str) -> Option<()> {
    let mut fs = self.fs.lock();
    let (index, inode_id) = self.read_disk_inode(|dir| { /* 扫描目录项，返回 (下标, inode id) */ })?;
    let (block_id, offset) = fs.get_disk_inode_pos(inode_id);
    let mut inode_freed = false;
    let freed = get_block_cache(block_id as usize, Arc::clone(&self.block_device))
        .lock()
        .modify(offset, |inode: &mut DiskInode| {
            inode.nlink -= 1;
            if inode.nlink == 0 {
                inode_freed = true;
                inode.clear_size(&self.block_device)   // 返回要回收的数据块
            } else {
                Vec::new()
            }
        });
    for block in freed { fs.dealloc_data(block); }
    if inode_freed { fs.dealloc_inode(inode_id); }
    // 用最后一个目录项覆盖待删除项，再缩小目录
    self.modify_disk_inode(|dir| {
        let file_count = (dir.size as usize) / DIRENT_SZ;
        let mut last = DirEntry::empty();
        dir.read_at((file_count - 1) * DIRENT_SZ, last.as_bytes_mut(), &self.block_device);
        dir.write_at(index * DIRENT_SZ, last.as_bytes(), &self.block_device);
        dir.size -= DIRENT_SZ as u32;
    });
    block_cache_sync_all();
    Some(())
}
```

用「最后一个目录项覆盖」而不是「整体前移」，可以保证删除是 O(1) 次块写入，代价只是目录项顺序会变 —— 而目录本来就没有顺序语义。

### 4.4 内核 VFS 层

`fd_table` 里存的是 `Arc<dyn File>`，因此把 `stat` 提到 `File` trait 上是最干净的：

```rust
pub trait File: Send + Sync {
    ...
    /// the metadata of the file
    fn stat(&self) -> Stat;
}
```

`OSInode` 转调 easy-fs 的 `stat()`，并把 `StatMode` 填好；`Stdin`/`Stdout` 返回一个 `StatMode::NULL` 的占位值。`Stat` 之前没有构造函数（`pad` 是私有字段），这里补一个：

```rust
impl Stat {
    /// Build a `Stat` from its significant fields, leaving the padding zeroed
    pub fn new(dev: u64, ino: u64, mode: StatMode, nlink: u32) -> Self {
        Self { dev, ino, mode, nlink, pad: [0; 7] }
    }
}
```

硬链接/删除只作用于根目录，所以在 `fs/inode.rs` 里给出两个语义清晰的入口：

```rust
pub fn link_file(old_name: &str, new_name: &str) -> bool {
    match ROOT_INODE.find(old_name) {
        Some(inode) => ROOT_INODE.link(new_name, inode.inode_id()).is_some(),
        None => false,
    }
}
pub fn unlink_file(name: &str) -> bool {
    ROOT_INODE.unlink(name).is_some()
}
```

### 4.5 系统调用层

`sys_fstat` 把内核里的 `Stat` 按字节拷贝到用户空间（用 `translated_byte_buffer` 处理跨页）：

```rust
pub fn sys_fstat(fd: usize, st: *mut Stat) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() { return -1; }
    let stat = match &inner.fd_table[fd] {
        Some(file) => file.stat(),
        None => return -1,
    };
    drop(inner);
    let bytes = unsafe {
        core::slice::from_raw_parts(&stat as *const Stat as *const u8,
                                    core::mem::size_of::<Stat>())
    };
    let mut copied = 0usize;
    for buffer in translated_byte_buffer(token, st as *const u8, bytes.len()) {
        let len = buffer.len();
        buffer.copy_from_slice(&bytes[copied..copied + len]);
        copied += len;
    }
    0
}
```

`sys_linkat` / `sys_unlinkat` 只是把用户路径取出来（`translated_str`）后转交给 VFS 层。

## 五、测试结果

```sh
make -C ci-user test CHAPTER=6
```

检查器输出（关键行）：

```
Test fstat OK!
Test link OK!
test iteration 0
...
test iteration 9
Test mass open/unlink OK!
file_test passed!
Test passed: 27/27
```

`ch6_file1`（fstat）、`ch6_file2`（link/unlink 与 nlink 计数）、`ch6_file3`（大量 create/unlink 的空间回收）以及 `ch6b_filetest_simple` 全部通过，`NOT_EXPECTED` 中的错误标记均未出现。

## 六、遇到的问题与解决

1. **`Stat` 无法构造**：`Stat` 的 `pad` 字段是私有的，系统调用层没法直接写结构体字面量，编译不过。补一个 `Stat::new(...)` 构造函数即可。
2. **`File` trait 上没有 `stat`**：`fd_table` 中是 `Arc<dyn File>`，想拿元数据只能靠 downcast 或者扩展 trait。扩展 trait 更符合面向接口的设计，于是给 `File` 加上 `fn stat(&self) -> Stat`，`Stdin`/`Stdout` 返回 `StatMode::NULL` 的占位值。
3. **`File` 与 `Stat` 的定义顺序**：`Stat` 原本在 `fs/mod.rs` 中定义在 trait 之后、`bitflags!` 之前，把 `stat` 加进 trait 时出现了「类型未定义」的错误，调整顺序后解决。
4. **`unlink` 不回收空间导致后续 create 失败**：`ch6_file3` 每轮写入 50 × 约 3 KB 数据，十轮下来约 1.5 MB。最初 `unlink` 只删目录项、不释放数据块，几轮之后数据位图耗尽，`open(CREATE)` 返回 -1，程序 panic。补上「`nlink` 归零时 `clear_size` + `dealloc_data` + `dealloc_inode`」之后，空间可以循环使用。
5. **目录项删除方式**：如果简单地把后面的目录项整体前移，就要对目录做大量读写；改成「最后一个覆盖被删项 + 缩小 size」后每次删除只需两次块写入。

## 七、小结

本实验的关键在于理解 **inode 与目录项的分离**：文件的数据属于 inode，名字属于目录项，硬链接就是「多个名字 → 同一个 inode」。由此可以自然推出 `nlink` 计数的必要性：只有最后一个名字被删除时，inode 与数据块才能回收。另外，`easy-fs`、VFS、syscall 三层各司其职的结构也让改动很局部：磁盘格式的改动在 `easy-fs/src/layout.rs`，目录操作在 `vfs.rs`，句柄语义在 `fs/inode.rs`，用户接口在 `syscall/fs.rs`。
