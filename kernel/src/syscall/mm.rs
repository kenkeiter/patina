//! Memory syscalls: thin wrappers over `mm::vm::AddressSpace`.

use patina_abi::errno::{EFAULT, EINVAL, ENOMEM, Errno};

use super::{SysResult, with_space};
use crate::mm::PAGE_SIZE;
use crate::mm::vm::access_ok;

/// Pages per `mincore` step (one stack buffer; the address-space lock is
/// dropped between steps).
const MINCORE_CHUNK: usize = 1024;

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

/// `mincore(addr, len, vec)`, as Linux `mm/mincore.c` (`sys_mincore`):
/// `addr` unaligned → EINVAL; `[addr, addr+len)` outside the user half →
/// ENOMEM; `vec` (one byte per page) outside it → EFAULT; then the range
/// is walked VMA by VMA in chunks of at most [`MINCORE_CHUNK`] pages,
/// each copied out before the next, so a hole stops the walk with ENOMEM
/// (earlier chunks already written) and nothing is allocated in
/// proportion to `len` (Go reserves tens of GiB `PROT_NONE`).
pub fn mincore(addr: usize, len: usize, vec: usize) -> SysResult {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno(EINVAL));
    }
    if !access_ok(addr, len) {
        return Err(Errno(ENOMEM));
    }
    let mut left = len.div_ceil(PAGE_SIZE);
    if !access_ok(vec, left) {
        return Err(Errno(EFAULT));
    }
    let mut buf = [0u8; MINCORE_CHUNK];
    let (mut addr, mut vec) = (addr, vec);
    while left > 0 {
        let n = with_space(|s| {
            let n = s.mincore_chunk(addr, left, &mut buf)?;
            s.copy_to_user(vec, &buf[..n]).map_err(|_| Errno(EFAULT))?;
            Ok::<usize, Errno>(n)
        })?;
        left -= n;
        vec += n;
        addr += n * PAGE_SIZE;
    }
    Ok(0)
}
