//! The vDSO and its vvar page (decision D75, design §7 "vDSO").
//!
//! The image is `patina_vdso::IMAGE` (built by `crates/vdso/build.rs`).
//! [`init`] allocates the vvar frame and publishes the clock parameters;
//! [`map`] maps the image read-exec at [`VDSO_BASE`] (`[vdso]`) and the
//! vvar frame read-only right after it (`[vvar]`, where the image's code
//! expects it). The loader then puts `AT_SYSINFO_EHDR` = [`VDSO_BASE`] in
//! the auxv. `patina.novdso=1` skips all of this (Go then takes the
//! syscalls).
//!
//! The vvar frame is kernel-owned: it is written through the direct map
//! under a seqlock (`patina_vdso::vvar`) and never freed, even if the
//! program unmaps it (`mm::tlb::Shootdown::free_frame` skips it).

use core::sync::atomic::{AtomicUsize, Ordering};

use patina_abi::mm::ProtFlags;
use patina_vdso::vvar::{CLOCK_MODE_COUNTER, Vvar};

use crate::mm::vm::{AddressSpace, VmaKind};
use crate::mm::{PAGE_SIZE, PhysAddr, frame, phys_to_virt};
use crate::sync::SpinLock;

/// User address of the vDSO image (`AT_SYSINFO_EHDR`): above the initial
/// stack (`loader::STACK_TOP`), away from Go's mmap hints and the
/// top-down mmap area.
pub const VDSO_BASE: usize = 0x7fff_f800_0000;

/// Physical address of the vvar frame; 0 = no vDSO.
static VVAR_PA: AtomicUsize = AtomicUsize::new(0);
/// Serializes vvar writers.
static WRITE: SpinLock<()> = SpinLock::new(());

fn vvar() -> Option<&'static Vvar> {
    let pa = VVAR_PA.load(Ordering::Acquire);
    // SAFETY: the vvar frame is allocated once in `init`, zeroed (a valid
    // `Vvar`: all atomics) and never freed; it is reached through the
    // direct map.
    (pa != 0).then(|| unsafe { &*phys_to_virt(PhysAddr(pa)).as_ptr::<Vvar>() })
}

/// Whether `pa` is the shared vvar frame (which must never be freed).
pub fn is_vvar_frame(pa: PhysAddr) -> bool {
    let v = VVAR_PA.load(Ordering::Relaxed);
    v != 0 && pa.0 == v
}

/// The vvar frame, if the vDSO is enabled.
pub fn vvar_frame() -> Option<PhysAddr> {
    let v = VVAR_PA.load(Ordering::Acquire);
    (v != 0).then_some(PhysAddr(v))
}

/// Allocate the vvar page and publish the clock (the realtime offset is
/// `epoch_ns`, as `wallclock::epoch_ns` before any `time` op) and the RNG
/// generation. Call once, after the RNG is seeded.
pub fn init(epoch_ns: u64) -> bool {
    let Some(pa) = frame::alloc_frames_zeroed(0) else {
        return false;
    };
    let (base, scale) = crate::arch::timer::vdso_clock();
    // SAFETY: a fresh zeroed frame is a valid `Vvar` (all atomics).
    let v: &Vvar = unsafe { &*phys_to_virt(pa).as_ptr::<Vvar>() };
    let _g = WRITE.lock();
    v.write(|v| {
        v.counter_base.store(base, Ordering::Relaxed);
        v.scale.store(scale, Ordering::Relaxed);
        v.realtime_offset.store(epoch_ns, Ordering::Relaxed);
        v.clock_mode.store(
            if scale != 0 { CLOCK_MODE_COUNTER } else { 0 },
            Ordering::Relaxed,
        );
    });
    // Fresh vgetrandom states are zeroed (generation 0): start at 1 so
    // their first call fetches a key.
    v.rng_generation.store(1, Ordering::Release);
    v.rng_ready.store(1, Ordering::Release);
    VVAR_PA.store(pa.0, Ordering::Release);
    true
}

/// `CLOCK_REALTIME` was stepped: realtime = monotonic + `offset`.
pub fn set_realtime_offset(offset: u64) {
    if let Some(v) = vvar() {
        let _g = WRITE.lock();
        v.write(|v| v.realtime_offset.store(offset, Ordering::Relaxed));
    }
}

/// The kernel RNG was reseeded: vgetrandom states fetch a new key.
pub fn rng_reseeded() {
    if let Some(v) = vvar() {
        v.rng_generation.fetch_add(1, Ordering::Release);
    }
}

/// Map the image and the vvar page into `space`. Returns the image base
/// (`AT_SYSINFO_EHDR`), or `None` (and maps nothing) if [`init`] did not
/// run or memory ran out.
pub fn map(space: &mut AddressSpace) -> Option<usize> {
    let vvar_pa = vvar_frame()?;
    let img = patina_vdso::IMAGE;
    let l = patina_vdso::elf::layout(img)?;
    let text_end = VDSO_BASE + l.text_len;
    let vvar_va = VDSO_BASE + l.vvar_offset;
    space
        .add_vma(
            VDSO_BASE,
            text_end,
            ProtFlags::READ | ProtFlags::EXEC,
            VmaKind::Vdso,
        )
        .ok()?;
    space
        .add_vma(vvar_va, vvar_va + PAGE_SIZE, ProtFlags::READ, VmaKind::Vvar)
        .ok()?;
    for off in (0..l.text_len).step_by(PAGE_SIZE) {
        let pa = frame::alloc_frames_zeroed(0)?;
        let n = l.file_len.saturating_sub(off).min(PAGE_SIZE);
        // SAFETY: a fresh frame via the direct map; n <= PAGE_SIZE and
        // off + n <= file_len <= img.len() (checked by `layout`).
        unsafe {
            core::ptr::copy_nonoverlapping(
                img.as_ptr().add(off),
                phys_to_virt(pa).as_mut_ptr::<u8>(),
                n,
            )
        };
        space.map_filled(VDSO_BASE + off, pa).ok()?;
    }
    space.map_filled(vvar_va, vvar_pa).ok()?;
    Some(VDSO_BASE)
}
