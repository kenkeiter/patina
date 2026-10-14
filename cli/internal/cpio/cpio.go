// Package cpio writes and reads "newc" (SVR4, magic 070701) cpio archives,
// the format the Linux kernel and patina accept as an initrd.
//
// Entry names are stored relative to the archive root without a leading
// slash ("app", "etc/hosts"), matching what gen_init_cpio and
// `find . | cpio -o -H newc` produce for the root directory.
package cpio

import (
	"bufio"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"strconv"
)

const (
	magic      = "070701"
	headerSize = 110
	trailer    = "TRAILER!!!"
)

// File type bits as stored in c_mode (identical to Linux S_IF*).
const (
	TypeMask    = 0o170000
	TypeDir     = 0o040000
	TypeReg     = 0o100000
	TypeSymlink = 0o120000
)

// Header describes one archive entry.
type Header struct {
	Name  string // path without leading slash
	Mode  uint32 // type bits | permission bits
	UID   uint32
	GID   uint32
	Nlink uint32
	MTime int64 // seconds since the epoch
	Size  int64 // data length (symlink: length of target)
	Ino   uint32
}

// IsDir reports whether the header describes a directory.
func (h *Header) IsDir() bool { return h.Mode&TypeMask == TypeDir }

// IsRegular reports whether the header describes a regular file.
func (h *Header) IsRegular() bool { return h.Mode&TypeMask == TypeReg }

// IsSymlink reports whether the header describes a symbolic link.
func (h *Header) IsSymlink() bool { return h.Mode&TypeMask == TypeSymlink }

// FileMode converts the cpio mode to an fs.FileMode (for display).
func (h *Header) FileMode() fs.FileMode {
	m := fs.FileMode(h.Mode & 0o777)
	switch h.Mode & TypeMask {
	case TypeDir:
		m |= fs.ModeDir
	case TypeSymlink:
		m |= fs.ModeSymlink
	}
	if h.Mode&0o4000 != 0 {
		m |= fs.ModeSetuid
	}
	if h.Mode&0o2000 != 0 {
		m |= fs.ModeSetgid
	}
	if h.Mode&0o1000 != 0 {
		m |= fs.ModeSticky
	}
	return m
}

// Writer emits a newc archive entry by entry. Callers are responsible for
// ordering (parents before children); see Builder for a tree-aware helper.
type Writer struct {
	w       io.Writer
	off     int64
	nextIno uint32
	remain  int64 // bytes of current entry's data still expected
	pad     int   // padding owed after the current entry's data
	closed  bool
}

// NewWriter returns a Writer that writes to w.
func NewWriter(w io.Writer) *Writer { return &Writer{w: w, nextIno: 1} }

func (cw *Writer) write(p []byte) error {
	n, err := cw.w.Write(p)
	cw.off += int64(n)
	return err
}

func pad4(n int64) int { return int((4 - n%4) % 4) }

var zeros [4]byte

// WriteHeader starts a new entry. Exactly hdr.Size bytes of data must then be
// written with Write before the next WriteHeader or Close.
func (cw *Writer) WriteHeader(hdr *Header) error {
	if cw.closed {
		return errors.New("cpio: write after close")
	}
	if err := cw.finishEntry(); err != nil {
		return err
	}
	if hdr.Name == "" || hdr.Name[0] == '/' {
		return fmt.Errorf("cpio: invalid entry name %q (must be non-empty, without leading slash)", hdr.Name)
	}
	if hdr.Size < 0 || hdr.Size > 0xffffffff {
		return fmt.Errorf("cpio: %s: size %d out of range for newc", hdr.Name, hdr.Size)
	}
	ino := hdr.Ino
	if ino == 0 && hdr.Name != trailer {
		ino = cw.nextIno
		cw.nextIno++
	}
	nlink := hdr.Nlink
	if nlink == 0 {
		nlink = 1
		if hdr.IsDir() {
			nlink = 2
		}
	}
	nameSize := len(hdr.Name) + 1
	fields := []uint32{
		ino, hdr.Mode, hdr.UID, hdr.GID, nlink, uint32(hdr.MTime), uint32(hdr.Size),
		0, 0, // devmajor, devminor
		0, 0, // rdevmajor, rdevminor
		uint32(nameSize),
		0, // check
	}
	buf := make([]byte, 0, headerSize+nameSize+3)
	buf = append(buf, magic...)
	for _, f := range fields {
		buf = append(buf, fmt.Sprintf("%08x", f)...)
	}
	buf = append(buf, hdr.Name...)
	buf = append(buf, 0)
	buf = append(buf, zeros[:pad4(int64(len(buf)))]...)
	if err := cw.write(buf); err != nil {
		return err
	}
	cw.remain = hdr.Size
	cw.pad = pad4(hdr.Size)
	return nil
}

// Write writes data for the current entry.
func (cw *Writer) Write(p []byte) (int, error) {
	if int64(len(p)) > cw.remain {
		return 0, fmt.Errorf("cpio: write of %d bytes exceeds remaining entry size %d", len(p), cw.remain)
	}
	n, err := cw.w.Write(p)
	cw.off += int64(n)
	cw.remain -= int64(n)
	return n, err
}

func (cw *Writer) finishEntry() error {
	if cw.remain != 0 {
		return fmt.Errorf("cpio: entry short by %d bytes", cw.remain)
	}
	if cw.pad > 0 {
		if err := cw.write(zeros[:cw.pad]); err != nil {
			return err
		}
		cw.pad = 0
	}
	return nil
}

// Close writes the TRAILER!!! entry. It does not close the underlying writer.
func (cw *Writer) Close() error {
	if cw.closed {
		return nil
	}
	if err := cw.WriteHeader(&Header{Name: trailer, Nlink: 1}); err != nil {
		return err
	}
	if err := cw.finishEntry(); err != nil {
		return err
	}
	cw.closed = true
	return nil
}

// Reader reads a newc archive sequentially.
type Reader struct {
	r      *bufio.Reader
	remain int64
	pad    int
	done   bool
}

// NewReader returns a Reader reading from r.
func NewReader(r io.Reader) *Reader { return &Reader{r: bufio.NewReader(r)} }

// Next advances to the next entry, skipping any unread data of the current
// one. It returns io.EOF after the trailer.
func (cr *Reader) Next() (*Header, error) {
	if cr.done {
		return nil, io.EOF
	}
	if _, err := io.CopyN(io.Discard, cr.r, cr.remain+int64(cr.pad)); err != nil {
		return nil, unexpected(err)
	}
	cr.remain, cr.pad = 0, 0
	var hb [headerSize]byte
	if _, err := io.ReadFull(cr.r, hb[:]); err != nil {
		return nil, unexpected(err)
	}
	if string(hb[:6]) != magic {
		return nil, fmt.Errorf("cpio: bad magic %q (only newc/070701 is supported)", hb[:6])
	}
	var f [13]uint32
	for i := range f {
		v, err := strconv.ParseUint(string(hb[6+8*i:14+8*i]), 16, 32)
		if err != nil {
			return nil, fmt.Errorf("cpio: bad header field %d: %v", i, err)
		}
		f[i] = uint32(v)
	}
	nameSize := int(f[11])
	if nameSize < 1 || nameSize > 4096 {
		return nil, fmt.Errorf("cpio: bad name size %d", nameSize)
	}
	nb := make([]byte, nameSize+pad4(int64(headerSize+nameSize)))
	if _, err := io.ReadFull(cr.r, nb); err != nil {
		return nil, unexpected(err)
	}
	if nb[nameSize-1] != 0 {
		return nil, errors.New("cpio: name not NUL-terminated")
	}
	h := &Header{
		Ino: f[0], Mode: f[1], UID: f[2], GID: f[3], Nlink: f[4],
		MTime: int64(f[5]), Size: int64(f[6]), Name: string(nb[:nameSize-1]),
	}
	if h.Name == trailer {
		cr.done = true
		return nil, io.EOF
	}
	cr.remain = h.Size
	cr.pad = pad4(h.Size)
	return h, nil
}

// Read reads the current entry's data.
func (cr *Reader) Read(p []byte) (int, error) {
	if cr.remain == 0 {
		return 0, io.EOF
	}
	if int64(len(p)) > cr.remain {
		p = p[:cr.remain]
	}
	n, err := cr.r.Read(p)
	cr.remain -= int64(n)
	if err == io.EOF && cr.remain > 0 {
		err = io.ErrUnexpectedEOF
	}
	return n, err
}

func unexpected(err error) error {
	if err == io.EOF {
		return io.ErrUnexpectedEOF
	}
	return err
}
