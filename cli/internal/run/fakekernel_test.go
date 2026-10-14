package run

import (
	"encoding/binary"
)

// Fake guest "kernels" for driving the QEMU driver end to end without the
// real patina kernel. Each prints msg on the first UART and powers off (or
// spins forever when msg is empty and spin is set).

// fakeAArch64Image builds a Linux arm64 Image (64-byte header, "ARM\x64")
// whose code writes msg to the virt machine's PL011 (0x09000000) and calls
// PSCI SYSTEM_OFF via HVC. With spin, it loops forever after printing.
func fakeAArch64Image(msg string, spin bool) []byte {
	var code []uint32
	movz64 := func(rd, imm16, hw uint32) uint32 { return 0xD2800000 | hw<<21 | imm16<<5 | rd }
	movk64 := func(rd, imm16, hw uint32) uint32 { return 0xF2800000 | hw<<21 | imm16<<5 | rd }
	code = append(code,
		movz64(1, 0x0900, 1),           // x1 = 0x09000000 (PL011 base)
		0x52800000|0x301<<5|4,          // movz w4, #0x301 (UARTEN|TXE|RXE)
		0xB9000000|(0x30/4)<<10|1<<5|4, // str w4, [x1, #0x30] (UARTCR)
	)
	adrIdx := len(code)
	code = append(code, 0) // adr x2, msg (patched below)
	loop := len(code)
	code = append(code, 0x38400400|1<<12|2<<5|3) // ldrb w3, [x2], #1
	cbzIdx := len(code)
	code = append(code, 0)                                             // cbz w3, done (patched)
	code = append(code, 0x39000000|1<<5|3)                             // strb w3, [x1]
	code = append(code, 0x14000000|(uint32(loop-len(code))&0x3FFFFFF)) // b loop
	done := len(code)
	if spin {
		code = append(code, 0x14000000) // b .
	} else {
		code = append(code,
			movz64(0, 0x0008, 0), // x0 = 0x84000008 PSCI SYSTEM_OFF
			movk64(0, 0x8400, 1),
			0xD4000002, // hvc #0
			0x14000000, // b .
		)
	}
	msgOff := len(code) * 4
	code[cbzIdx] = 0x34000000 | (uint32(done-cbzIdx)&0x7FFFF)<<5 | 3
	rel := uint32(msgOff - adrIdx*4)
	code[adrIdx] = 0x10000000 | (rel&3)<<29 | ((rel>>2)&0x7FFFF)<<5 | 2

	body := make([]byte, 0, len(code)*4+len(msg)+1)
	for _, w := range code {
		body = binary.LittleEndian.AppendUint32(body, w)
	}
	body = append(body, msg...)
	body = append(body, 0)
	for len(body)%8 != 0 {
		body = append(body, 0)
	}

	hdr := make([]byte, 64)
	binary.LittleEndian.PutUint32(hdr[0:], 0x14000000|64/4) // b +64 (to code)
	binary.LittleEndian.PutUint64(hdr[8:], 0)               // text_offset
	binary.LittleEndian.PutUint64(hdr[16:], uint64(64+len(body)))
	binary.LittleEndian.PutUint64(hdr[24:], 0xA) // LE, 4K pages, place anywhere
	copy(hdr[56:], "ARM\x64")
	return append(hdr, body...)
}

// fakeX86PVH builds an ELF64 with a Xen PVH entry note whose 32-bit code
// writes msg to COM1 (0x3f8) and then writes exitCode to isa-debug-exit
// (0xf4). With spin, it halts in a loop instead of exiting.
func fakeX86PVH(msg string, exitCode uint32, spin bool) []byte {
	const (
		base    = 0x100000
		phoff   = 64
		noteOff = phoff + 2*56
		codeOff = 0x100
	)
	entry := uint32(base + codeOff)
	var c []byte
	msgAddrAt := len(c) + 1
	c = append(c, 0xBE, 0, 0, 0, 0)       // mov esi, msg
	c = append(c, 0x66, 0xBA, 0xF8, 0x03) // mov dx, 0x3f8
	loop := len(c)
	c = append(c, 0xAC, 0x84, 0xC0) // lodsb; test al, al
	jzAt := len(c)
	c = append(c, 0x74, 0)                     // jz done
	c = append(c, 0xEE)                        // out dx, al
	c = append(c, 0xEB, byte(loop-(len(c)+2))) // jmp loop
	c[jzAt+1] = byte(len(c) - (jzAt + 2))
	if !spin {
		c = append(c, 0x66, 0xBA, 0xF4, 0x00) // mov dx, 0xf4
		c = append(c, 0xB8)                   // mov eax, exitCode
		c = binary.LittleEndian.AppendUint32(c, exitCode)
		c = append(c, 0xEF) // out dx, eax
	}
	c = append(c, 0xFA, 0xF4, 0xEB, 0xFD) // cli; hlt; jmp hlt
	msgAddr := entry + uint32(len(c))
	binary.LittleEndian.PutUint32(c[msgAddrAt:], msgAddr)
	c = append(c, msg...)
	c = append(c, 0)

	total := codeOff + len(c)
	f := make([]byte, total)
	copy(f[0:], "\x7fELF")
	f[4], f[5], f[6] = 2, 1, 1 // ELFCLASS64, LSB, EV_CURRENT
	le := binary.LittleEndian
	le.PutUint16(f[16:], 2)  // ET_EXEC
	le.PutUint16(f[18:], 62) // EM_X86_64
	le.PutUint32(f[20:], 1)
	le.PutUint64(f[24:], uint64(entry))
	le.PutUint64(f[32:], phoff)
	le.PutUint16(f[52:], 64) // ehsize
	le.PutUint16(f[54:], 56) // phentsize
	le.PutUint16(f[56:], 2)  // phnum
	// PT_LOAD covering the whole file at base.
	ph := f[phoff:]
	le.PutUint32(ph[0:], 1)
	le.PutUint32(ph[4:], 5) // R|X
	le.PutUint64(ph[8:], 0)
	le.PutUint64(ph[16:], base)
	le.PutUint64(ph[24:], base)
	le.PutUint64(ph[32:], uint64(total))
	le.PutUint64(ph[40:], uint64(total))
	le.PutUint64(ph[48:], 0x1000)
	// PT_NOTE with XEN_ELFNOTE_PHYS32_ENTRY (18).
	ph = f[phoff+56:]
	le.PutUint32(ph[0:], 4)
	le.PutUint32(ph[4:], 4)
	le.PutUint64(ph[8:], noteOff)
	le.PutUint64(ph[16:], base+noteOff)
	le.PutUint64(ph[24:], base+noteOff)
	le.PutUint64(ph[32:], 20)
	le.PutUint64(ph[40:], 20)
	le.PutUint64(ph[48:], 4)
	n := f[noteOff:]
	le.PutUint32(n[0:], 4)  // namesz
	le.PutUint32(n[4:], 4)  // descsz
	le.PutUint32(n[8:], 18) // type
	copy(n[12:], "Xen\x00")
	le.PutUint32(n[16:], entry)
	copy(f[codeOff:], c)
	return f
}
