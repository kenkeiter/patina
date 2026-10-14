package cpio

import (
	"fmt"
	"io"
	"io/fs"
	"os"
	"path"
	"path/filepath"
	"sort"
	"strings"
)

// Builder assembles a directory tree in memory (contents of host files are
// read lazily) and writes it as a newc archive with every parent directory
// present and listed before its children.
type Builder struct {
	entries map[string]*entry
}

type entry struct {
	hdr      Header
	data     []byte // regular file contents or symlink target, if inline
	src      string // host path to stream regular-file contents from
	implicit bool   // directory created only because a child needs it
}

// NewBuilder returns an empty Builder.
func NewBuilder() *Builder { return &Builder{entries: map[string]*entry{}} }

// CleanPath converts a guest path ("/etc/hosts", "etc//hosts/") to archive
// form ("etc/hosts"). It rejects the root and paths escaping it.
func CleanPath(p string) (string, error) {
	if strings.ContainsRune(p, 0) {
		return "", fmt.Errorf("invalid path %q", p)
	}
	c := strings.TrimPrefix(path.Clean("/"+p), "/")
	if c == "" {
		return "", fmt.Errorf("invalid path %q: refers to the root directory", p)
	}
	return c, nil
}

func (b *Builder) ensureParents(name string, mtime int64) error {
	dir := path.Dir(name)
	if dir == "." {
		return nil
	}
	if e, ok := b.entries[dir]; ok {
		if !e.hdr.IsDir() {
			return fmt.Errorf("cpio: %s: parent /%s is not a directory", name, dir)
		}
		return nil
	}
	if err := b.ensureParents(dir, mtime); err != nil {
		return err
	}
	b.entries[dir] = &entry{hdr: Header{Name: dir, Mode: TypeDir | 0o755, MTime: mtime}, implicit: true}
	return nil
}

func (b *Builder) add(e *entry) error {
	name, err := CleanPath(e.hdr.Name)
	if err != nil {
		return err
	}
	e.hdr.Name = name
	if old, ok := b.entries[name]; ok {
		if old.hdr.IsDir() && e.hdr.IsDir() {
			// Explicit directory metadata wins over implicit; merging two
			// explicit dirs (e.g. overlapping --files trees) is fine.
			if old.implicit {
				b.entries[name] = e
			}
			return nil
		}
		return fmt.Errorf("duplicate path /%s in image", name)
	}
	if err := b.ensureParents(name, e.hdr.MTime); err != nil {
		return err
	}
	b.entries[name] = e
	return nil
}

// AddDir adds a directory with the given permission bits.
func (b *Builder) AddDir(name string, perm uint32, mtime int64) error {
	return b.add(&entry{hdr: Header{Name: name, Mode: TypeDir | perm&0o7777, MTime: mtime}})
}

// AddBytes adds a regular file with inline contents.
func (b *Builder) AddBytes(name string, perm uint32, mtime int64, data []byte) error {
	return b.add(&entry{hdr: Header{Name: name, Mode: TypeReg | perm&0o7777, MTime: mtime, Size: int64(len(data))}, data: data})
}

// AddHostFile adds a regular file whose contents are streamed from src at
// write time. perm overrides the host permission bits.
func (b *Builder) AddHostFile(name, src string, perm uint32) error {
	fi, err := os.Stat(src)
	if err != nil {
		return err
	}
	if !fi.Mode().IsRegular() {
		return fmt.Errorf("%s: not a regular file", src)
	}
	return b.add(&entry{hdr: Header{Name: name, Mode: TypeReg | perm&0o7777, MTime: fi.ModTime().Unix(), Size: fi.Size()}, src: src})
}

// AddSymlink adds a symbolic link.
func (b *Builder) AddSymlink(name, target string, mtime int64) error {
	return b.add(&entry{hdr: Header{Name: name, Mode: TypeSymlink | 0o777, MTime: mtime, Size: int64(len(target))}, data: []byte(target)})
}

// AddTree copies a host file or directory (recursively) to dst in the image,
// preserving permission bits, mtimes and symlinks. Other file types
// (sockets, devices, FIFOs) are rejected.
func (b *Builder) AddTree(src, dst string) error {
	return filepath.WalkDir(src, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(src, p)
		if err != nil {
			return err
		}
		name := dst
		if rel != "." {
			name = path.Join(dst, filepath.ToSlash(rel))
		}
		fi, err := os.Lstat(p)
		if err != nil {
			return err
		}
		perm := uint32(fi.Mode().Perm())
		switch {
		case fi.Mode().IsDir():
			return b.AddDir(name, perm, fi.ModTime().Unix())
		case fi.Mode().IsRegular():
			return b.add(&entry{hdr: Header{Name: name, Mode: TypeReg | perm, MTime: fi.ModTime().Unix(), Size: fi.Size()}, src: p})
		case fi.Mode()&fs.ModeSymlink != 0:
			t, err := os.Readlink(p)
			if err != nil {
				return err
			}
			return b.AddSymlink(name, t, fi.ModTime().Unix())
		default:
			return fmt.Errorf("%s: unsupported file type %s", p, fi.Mode().Type())
		}
	})
}

// Names returns the archive names in write order.
func (b *Builder) Names() []string {
	names := make([]string, 0, len(b.entries))
	for n := range b.entries {
		names = append(names, n)
	}
	// A parent is a strict prefix of its child, so plain lexical order puts
	// every directory before its contents.
	sort.Strings(names)
	return names
}

// WriteTo writes the archive (including the trailer) to w.
func (b *Builder) WriteTo(w io.Writer) (int64, error) {
	cw := NewWriter(w)
	for _, n := range b.Names() {
		e := b.entries[n]
		hdr := e.hdr
		if err := cw.WriteHeader(&hdr); err != nil {
			return cw.off, err
		}
		switch {
		case e.src != "":
			f, err := os.Open(e.src)
			if err != nil {
				return cw.off, err
			}
			_, err = io.Copy(cw, f)
			f.Close()
			if err != nil {
				return cw.off, fmt.Errorf("%s: %w (file changed while archiving?)", e.src, err)
			}
		case len(e.data) > 0:
			if _, err := cw.Write(e.data); err != nil {
				return cw.off, err
			}
		}
	}
	err := cw.Close()
	return cw.off, err
}
