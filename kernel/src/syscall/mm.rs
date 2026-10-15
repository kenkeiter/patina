//! Memory syscalls: thin wrappers over `mm::vm::AddressSpace`.

use patina_abi::errno::{EFAULT, EINVAL, ENOMEM, Errno};
use patina_abi::mm::{
    MCL_CURRENT, MCL_FUTURE, MCL_ONFAULT, MLOCK_ONFAULT, MS_ASYNC, MS_INVALIDATE, MS_SYNC,
};

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

/// `msync(addr, len, flags)`: there are no file mappings, so nothing is
/// written back; the arguments are checked as Linux `mm/msync.c`: flags
/// outside `MS_ASYNC|MS_INVALIDATE|MS_SYNC` → EINVAL, `addr` unaligned →
/// EINVAL, `MS_ASYNC` with `MS_SYNC` → EINVAL, the rounded range
/// overflowing → ENOMEM, an empty range → 0, a range not fully mapped →
/// ENOMEM.
pub fn msync(addr: usize, len: usize, flags: i32) -> SysResult {
    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0 {
        return Err(Errno(EINVAL));
    }
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno(EINVAL));
    }
    if flags & MS_ASYNC != 0 && flags & MS_SYNC != 0 {
        return Err(Errno(EINVAL));
    }
    let len = len.checked_add(PAGE_SIZE - 1).ok_or(Errno(ENOMEM))? & !(PAGE_SIZE - 1);
    let end = addr.checked_add(len).ok_or(Errno(ENOMEM))?;
    if len == 0 {
        return Ok(0);
    }
    if !with_space(|s| s.is_covered(addr, end)) {
        return Err(Errno(ENOMEM));
    }
    Ok(0)
}

/// The range checks of Linux `mm/mlock.c` (`do_mlock`/`munlock` page
/// rounding with wrap-around, then `apply_vma_lock_flags`): an end that
/// wraps → EINVAL, an empty range → 0, a hole → ENOMEM. Locking itself is
/// accepted and ignored (D57: nothing is ever paged out); `RLIMIT_MEMLOCK`
/// is not enforced.
fn lock_range(start: usize, len: usize) -> SysResult {
    let len = len
        .wrapping_add(start & (PAGE_SIZE - 1))
        .wrapping_add(PAGE_SIZE - 1)
        & !(PAGE_SIZE - 1);
    let start = start & !(PAGE_SIZE - 1);
    let end = start.wrapping_add(len);
    if end < start {
        return Err(Errno(EINVAL));
    }
    if end == start {
        return Ok(0);
    }
    if !with_space(|s| s.is_covered(start, end)) {
        return Err(Errno(ENOMEM));
    }
    Ok(0)
}

/// `mlock(addr, len)` / `munlock(addr, len)`.
pub fn mlock(addr: usize, len: usize) -> SysResult {
    lock_range(addr, len)
}

/// `mlock2(addr, len, flags)`: flags other than `MLOCK_ONFAULT` → EINVAL.
pub fn mlock2(addr: usize, len: usize, flags: u32) -> SysResult {
    if flags & !MLOCK_ONFAULT != 0 {
        return Err(Errno(EINVAL));
    }
    lock_range(addr, len)
}

/// `mlockall(flags)`: 0, unknown bits, or `MCL_ONFAULT` alone → EINVAL.
pub fn mlockall(flags: i32) -> SysResult {
    if flags == 0 || flags & !(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT) != 0 || flags == MCL_ONFAULT
    {
        return Err(Errno(EINVAL));
    }
    Ok(0)
}
