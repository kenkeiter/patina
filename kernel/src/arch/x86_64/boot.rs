//! x86_64 entry: PVH ELF note, 32-bit protected-mode entry, long-mode
//! switch on boot page tables, and the high-VA Rust entry [`early_main`].
//!
//! PVH entry state (xen/include/public/arch-x86/hvm/start_info.h): 32-bit
//! protected mode, paging off, flat CS/DS, `ebx` = physical address of
//! `hvm_start_info`, no usable stack. `_start32` runs at its physical
//! address, so it only uses absolute addresses of the form `sym - KVA`
//! (link-time constants below 4 GiB).
//!
//! Boot tables (7 pages in `.bss.boot_pt`, all 2 MiB pages, RWX):
//! * PML4[0]   -> PDPT_low: identity map of the low 4 GiB (4 PDs). Only used
//!   until the jump to the high VA; dropped when `early_main` switches CR3 to
//!   the final tables.
//! * PML4[256] -> the same PDPT_low: `PHYS_OFFSET + pa -> pa` for the low
//!   4 GiB (start_info, memmap, cmdline, modules are read through it).
//! * PML4[511] -> PDPT_high, entry 510 -> PD #0: `KERNEL_VA_BASE + pa -> pa`
//!   for the first 1 GiB (covers the image at 0x20_0000).

use core::arch::global_asm;

use super::start_info::{self, MEMMAP_ACPI, MEMMAP_NVS, MEMMAP_RAM};
use super::{KERNEL_VA_BASE, console, exceptions, gdt, kernel_load_phys, kernel_size, paging};
use crate::bootinfo::{BootInfo, MemKind, MemoryMap, PhysRange, PlatformInfo};
use crate::mm::{PhysAddr, frame, heap, page_align_down, page_align_up};
use crate::sync::OnceCell;

/// CR4: PAE | PGE | OSFXSR | OSXMMEXCPT.
const CR4_BOOT: u32 = (1 << 5) | (1 << 7) | (1 << 9) | (1 << 10);
/// CR0 bits set: PE | MP | NE | WP | PG.
const CR0_SET: u32 = (1 << 0) | (1 << 1) | (1 << 5) | (1 << 16) | (1 << 31);
/// CR0 bits cleared: EM | TS | NW | CD.
const CR0_CLEAR: u32 = (1 << 2) | (1 << 3) | (1 << 29) | (1 << 30);
/// EFER: LME | NXE.
const EFER_BOOT: u32 = (1 << 8) | (1 << 11);
const MSR_EFER: u32 = 0xc000_0080;
/// XEN_ELFNOTE_PHYS32_ENTRY.
const XEN_ELFNOTE_PHYS32_ENTRY: u32 = 18;

global_asm!(
    r#"
    .set KVA, {kva}

    // ---- PVH ELF note: physical 32-bit entry point. Kept by the linker
    // script and placed in a PT_NOTE segment. The descriptor is 8 bytes
    // (like Linux's), which QEMU reads as a u64 and other loaders as a u32.
    .section .note.Xen, "a", @note
    .balign 4
    .long   4                           // namesz
    .long   8                           // descsz
    .long   {note_type}                 // type
    .asciz  "Xen"
    .balign 4
    .quad   _start32 - KVA

    .section .text.boot32, "ax"
    .code32
    .global _start32
_start32:
    cli
    cld
    movl    %ebx, %esi                  // esi = start_info PA (kept until early_main)

    lgdt    (__boot_gdt_ptr - KVA)
    movl    ${kds}, %eax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    movw    %ax, %fs
    movw    %ax, %gs

    // ---- Zero .bss (includes the boot tables and the boot stack).
    movl    $(__bss_start - KVA), %edi
    movl    $(__bss_end - KVA), %ecx
    subl    %edi, %ecx
    xorl    %eax, %eax
    rep stosb

    // ---- Boot page tables.
    movl    $(__boot_pml4 - KVA), %edi
    movl    $(__boot_pdpt_low - KVA + 3), %eax
    movl    %eax, 0(%edi)               // PML4[0]   identity
    movl    %eax, 2048(%edi)            // PML4[256] PHYS_OFFSET
    movl    $(__boot_pdpt_high - KVA + 3), %eax
    movl    %eax, 4088(%edi)            // PML4[511] kernel

    movl    $(__boot_pdpt_low - KVA), %edi
    movl    $(__boot_pd - KVA + 3), %eax
    movl    %eax, 0(%edi)
    addl    $4096, %eax
    movl    %eax, 8(%edi)
    addl    $4096, %eax
    movl    %eax, 16(%edi)
    addl    $4096, %eax
    movl    %eax, 24(%edi)

    movl    $(__boot_pdpt_high - KVA), %edi
    movl    $(__boot_pd - KVA + 3), %eax
    movl    %eax, 4080(%edi)            // PDPT_high[510] -> first GiB

    movl    $(__boot_pd - KVA), %edi
    movl    $0x83, %eax                 // P | RW | PS
    movl    $2048, %ecx
1:  movl    %eax, (%edi)
    movl    $0, 4(%edi)
    addl    $0x200000, %eax
    addl    $8, %edi
    decl    %ecx
    jnz     1b

    // ---- Long mode.
    movl    %cr4, %eax
    orl     ${cr4}, %eax
    movl    %eax, %cr4
    movl    $(__boot_pml4 - KVA), %eax
    movl    %eax, %cr3
    movl    ${efer_msr}, %ecx
    rdmsr
    orl     ${efer}, %eax
    wrmsr
    movl    %cr0, %eax
    andl    ${cr0_clear_inv}, %eax
    orl     ${cr0_set}, %eax
    movl    %eax, %cr0
    ljmp    ${kcs}, $(__boot64_low - KVA)

    .code64
__boot64_low:
    // Still executing at the physical address (identity map).
    movabsq $__boot64_high, %rax
    jmpq    *%rax

    .section .text.boot, "ax"
__boot64_high:
    leaq    __boot_stack_top(%rip), %rsp
    xorl    %ebp, %ebp
    movl    %esi, %edi                  // zero-extends: start_info PA
    call    {early_main}
2:  cli
    hlt
    jmp     2b

    .section .data.boot, "aw"
    .balign 16
__boot_gdt:
    .quad   0
    .quad   0x00af9a000000ffff          // 0x08: kernel code, 64-bit
    .quad   0x00cf92000000ffff          // 0x10: kernel data
__boot_gdt_end:
    .balign 8
    .word   0
__boot_gdt_ptr:
    .word   __boot_gdt_end - __boot_gdt - 1
    .long   __boot_gdt - KVA

    .section .bss.boot_pt, "aw", @nobits
    .balign 4096
__boot_pml4:
    .space  4096
__boot_pdpt_low:
    .space  4096
__boot_pdpt_high:
    .space  4096
__boot_pd:
    .space  4 * 4096
"#,
    kva = const KERNEL_VA_BASE,
    note_type = const XEN_ELFNOTE_PHYS32_ENTRY,
    kds = const gdt::KERNEL_DS,
    kcs = const gdt::KERNEL_CS,
    cr4 = const CR4_BOOT,
    efer_msr = const MSR_EFER,
    efer = const EFER_BOOT,
    cr0_set = const CR0_SET,
    cr0_clear_inv = const !CR0_CLEAR,
    early_main = sym early_main,
    options(att_syntax)
);

static BOOT_INFO: OnceCell<BootInfo> = OnceCell::new();

const MAX_DIRECT: usize = start_info::MAX_MEMMAP + 2;

/// Sorted, merged list of page-aligned physical ranges to direct-map.
struct Ranges {
    items: [(usize, usize); MAX_DIRECT],
    len: usize,
}

impl Ranges {
    fn new() -> Self {
        Self {
            items: [(0, 0); MAX_DIRECT],
            len: 0,
        }
    }

    fn add(&mut self, start: usize, end: usize) {
        let (start, end) = (page_align_down(start), page_align_up(end));
        if start >= end || self.len == MAX_DIRECT {
            return;
        }
        // Insertion sort by start.
        let mut i = self.len;
        while i > 0 && self.items[i - 1].0 > start {
            self.items[i] = self.items[i - 1];
            i -= 1;
        }
        self.items[i] = (start, end);
        self.len += 1;
    }

    fn merged(&mut self) -> &[(usize, usize)] {
        let mut out = 0;
        for i in 0..self.len {
            let (s, e) = self.items[i];
            if out > 0 && s <= self.items[out - 1].1 {
                self.items[out - 1].1 = self.items[out - 1].1.max(e);
            } else {
                self.items[out] = (s, e);
                out += 1;
            }
        }
        self.len = out;
        &self.items[..out]
    }
}

fn console_port(cmdline: &str) -> (u16, &'static str) {
    match crate::cmdline::param(cmdline, "console") {
        Some(c) if c.starts_with("ttyS") => {
            let n: usize = c[4..]
                .split(',')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            match console::COM_PORTS.get(n) {
                Some(&p) => (p, "cmdline console="),
                None => (console::COM_PORTS[0], "default (unknown ttyS index)"),
            }
        }
        Some(_) => (console::COM_PORTS[0], "default (console= not a 16550)"),
        None => (console::COM_PORTS[0], "default"),
    }
}

/// First Rust code, running at the link address on the boot tables with the
/// boot stack. Never returns.
#[unsafe(no_mangle)]
pub extern "C" fn early_main(start_info_pa: usize) -> ! {
    paging::adopt_boot_tables();
    gdt::init();
    exceptions::init();

    // SAFETY: on the boot tables, called once.
    let parsed = unsafe { start_info::parse(start_info_pa) };

    // ---- Console (the cmdline, if we got one, may pick the port).
    let cmdline = parsed.as_ref().map(|s| s.cmdline).unwrap_or("");
    let (port, why) = console_port(cmdline);
    let present = console::Uart16550::present(port);
    // SAFETY: legacy COM port range.
    console::install(unsafe { console::Uart16550::new(port) });

    let si = match parsed {
        Ok(s) => s,
        Err(e) => panic!("PVH start_info @ {:#x} unusable: {:?}", start_info_pa, e),
    };
    crate::kprintln!(
        "[patina] early: console 16550 @ port {:#x} ({}, {}), start_info @ {:#x} v{} flags {:#x}, memmap {} entries @ {:#x}, cmdline @ {:#x} ({} bytes), modules {} @ {:#x}, rsdp @ {:#x}, kernel @ {:#x}",
        port,
        why,
        if present {
            "scratch ok"
        } else {
            "no scratch reg"
        },
        si.pa,
        si.version,
        si.flags,
        si.memmap_entries,
        si.memmap_pa,
        si.cmdline_pa,
        si.cmdline_len,
        si.nr_modules,
        si.modlist.map_or(0, |m| m.0),
        si.rsdp,
        kernel_load_phys()
    );
    for e in si.memmap() {
        crate::kprintln!(
            "[patina] early:   pvh memmap {:#014x}-{:#014x} {}",
            e.addr,
            e.addr.saturating_add(e.size),
            start_info::memmap_kind_name(e.kind)
        );
    }
    if let Some((s, e)) = si.initrd {
        crate::kprintln!("[patina] early:   module 0 (initrd) {:#x}-{:#x}", s, e);
    }
    if si.memmap_len == 0 {
        panic!(
            "PVH start_info has no memmap (start_info version {}): cannot discover RAM",
            si.version
        );
    }
    if si.memmap_entries > si.memmap_len {
        crate::kprintln!(
            "[patina] early: warning: memmap truncated to {} of {} entries",
            si.memmap_len,
            si.memmap_entries
        );
    }
    if si.cmdline_truncated {
        crate::kprintln!("[patina] early: warning: cmdline truncated to 4096 bytes");
    }

    // ---- Memory map.
    let mut map = MemoryMap::new();
    let mut direct = Ranges::new();
    for e in si.memmap() {
        let (s, end) = (e.addr as usize, (e.addr + e.size) as usize);
        if e.kind == MEMMAP_RAM {
            map.add_ram(s, end).expect("memory map full");
        }
        if matches!(e.kind, MEMMAP_RAM | MEMMAP_ACPI | MEMMAP_NVS) {
            direct.add(s, end);
        }
    }
    for e in si.memmap() {
        let (s, end) = (e.addr as usize, (e.addr + e.size) as usize);
        let kind = match e.kind {
            MEMMAP_RAM => continue,
            MEMMAP_ACPI | MEMMAP_NVS => MemKind::Firmware,
            _ => MemKind::Reserved,
        };
        map.reserve(s, end, kind).expect("memory map full");
    }
    // Low 1 MiB: real-mode/BIOS area, EBDA, RSDP; also kept direct-mapped so
    // Phase 1 can read the RSDP and an AP trampoline can live there.
    map.reserve(0, 0x10_0000, MemKind::Reserved)
        .expect("memory map full");
    direct.add(0, 0x10_0000);
    let fw = |map: &mut MemoryMap, s: usize, e: usize| {
        map.reserve(s, e, MemKind::Firmware)
            .expect("memory map full");
    };
    fw(&mut map, si.pa, si.pa + 64);
    if let Some((s, e)) = si.modlist {
        fw(&mut map, s, e);
    }
    if si.cmdline_pa != 0 {
        fw(
            &mut map,
            si.cmdline_pa,
            si.cmdline_pa + si.cmdline_len.max(1),
        );
    }
    if si.memmap_pa != 0 {
        fw(
            &mut map,
            si.memmap_pa,
            si.memmap_pa + si.memmap_entries * 24,
        );
    }
    if si.rsdp != 0 {
        fw(&mut map, si.rsdp, si.rsdp + 36);
    }
    if let Some((s, e)) = si.initrd {
        map.reserve(s, e, MemKind::Initrd).expect("memory map full");
        // QEMU/qboot may report the initrd's pages as e820-reserved rather
        // than RAM; it must be readable through the direct map regardless.
        direct.add(s, e);
    }
    let (kstart, ksize) = (kernel_load_phys(), kernel_size());
    map.reserve(kstart, kstart + ksize, MemKind::Kernel)
        .expect("memory map full");

    // ---- Final kernel page tables (drops the identity map).
    let ranges = direct.merged();
    if let Err(e) = paging::init_kernel_tables(ranges) {
        panic!("building kernel page tables failed: {:?}", e);
    }
    let pool = paging::pool_used();

    // ---- Allocators.
    frame::init(&map);
    heap::init();
    if let Err(e) = paging::populate_kernel_pml4() {
        panic!("populating kernel PML4 failed: {:?}", e);
    }
    let top = ranges.last().map_or(0, |r| r.1);
    crate::kprintln!(
        "[patina] early: final page tables live ({} pool pages, {} direct-map ranges up to {:#x}), identity map dropped, NX on",
        pool,
        ranges.len(),
        top
    );

    let bi = BootInfo {
        memory: map,
        cmdline: si.cmdline,
        initrd: si.initrd.map(|(s, e)| PhysRange::new(s, e)),
        platform: PlatformInfo::StartInfo(PhysAddr(si.pa)),
        kernel: PhysRange::new(kstart, kstart + ksize),
    };
    let bi = match BOOT_INFO.set(bi) {
        Ok(b) => b,
        Err(_) => unreachable!(),
    };
    crate::kernel_main(bi)
}
