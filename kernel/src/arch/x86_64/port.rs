//! Port I/O.

use core::arch::asm;

/// # Safety
/// Port I/O can have arbitrary device side effects. (Not `nomem`: port
/// accesses also order against memory accesses, e.g. DMA buffers.)
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    // SAFETY: caller's responsibility.
    unsafe { asm!("in al, dx", out("al") v, in("dx") port, options(nostack, preserves_flags)) };
    v
}

/// # Safety
/// Port I/O can have arbitrary device side effects. (Not `nomem`: port
/// accesses also order against memory accesses, e.g. DMA buffers.)
#[inline]
pub unsafe fn outb(port: u16, v: u8) {
    // SAFETY: caller's responsibility.
    unsafe { asm!("out dx, al", in("dx") port, in("al") v, options(nostack, preserves_flags)) };
}

/// # Safety
/// Port I/O can have arbitrary device side effects. (Not `nomem`: port
/// accesses also order against memory accesses, e.g. DMA buffers.)
#[inline]
pub unsafe fn outw(port: u16, v: u16) {
    // SAFETY: caller's responsibility.
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") v, options(nostack, preserves_flags)) };
}

/// # Safety
/// Port I/O can have arbitrary device side effects. (Not `nomem`: port
/// accesses also order against memory accesses, e.g. DMA buffers.)
#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    let v: u32;
    // SAFETY: caller's responsibility.
    unsafe { asm!("in eax, dx", out("eax") v, in("dx") port, options(nostack, preserves_flags)) };
    v
}

/// # Safety
/// Port I/O can have arbitrary device side effects. (Not `nomem`: port
/// accesses also order against memory accesses, e.g. DMA buffers.)
#[inline]
pub unsafe fn outl(port: u16, v: u32) {
    // SAFETY: caller's responsibility.
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") v, options(nostack, preserves_flags)) };
}
