// DTB probe "kernel" for Apple VZ (see vz_probe_test.go).
//
// VZ gives the host no view of the device tree it builds and the guest no
// console without a virtio driver, so this tiny arm64 Image answers one
// question per boot through *timing*: it computes a value v from the DTB the
// VMM passed in x0, waits v * unit_ms, and calls PSCI SYSTEM_OFF. The host
// measures start -> guestDidStop and divides.
//
// Parameters live in the `params` block at the end of .text, patched by the
// host before each boot:
//   mode 0: v = number of occurrences of needle anywhere in the blob
//   mode 1: v = (byte at first_match + offset >> shift) & mask  (0 if no match)
//   mode 2: v = (byte at offset from the DTB start >> shift) & mask
//   mode 3: v = (32-bit word at physical address offset >> shift) & mask
//           (e.g. PCI ECAM config space; offset must be 4-aligned)
//
// Assemble: clang -target aarch64-none-elf -c vzprobe.S. Position independent;
// runs with the MMU off (byte loads only, except the aligned header word).

	.text
	.globl _start
_start:
	mov	x19, x0			// DTB
	ldr	w1, [x19, #4]		// fdt totalsize (big endian)
	rev	w1, w1
	add	x20, x19, x1		// end of blob
	adr	x21, needle
	mov	x22, #0			// needle length
1:	ldrb	w2, [x21, x22]
	cbz	w2, 2f
	add	x22, x22, #1
	b	1b
2:	ldr	x24, mode
	cmp	x24, #2
	b.eq	abs_byte
	cmp	x24, #3
	b.eq	phys_word
	mov	x23, #0			// match count
	mov	x25, #-1		// first match address
	mov	x3, x19
scan:	add	x4, x3, x22
	cmp	x4, x20
	b.hi	scanned
	mov	x5, #0
3:	cmp	x5, x22
	b.eq	match
	ldrb	w6, [x3, x5]
	ldrb	w7, [x21, x5]
	cmp	w6, w7
	b.ne	next
	add	x5, x5, #1
	b	3b
match:	add	x23, x23, #1
	cmn	x25, #1
	csel	x25, x3, x25, eq
next:	add	x3, x3, #1
	b	scan
scanned:
	cbz	x24, delay		// mode 0: v = count
	mov	x23, #0
	cmn	x25, #1
	b.eq	delay			// mode 1, no match: v = 0
	ldr	x6, offset
	add	x3, x25, x6
	b	extract
abs_byte:
	ldr	x6, offset
	add	x3, x19, x6
extract:
	mov	x23, #0
	cmp	x3, x20
	b.hs	delay
	ldrb	w23, [x3]
	b	shiftmask
phys_word:
	ldr	x3, offset
	ldr	w23, [x3]
shiftmask:
	ldr	x6, shift
	lsr	x23, x23, x6
	ldr	x6, mask
	and	x23, x23, x6
delay:	mrs	x8, cntfrq_el0
	ldr	x9, unit_ms
	mul	x8, x8, x9
	mov	x10, #1000
	udiv	x8, x8, x10
	mul	x8, x8, x23		// ticks to wait
	isb
	mrs	x11, cntvct_el0
	add	x11, x11, x8
4:	isb
	mrs	x12, cntvct_el0
	cmp	x12, x11
	b.lo	4b
	movz	x0, #0x0008
	movk	x0, #0x8400, lsl #16	// PSCI SYSTEM_OFF
	hvc	#0
	smc	#0
5:	wfi
	b	5b

	.balign 8
	.globl params
params:
mode:	.quad 0
offset:	.quad 0
shift:	.quad 0
mask:	.quad 0xff
unit_ms: .quad 100
needle:	.space 120
