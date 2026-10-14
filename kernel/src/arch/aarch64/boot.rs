//! aarch64 entry: Linux arm64 `Image` header, EL2->EL1 drop, boot page
//! tables, MMU enable, and the high-VA Rust entry [`early_main`].
//!
//! Everything up to the jump to `__boot_high` runs at the physical load
//! address with the MMU off, so it is written in assembly and uses only
//! PC-relative addressing (`adr`/`adrp`). With the MMU off all data accesses
//! are Device-nGnRnE (non-cacheable), so after zeroing `.bss` and building
//! the tables we invalidate those lines to the PoC before turning caches on.
//!
//! Boot tables (2 MiB blocks, from a 16-page pool in `.bss.boot_pt`):
//! * TTBR0: identity map of the kernel image (only needed to survive the
//!   instruction that turns the MMU on; parked on an empty table later).
//! * TTBR1: `KERNEL_VA_BASE -> load_phys` for the image, and
//!   `PHYS_OFFSET + pa -> pa` for the two 2 MiB blocks starting at the one
//!   containing the DTB (`x0`), so the DTB is readable wherever the VMM put it.

use core::arch::global_asm;

use super::dtb::{self, UartKind};
use super::{
    KERNEL_SIZE, KERNEL_VA_BASE, LOAD_PHYS, PHYS_OFFSET, console, cpu, gic, layout, paging, power,
    smp, timer,
};
use crate::bootinfo::{BootInfo, MemKind, MemoryMap, PhysRange, PlatformInfo};
use crate::mm::{PhysAddr, frame, heap};
use crate::sync::OnceCell;
use core::sync::atomic::Ordering;

/// SCTLR_EL1 with the MMU and caches off: RES1 bits only (29, 28, 23, 22, 20, 11).
const SCTLR_EL1_RES1: u64 = (1 << 29) | (1 << 28) | (1 << 23) | (1 << 22) | (1 << 20) | (1 << 11);
/// SCTLR_EL1 with the MMU on: M, C, SA, SA0, I, DZE (EL0 `dc zva`, used by
/// Go's memclr), UCT (EL0 CTR_EL0 reads), nTWI/nTWE (EL0 WFI/WFE not
/// trapped). SPAN (bit 23) stays set here; `cpu::enable_pan` clears it when
/// FEAT_PAN exists (it is RES1 otherwise).
const SCTLR_EL1_MMU_ON: u64 = SCTLR_EL1_RES1
    | (1 << 0) // M
    | (1 << 2) // C
    | (1 << 3) // SA
    | (1 << 4) // SA0
    | (1 << 12) // I
    | (1 << 14) // DZE
    | (1 << 15) // UCT
    | (1 << 16) // nTWI
    | (1 << 18); // nTWE

/// TCR_EL1: T0SZ = T1SZ = 16 (48-bit), 4 KiB granules (TG0 = 0b00,
/// TG1 = 0b10), inner-shareable write-back walks. IPS is filled in at boot
/// from ID_AA64MMFR0_EL1.PARange (capped at 48 bits).
const TCR_EL1_BASE: u64 = 16 // T0SZ
    | (0b01 << 8) // IRGN0 WB RA/WA
    | (0b01 << 10) // ORGN0
    | (0b11 << 12) // SH0 inner
    | (16 << 16) // T1SZ
    | (0b01 << 24) // IRGN1
    | (0b01 << 26) // ORGN1
    | (0b11 << 28) // SH1
    | (0b10 << 30); // TG1 = 4 KiB

/// Boot-table block descriptor for Normal memory, RWX at EL1 (UXN set):
/// AttrIndx 1, SH inner, AF, block.
const BOOT_BLOCK_RWX: u64 = (1 << 54) | (1 << 10) | (0b11 << 8) | (1 << 2) | 0b01;
/// Same, but also PXN (data only).
const BOOT_BLOCK_RW: u64 = BOOT_BLOCK_RWX | (1 << 53);

const BOOT_PT_PAGES: usize = 16;

global_asm!(
    r#"
    .section .text.head, "ax"
    .global _start
_start:
    // Linux arm64 Image header (Documentation/arch/arm64/booting.rst).
    b       _start_real                 // code0
    .long   0                           // code1
    .quad   0                           // text_offset
    .quad   __image_size                // image_size (incl. bss)
    .quad   0xa                         // flags: LE, 4K pages, placement anywhere
    .quad   0                           // res2
    .quad   0                           // res3
    .quad   0                           // res4
    .ascii  "ARM\x64"                   // magic
    .long   0                           // res5

    .section .text.boot, "ax"
_start_real:
    mov     x19, x0                     // x19 = DTB physical address
    msr     daifset, #0xf

    bl      __el2_to_el1
    // ---- EL1, MMU off.
    ldr     x9, ={sctlr_res1}
    msr     sctlr_el1, x9
    mov     x9, #(3 << 20)              // CPACR_EL1.FPEN: no FP/SIMD traps
    msr     cpacr_el1, x9
    msr     tpidr_el1, xzr              // logical CPU id 0
    isb

    adrp    x20, _start                 // x20 = load_phys
    tst     x20, #0x1fffff
    b.ne    __boot_hang                 // must be 2 MiB aligned

    // ---- Zero .bss (invalidate first so no stale dirty line can be
    // written back over it later).
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
    bl      __inval_dcache_range
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
3:  cmp     x0, x1
    b.hs    4f
    stp     xzr, xzr, [x0], #16
    b       3b
4:
    // ---- Boot page tables.
    adrp    x21, __boot_pt_pool         // x21 = next free pool page
    add     x21, x21, :lo12:__boot_pt_pool
    add     x22, x21, #({pt_pages} * 4096)   // x22 = pool end
    mov     x23, x21                    // x23 = TTBR0 root
    add     x21, x21, #4096
    mov     x24, x21                    // x24 = TTBR1 root
    add     x21, x21, #4096

    // x25 = image size rounded up to 2 MiB
    ldr     x25, =__image_size
    mov     x9, #0x1fffff
    add     x25, x25, x9
    bic     x25, x25, x9

    ldr     x26, ={block_rwx}
    mov     x27, #0                     // offset
5:  mov     x0, x23                     // identity: load_phys+off -> same
    add     x1, x20, x27
    add     x2, x20, x27
    mov     x3, x26
    bl      __map_block_2m
    mov     x0, x24                     // KERNEL_VA_BASE+off -> load_phys+off
    ldr     x1, ={kva}
    add     x1, x1, x27
    add     x2, x20, x27
    mov     x3, x26
    bl      __map_block_2m
    add     x27, x27, #0x200000
    cmp     x27, x25
    b.lo    5b

    cbz     x19, 7f                     // no DTB
    ldr     x26, ={block_rw}
    mov     x9, #0x1fffff
    bic     x27, x19, x9                // first block containing the DTB
    mov     x28, #2
6:  mov     x0, x24
    ldr     x1, ={phys_offset}
    add     x1, x1, x27
    mov     x2, x27
    mov     x3, x26
    bl      __map_block_2m
    add     x27, x27, #0x200000
    subs    x28, x28, #1
    b.ne    6b
7:
    // Tables were written with non-cacheable stores: drop any stale lines.
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
    bl      __inval_dcache_range

    // ---- MMU on.
    bl      __setup_mair_tcr
    msr     ttbr0_el1, x23
    msr     ttbr1_el1, x24
    isb
    tlbi    vmalle1
    dsb     nsh
    isb
    ldr     x9, ={sctlr_on}
    msr     sctlr_el1, x9
    isb
    ic      iallu
    dsb     nsh
    isb
    ldr     x9, =__boot_high
    br      x9

// Drop from EL2 to EL1h (DAIF masked) if entered at EL2, returning to x30
// at EL1; plain return at EL1. Shared by the boot CPU and secondary CPUs
// (PSCI CPU_ON starts a secondary at the caller's EL or at EL2, depending
// on the VMM/firmware). Clobbers x9.
__el2_to_el1:
    mrs     x9, CurrentEL
    ubfx    x9, x9, #2, #2
    cmp     x9, #2
    b.ne    1f
    mov     x9, #(1 << 31)              // HCR_EL2.RW: EL1 is AArch64
    msr     hcr_el2, x9
    mrs     x9, cnthctl_el2
    orr     x9, x9, #3                  // EL1PCTEN | EL1PCEN
    msr     cnthctl_el2, x9
    msr     cntvoff_el2, xzr
    mov     x9, #0x33ff                 // CPTR_EL2: no FP/SIMD traps
    msr     cptr_el2, x9
    msr     hstr_el2, xzr
    msr     vttbr_el2, xzr
    mrs     x9, midr_el1
    msr     vpidr_el2, x9
    mrs     x9, mpidr_el1
    msr     vmpidr_el2, x9
    mrs     x9, id_aa64pfr0_el1         // GICv3 sysregs: allow EL1 access
    ubfx    x9, x9, #24, #4
    cbz     x9, 2f
    mrs     x9, S3_4_C12_C9_5           // ICC_SRE_EL2
    orr     x9, x9, #0xf                // SRE | DFB | DIB | Enable
    msr     S3_4_C12_C9_5, x9
    isb
2:  ldr     x9, ={sctlr_res1}
    msr     sctlr_el1, x9
    mov     x9, #0x3c5                  // EL1h, DAIF masked
    msr     spsr_el2, x9
    msr     elr_el2, x30
    eret
1:  ret

// Program MAIR/TCR (IPS from ID_AA64MMFR0_EL1.PARange, capped at 48 bits).
// Clobbers x9-x11.
__setup_mair_tcr:
    ldr     x9, ={mair}
    msr     mair_el1, x9
    ldr     x9, ={tcr}
    mrs     x10, id_aa64mmfr0_el1
    and     x10, x10, #7                // PARange
    cmp     x10, #5
    mov     x11, #5
    csel    x10, x10, x11, ls           // cap at 48 bits
    bfi     x9, x10, #32, #3            // TCR_EL1.IPS
    msr     tcr_el1, x9
    ret

// ---- Secondary CPU entry (PSCI CPU_ON target): physical address, MMU and
// caches off, x0 = context_id = PA of `smp::AP_BOOT`, which the boot CPU
// cleaned to the PoC before CPU_ON (reads here are non-cacheable). TTBR0 =
// the boot identity map of the image (still intact in `.bss.boot_pt`,
// never written after boot) so the instruction that turns the MMU on and
// the branch after it stay mapped; TTBR1 = the final kernel tables. Then
// continue at the link address (`__ap_high`).
    .global __ap_entry
__ap_entry:
    msr     daifset, #0xf
    mov     x19, x0
    mrs     x20, CurrentEL              // entry EL (diagnostics)
    ubfx    x20, x20, #2, #2
    bl      __el2_to_el1
    ldr     x9, ={sctlr_res1}
    msr     sctlr_el1, x9
    mov     x9, #(3 << 20)              // CPACR_EL1.FPEN: no FP/SIMD traps
    msr     cpacr_el1, x9
    ldr     x9, [x19, #{ap_cpu}]
    msr     tpidr_el1, x9               // logical CPU id
    isb
    bl      __setup_mair_tcr
    adrp    x9, __boot_pt_pool          // boot TTBR0 root (identity map)
    add     x9, x9, :lo12:__boot_pt_pool
    msr     ttbr0_el1, x9
    ldr     x9, [x19, #{ap_ttbr1}]
    msr     ttbr1_el1, x9
    isb
    tlbi    vmalle1
    dsb     nsh
    isb
    ldr     x9, ={sctlr_on}
    msr     sctlr_el1, x9
    isb
    ic      iallu
    dsb     nsh
    isb
    ldr     x9, =__ap_high
    br      x9

// Map one 2 MiB block: x0 = L0 table PA, x1 = VA, x2 = PA, x3 = attributes.
// Allocates missing L1/L2 tables from [x21, x22). Clobbers x4-x8.
__map_block_2m:
    ubfx    x4, x1, #39, #9
    ldr     x5, [x0, x4, lsl #3]
    cbnz    x5, 1f
    mov     x5, x21
    add     x21, x21, #4096
    cmp     x21, x22
    b.hi    __boot_hang
    orr     x6, x5, #3
    str     x6, [x0, x4, lsl #3]
    b       2f
1:  and     x5, x5, #0x0000fffffffff000
2:  ubfx    x4, x1, #30, #9
    ldr     x6, [x5, x4, lsl #3]
    cbnz    x6, 3f
    mov     x6, x21
    add     x21, x21, #4096
    cmp     x21, x22
    b.hi    __boot_hang
    orr     x7, x6, #3
    str     x7, [x5, x4, lsl #3]
    b       4f
3:  and     x6, x6, #0x0000fffffffff000
4:  ubfx    x4, x1, #21, #9
    orr     x7, x2, x3
    str     x7, [x6, x4, lsl #3]
    ret

// Invalidate data cache lines covering [x0, x1) to the PoC. Clobbers x2, x3.
__inval_dcache_range:
    mrs     x3, ctr_el0
    ubfx    x3, x3, #16, #4             // DminLine: log2(words)
    mov     x2, #4
    lsl     x2, x2, x3                  // line size in bytes
    sub     x3, x2, #1
    bic     x0, x0, x3
1:  dc      ivac, x0
    add     x0, x0, x2
    cmp     x0, x1
    b.lo    1b
    dsb     sy
    ret

__boot_hang:
    wfe
    b       __boot_hang

    .ltorg

    // ---- Now running at the kernel's link address.
    .section .text.boot, "ax"
__boot_high:
    ldr     x9, =__boot_stack_top
    mov     sp, x9
    ldr     x9, =__exception_vectors
    msr     vbar_el1, x9
    isb
    mov     x29, #0
    mov     x30, #0
    mov     x0, x19                     // dtb_phys
    mov     x1, x20                     // load_phys
    bl      {early_main}
    b       __boot_hang

// Secondary CPU, now at the link address on the kernel tables.
__ap_high:
    ldr     x19, ={ap_boot}             // the slot, through its link VA
    ldr     x9, [x19, #{ap_ttbr0}]      // park TTBR0 on the empty table
    msr     ttbr0_el1, x9
    isb
    tlbi    vmalle1
    dsb     nsh
    isb
    ldr     x9, [x19, #{ap_stack}]
    mov     sp, x9
    ldr     x9, =__exception_vectors
    msr     vbar_el1, x9
    isb
    mov     x29, #0
    mov     x30, #0
    mrs     x0, tpidr_el1
    mov     x1, x20                     // EL the CPU entered at
    bl      {ap_rust_entry}
    b       __boot_hang

    .section .bss.boot_pt, "aw", %nobits
    .balign 4096
__boot_pt_pool:
    .space  {pt_pages} * 4096
"#,
    sctlr_res1 = const SCTLR_EL1_RES1,
    sctlr_on = const SCTLR_EL1_MMU_ON,
    tcr = const TCR_EL1_BASE,
    mair = const paging::MAIR_VALUE,
    block_rwx = const BOOT_BLOCK_RWX,
    block_rw = const BOOT_BLOCK_RW,
    kva = const KERNEL_VA_BASE,
    phys_offset = const PHYS_OFFSET,
    pt_pages = const BOOT_PT_PAGES,
    early_main = sym early_main,
    ap_boot = sym super::smp::AP_BOOT,
    ap_rust_entry = sym super::smp::ap_rust_entry,
    ap_cpu = const super::smp::AP_OFF_CPU,
    ap_ttbr1 = const super::smp::AP_OFF_TTBR1,
    ap_ttbr0 = const super::smp::AP_OFF_TTBR0,
    ap_stack = const super::smp::AP_OFF_STACK,
);

static BOOT_INFO: OnceCell<BootInfo> = OnceCell::new();

/// First Rust code, running at the link address on the boot tables with
/// the boot stack. Never returns.
#[unsafe(no_mangle)]
pub extern "C" fn early_main(dtb_phys: usize, load_phys: usize) -> ! {
    let kernel_size = layout::kernel_end() - layout::kernel_start();
    LOAD_PHYS.store(load_phys, Ordering::Relaxed);
    KERNEL_SIZE.store(kernel_size, Ordering::Relaxed);
    paging::adopt_boot_tables();

    if dtb_phys == 0 {
        // No DTB means no console either; nothing useful to do.
        power::poweroff(255);
    }
    // SAFETY: the boot tables map 4 MiB from the DTB's 2 MiB block, and the
    // DTB is reserved in the memory map below, so it is never overwritten.
    let plat = match unsafe { dtb::parse(PHYS_OFFSET + dtb_phys) } {
        Ok(p) => p,
        Err(_) => power::poweroff(255),
    };
    if let Some(c) = plat.psci {
        power::set_conduit(c);
    }
    if let Some(g) = plat.gic {
        gic::set_desc(g);
    }
    timer::early_init(plat.timer_virt_intid);

    // ---- Console: `console=` on the cmdline picks the UART type when it
    // names one we drive (ttyAMA0 = PL011, ttyS0 = 16550); otherwise (absent,
    // hvc0 = virtio-console which is not driven yet) the DTB decides:
    // stdout-path, else the first UART node.
    let want = crate::cmdline::param(plat.bootargs, "console");
    let want_kind = match want {
        Some(c) if c.starts_with("ttyAMA") => Some(UartKind::Pl011),
        Some(c) if c.starts_with("ttyS") => Some(UartKind::Ns16550),
        _ => None,
    };
    let all = plat.uarts.iter().flatten();
    let (chosen, why) = match want_kind {
        Some(k) => match plat.stdout_uart.filter(|u| u.kind == k) {
            Some(u) => (Some(u), "cmdline console= + dtb stdout-path"),
            None => match all.clone().find(|u| u.kind == k) {
                Some(u) => (Some(*u), "cmdline console="),
                None => (None, ""),
            },
        },
        None => (None, ""),
    };
    let (chosen, why) = match chosen {
        Some(u) => (Some(u), why),
        None => match plat.stdout_uart {
            Some(u) => (Some(u), "dtb stdout-path"),
            None => (all.clone().next().copied(), "first dtb uart"),
        },
    };
    if let Some(u) = chosen {
        let va = paging::map_device(PhysAddr(u.base), u.size.max(0x1000)).as_usize();
        // SAFETY: just mapped the UART's register block at `va`.
        let uart = unsafe {
            match u.kind {
                UartKind::Pl011 => console::Uart::Pl011(console::Pl011::new(va)),
                UartKind::Ns16550 => {
                    console::Uart::Ns16550(console::Ns16550::new(va, u.reg_shift, u.reg_io_width))
                }
            }
        };
        console::install(uart);
        crate::kprintln!(
            "[patina] early: console {} ({}, console={}) @ {:#x} (reg-shift {}, io-width {}), dtb @ {:#x} ({} bytes), kernel @ {:#x}",
            u.compatible,
            why,
            want.unwrap_or("<none>"),
            u.base,
            u.reg_shift,
            u.reg_io_width,
            dtb_phys,
            plat.dtb_size,
            load_phys
        );
    }

    // ---- Memory map.
    let mut map = MemoryMap::new();
    for &(s, e) in plat.ram.as_slice() {
        map.add_ram(s, e).expect("memory map full");
    }
    for &(s, e) in plat.reserved.as_slice() {
        map.reserve(s, e, MemKind::Reserved)
            .expect("memory map full");
    }
    map.reserve(dtb_phys, dtb_phys + plat.dtb_size, MemKind::Firmware)
        .expect("memory map full");
    if let Some((s, e)) = plat.initrd {
        map.reserve(s, e, MemKind::Initrd).expect("memory map full");
    }
    map.reserve(load_phys, load_phys + kernel_size, MemKind::Kernel)
        .expect("memory map full");

    // ---- Final kernel page tables.
    if let Err(e) = paging::init_kernel_tables(plat.ram.as_slice()) {
        panic!("building kernel page tables failed: {:?}", e);
    }
    let pan = cpu::enable_pan();
    crate::kprintln!(
        "[patina] early: final page tables live ({} pool pages), TTBR0 parked, PAN {}",
        paging::pool_used(),
        if pan { "on" } else { "unavailable" }
    );
    smp::early_init(&plat);

    // ---- Allocators.
    frame::init(&map);
    heap::init();

    let bi = BootInfo {
        memory: map,
        cmdline: plat.bootargs,
        initrd: plat.initrd.map(|(s, e)| PhysRange::new(s, e)),
        platform: PlatformInfo::Dtb(PhysAddr(dtb_phys)),
        kernel: PhysRange::new(load_phys, load_phys + kernel_size),
    };
    let bi = match BOOT_INFO.set(bi) {
        Ok(b) => b,
        Err(_) => unreachable!(),
    };
    crate::kernel_main(bi)
}
