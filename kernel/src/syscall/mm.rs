//! Memory syscalls: thin wrappers over `mm::vm::AddressSpace`.

use super::{SysResult, with_space};

pub fn mmap(addr: usize, len: usize, prot: u32, flags: u32, fd: i32, off: u64) -> SysResult {
    with_space(|s| s.mmap(addr, len, prot, flags, fd, off)).map(|a| a as isize)
}

pub fn munmap(addr: usize, len: usize) -> SysResult {
    with_space(|s| s.munmap(addr, len)).map(|_| 0)
}

pub fn mprotect(addr: usize, len: usize, prot: u32) -> SysResult {
    with_space(|s| s.mprotect(addr, len, prot)).map(|_| 0)
}

pub fn madvise(addr: usize, len: usize, advice: i32) -> SysResult {
    with_space(|s| s.madvise(addr, len, advice)).map(|_| 0)
}

pub fn mremap(old: usize, old_len: usize, new_len: usize, flags: u32) -> SysResult {
    with_space(|s| s.mremap(old, old_len, new_len, flags)).map(|a| a as isize)
}

pub fn brk(req: usize) -> SysResult {
    Ok(with_space(|s| s.brk(req)) as isize)
}

pub fn mincore(addr: usize, len: usize, vec: usize) -> SysResult {
    with_space(|s| {
        let v = s.mincore(addr, len)?;
        s.copy_to_user(vec, &v)
    })
    .map(|_| 0)
}
