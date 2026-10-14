// fileio exercises the VFS through Go's os, io/fs and syscall packages:
// regular files, directories, links, metadata, cwd, umask, FIFOs, /dev,
// /proc, /etc, os.Root, and the error codes Go maps to fs.ErrExist &c.
// Prints "ok   <name>" per check and "PASS fileio" at the end, or
// "FAIL fileio: <name>: <error>" lines and "FAIL fileio" (exit 1).
package main

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/user"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"time"
)

// O_PATH (not in package syscall on amd64; same value on both arches).
const oPath = 0x200000

var failed bool

func check(name string, err error) {
	if err != nil {
		failed = true
		fmt.Printf("FAIL fileio: %s: %v\n", name, err)
		return
	}
	fmt.Printf("ok   %s\n", name)
}

func errorf(format string, a ...any) error { return fmt.Errorf(format, a...) }

// wantErrno: err must wrap errno.
func wantErrno(err error, errno syscall.Errno) error {
	if !errors.Is(err, errno) {
		return errorf("got %v, want %v", err, errno)
	}
	return nil
}

func readString(p string) (string, error) {
	b, err := os.ReadFile(p)
	return string(b), err
}

func names(dir string) ([]string, error) {
	ents, err := os.ReadDir(dir)
	if err != nil {
		return nil, err
	}
	var n []string
	for _, e := range ents {
		n = append(n, e.Name())
	}
	sort.Strings(n)
	return n, nil
}

func hasAll(have []string, want ...string) error {
	set := map[string]bool{}
	for _, h := range have {
		set[h] = true
	}
	for _, w := range want {
		if !set[w] {
			return errorf("missing %q in %v", w, have)
		}
	}
	return nil
}

func stat(p string) (*syscall.Stat_t, error) {
	fi, err := os.Stat(p)
	if err != nil {
		return nil, err
	}
	return fi.Sys().(*syscall.Stat_t), nil
}

func main() {
	dir, err := os.MkdirTemp("/tmp", "fileio-")
	check("mkdtemp /tmp", err)
	if err != nil {
		os.Exit(1)
	}
	p := filepath.Join(dir, "a.txt")

	check("write", os.WriteFile(p, []byte("hello\n"), 0o644))
	check("read", func() error {
		s, err := readString(p)
		if err != nil {
			return err
		}
		if s != "hello\n" {
			return errorf("got %q", s)
		}
		return nil
	}())
	check("append+seek", func() error {
		f, err := os.OpenFile(p, os.O_APPEND|os.O_RDWR, 0)
		if err != nil {
			return err
		}
		defer f.Close()
		if _, err := f.WriteString("world\n"); err != nil {
			return err
		}
		if _, err := f.Seek(6, io.SeekStart); err != nil {
			return err
		}
		b, err := io.ReadAll(f)
		if err != nil {
			return err
		}
		if string(b) != "world\n" {
			return errorf("after seek got %q", b)
		}
		return nil
	}())
	check("stat", func() error {
		fi, err := os.Stat(p)
		if err != nil {
			return err
		}
		if fi.Size() != 12 || !fi.Mode().IsRegular() || fi.Mode().Perm() != 0o644 {
			return errorf("size %d mode %v", fi.Size(), fi.Mode())
		}
		return nil
	}())
	check("O_EXCL EEXIST", func() error {
		_, err := os.OpenFile(p, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o644)
		if !errors.Is(err, fs.ErrExist) {
			return errorf("got %v", err)
		}
		return wantErrno(err, syscall.EEXIST)
	}())
	check("O_TRUNC", func() error {
		t := filepath.Join(dir, "trunc")
		if err := os.WriteFile(t, []byte("abcdef"), 0o644); err != nil {
			return err
		}
		f, err := os.OpenFile(t, os.O_WRONLY|os.O_TRUNC, 0)
		if err != nil {
			return err
		}
		f.Close()
		fi, err := os.Stat(t)
		if err != nil {
			return err
		}
		if fi.Size() != 0 {
			return errorf("size %d after O_TRUNC", fi.Size())
		}
		return nil
	}())
	check("EISDIR", func() error {
		_, err := os.OpenFile(dir, os.O_WRONLY, 0)
		if err := wantErrno(err, syscall.EISDIR); err != nil {
			return err
		}
		f, err := os.Open(dir)
		if err != nil {
			return err
		}
		defer f.Close()
		_, err = f.Read(make([]byte, 10))
		return wantErrno(err, syscall.EISDIR)
	}())
	check("ENOTDIR", func() error {
		_, err := os.Open(p + "/x")
		if err := wantErrno(err, syscall.ENOTDIR); err != nil {
			return err
		}
		_, err = os.ReadDir(p)
		return wantErrno(err, syscall.ENOTDIR)
	}())
	check("ENOTEMPTY", func() error {
		d := filepath.Join(dir, "full")
		if err := os.MkdirAll(filepath.Join(d, "x"), 0o755); err != nil {
			return err
		}
		return wantErrno(os.Remove(d), syscall.ENOTEMPTY)
	}())
	q := filepath.Join(dir, "b.txt")
	check("rename", os.Rename(p, q))
	check("rename over existing", func() error {
		o := filepath.Join(dir, "other")
		if err := os.WriteFile(o, []byte("old"), 0o644); err != nil {
			return err
		}
		n := filepath.Join(dir, "new")
		if err := os.WriteFile(n, []byte("new"), 0o644); err != nil {
			return err
		}
		if err := os.Rename(n, o); err != nil {
			return err
		}
		s, err := readString(o)
		if err != nil {
			return err
		}
		if s != "new" {
			return errorf("got %q", s)
		}
		if _, err := os.Stat(n); !os.IsNotExist(err) {
			return errorf("source still exists: %v", err)
		}
		return nil
	}())
	check("rename across dirs", func() error {
		d1, d2 := filepath.Join(dir, "d1"), filepath.Join(dir, "d2")
		if err := os.Mkdir(d1, 0o755); err != nil {
			return err
		}
		if err := os.Mkdir(d2, 0o755); err != nil {
			return err
		}
		if err := os.WriteFile(filepath.Join(d1, "f"), []byte("x"), 0o644); err != nil {
			return err
		}
		if err := os.Rename(filepath.Join(d1, "f"), filepath.Join(d2, "g")); err != nil {
			return err
		}
		// A directory too.
		if err := os.Rename(d2, filepath.Join(d1, "d2moved")); err != nil {
			return err
		}
		s, err := readString(filepath.Join(d1, "d2moved", "g"))
		if err != nil {
			return err
		}
		if s != "x" {
			return errorf("got %q", s)
		}
		return nil
	}())
	check("truncate", func() error {
		if err := os.Truncate(q, 5); err != nil {
			return err
		}
		s, err := readString(q)
		if err != nil {
			return err
		}
		if s != "hello" {
			return errorf("got %q", s)
		}
		return nil
	}())
	check("truncate grow", func() error {
		if err := os.Truncate(q, 10000); err != nil {
			return err
		}
		b, err := os.ReadFile(q)
		if err != nil {
			return err
		}
		if len(b) != 10000 || string(b[:5]) != "hello" || !bytes.Equal(b[5:], make([]byte, 9995)) {
			return errorf("len %d head %q", len(b), b[:5])
		}
		return os.Truncate(q, 5)
	}())
	check("mkdir+readdir", func() error {
		sub := filepath.Join(dir, "sub")
		if err := os.MkdirAll(filepath.Join(sub, "deeper"), 0o755); err != nil {
			return err
		}
		if err := os.WriteFile(filepath.Join(sub, "f"), nil, 0o644); err != nil {
			return err
		}
		n, err := names(sub)
		if err != nil {
			return err
		}
		if strings.Join(n, ",") != "deeper,f" {
			return errorf("entries %v", n)
		}
		return nil
	}())
	check("readdir rewind", func() error {
		f, err := os.Open(dir)
		if err != nil {
			return err
		}
		defer f.Close()
		a, err := f.ReadDir(-1)
		if err != nil {
			return err
		}
		if _, err := f.Seek(0, io.SeekStart); err != nil {
			return err
		}
		b, err := f.ReadDir(-1)
		if err != nil {
			return err
		}
		if len(a) == 0 || len(a) != len(b) {
			return errorf("%d then %d entries", len(a), len(b))
		}
		return nil
	}())
	check("symlink", func() error {
		l := filepath.Join(dir, "link")
		if err := os.Symlink("b.txt", l); err != nil {
			return err
		}
		t, err := os.Readlink(l)
		if err != nil {
			return err
		}
		s, err := readString(l)
		if err != nil {
			return err
		}
		fi, err := os.Lstat(l)
		if err != nil {
			return err
		}
		if t != "b.txt" || s != "hello" || fi.Mode()&fs.ModeSymlink == 0 {
			return errorf("target %q content %q mode %v", t, s, fi.Mode())
		}
		return nil
	}())
	check("hard link nlink", func() error {
		h := filepath.Join(dir, "hard")
		if err := os.Link(q, h); err != nil {
			return err
		}
		st, err := stat(q)
		if err != nil {
			return err
		}
		st2, err := stat(h)
		if err != nil {
			return err
		}
		if st.Nlink != 2 || st.Ino != st2.Ino {
			return errorf("nlink %d ino %d/%d", st.Nlink, st.Ino, st2.Ino)
		}
		if err := os.Remove(h); err != nil {
			return err
		}
		st, err = stat(q)
		if err != nil {
			return err
		}
		if st.Nlink != 1 {
			return errorf("nlink %d after unlink", st.Nlink)
		}
		return nil
	}())
	check("chmod chown chtimes", func() error {
		if err := os.Chmod(q, 0o600); err != nil {
			return err
		}
		if err := os.Chown(q, 1000, 1001); err != nil {
			return err
		}
		mt := time.Date(2020, 5, 17, 12, 34, 56, 789000000, time.UTC)
		if err := os.Chtimes(q, mt, mt); err != nil {
			return err
		}
		fi, err := os.Stat(q)
		if err != nil {
			return err
		}
		st := fi.Sys().(*syscall.Stat_t)
		if fi.Mode().Perm() != 0o600 || st.Uid != 1000 || st.Gid != 1001 || !fi.ModTime().Equal(mt) {
			return errorf("mode %v uid %d gid %d mtime %v", fi.Mode(), st.Uid, st.Gid, fi.ModTime())
		}
		return os.Chown(q, 0, 0)
	}())
	check("futimes via /proc/self/fd", func() error {
		f, err := os.Open(q)
		if err != nil {
			return err
		}
		defer f.Close()
		tv := []syscall.Timeval{{Sec: 1000000, Usec: 0}, {Sec: 2000000, Usec: 500000}}
		if err := syscall.Futimes(int(f.Fd()), tv); err != nil {
			return err
		}
		fi, err := os.Stat(q)
		if err != nil {
			return err
		}
		if fi.ModTime().Unix() != 2000000 || fi.ModTime().Nanosecond() != 500000000 {
			return errorf("mtime %v", fi.ModTime())
		}
		return nil
	}())
	legacyChecks(q)
	check("large file", func() error {
		big := bytes.Repeat([]byte("0123456789abcdef"), 64<<10) // 1 MiB
		bp := filepath.Join(dir, "big")
		if err := os.WriteFile(bp, big, 0o600); err != nil {
			return err
		}
		got, err := os.ReadFile(bp)
		if err != nil {
			return err
		}
		if !bytes.Equal(got, big) {
			return errorf("1 MiB round trip mismatch")
		}
		return nil
	}())
	check("seek past EOF hole", func() error {
		hp := filepath.Join(dir, "hole")
		f, err := os.Create(hp)
		if err != nil {
			return err
		}
		defer f.Close()
		if _, err := f.WriteString("a"); err != nil {
			return err
		}
		if _, err := f.Seek(100000, io.SeekStart); err != nil {
			return err
		}
		if _, err := f.WriteString("z"); err != nil {
			return err
		}
		b, err := os.ReadFile(hp)
		if err != nil {
			return err
		}
		if len(b) != 100001 || b[0] != 'a' || b[100000] != 'z' || !bytes.Equal(b[1:100000], make([]byte, 99999)) {
			return errorf("len %d", len(b))
		}
		end, err := f.Seek(0, io.SeekEnd)
		if err != nil || end != 100001 {
			return errorf("SEEK_END %d %v", end, err)
		}
		return nil
	}())
	check("pread/pwrite", func() error {
		f, err := os.Create(filepath.Join(dir, "pp"))
		if err != nil {
			return err
		}
		defer f.Close()
		if _, err := f.WriteAt([]byte("world"), 6); err != nil {
			return err
		}
		if _, err := f.WriteAt([]byte("hello "), 0); err != nil {
			return err
		}
		b := make([]byte, 5)
		if _, err := f.ReadAt(b, 6); err != nil {
			return err
		}
		if string(b) != "world" {
			return errorf("got %q", b)
		}
		return nil
	}())
	check("dup shares offset", func() error {
		f, err := os.Create(filepath.Join(dir, "dup"))
		if err != nil {
			return err
		}
		defer f.Close()
		if _, err := f.WriteString("abcdef"); err != nil {
			return err
		}
		fd := int(f.Fd())
		fd2, err := syscall.Dup(fd)
		if err != nil {
			return err
		}
		defer syscall.Close(fd2)
		if _, err := syscall.Seek(fd2, 2, io.SeekStart); err != nil {
			return err
		}
		b := make([]byte, 2)
		n, err := syscall.Read(fd, b)
		if err != nil {
			return err
		}
		if string(b[:n]) != "cd" {
			return errorf("got %q", b[:n])
		}
		return nil
	}())
	check("O_APPEND two fds", func() error {
		ap := filepath.Join(dir, "app")
		f1, err := os.OpenFile(ap, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o644)
		if err != nil {
			return err
		}
		defer f1.Close()
		f2, err := os.OpenFile(ap, os.O_WRONLY|os.O_APPEND, 0)
		if err != nil {
			return err
		}
		defer f2.Close()
		for i := 0; i < 3; i++ {
			f1.WriteString("A")
			f2.WriteString("B")
		}
		s, err := readString(ap)
		if err != nil {
			return err
		}
		if s != "ABABAB" {
			return errorf("got %q", s)
		}
		return nil
	}())
	check("umask", func() error {
		old := syscall.Umask(0o077)
		defer syscall.Umask(old)
		if old != 0o022 {
			return errorf("initial umask %o", old)
		}
		up := filepath.Join(dir, "umask")
		if err := os.WriteFile(up, nil, 0o666); err != nil {
			return err
		}
		if err := os.Mkdir(up+"d", 0o777); err != nil {
			return err
		}
		fi, err := os.Stat(up)
		if err != nil {
			return err
		}
		di, err := os.Stat(up + "d")
		if err != nil {
			return err
		}
		if fi.Mode().Perm() != 0o600 || di.Mode().Perm() != 0o700 {
			return errorf("file %v dir %v", fi.Mode(), di.Mode())
		}
		return nil
	}())
	check("getwd chdir relative", func() error {
		wd, err := os.Getwd()
		if err != nil {
			return err
		}
		if wd != "/" {
			return errorf("initial cwd %q", wd)
		}
		if err := os.Chdir(dir); err != nil {
			return err
		}
		defer os.Chdir("/")
		wd, err = os.Getwd()
		if err != nil {
			return err
		}
		if wd != dir {
			return errorf("cwd %q want %q", wd, dir)
		}
		s, err := readString("b.txt")
		if err != nil {
			return err
		}
		if s != "hello" {
			return errorf("relative read %q", s)
		}
		if err := os.WriteFile("./rel", []byte("r"), 0o644); err != nil {
			return err
		}
		if _, err := os.Stat(filepath.Join(dir, "rel")); err != nil {
			return err
		}
		if l, err := os.Readlink("/proc/self/cwd"); err != nil || l != dir {
			return errorf("/proc/self/cwd = %q, %v", l, err)
		}
		if _, err := os.Stat("/proc/self/cwd/rel"); err != nil {
			return err
		}
		return nil
	}())
	check("getcwd of removed dir", func() error {
		d := filepath.Join(dir, "gone")
		if err := os.Mkdir(d, 0o755); err != nil {
			return err
		}
		if err := os.Chdir(d); err != nil {
			return err
		}
		defer os.Chdir("/")
		if err := os.Remove(d); err != nil {
			return err
		}
		_, err := syscall.Getwd()
		return wantErrno(err, syscall.ENOENT)
	}())
	check("O_PATH", func() error {
		fd, err := syscall.Open(q, oPath|syscall.O_CLOEXEC, 0)
		if err != nil {
			return err
		}
		defer syscall.Close(fd)
		var st syscall.Stat_t
		if err := syscall.Fstat(fd, &st); err != nil {
			return err
		}
		if st.Size != 5 {
			return errorf("size %d", st.Size)
		}
		_, err = syscall.Read(fd, make([]byte, 4))
		return wantErrno(err, syscall.EBADF)
	}())
	check("fd fstat flock", func() error {
		f, err := os.Open(q)
		if err != nil {
			return err
		}
		defer f.Close()
		var st syscall.Stat_t
		if err := syscall.Fstat(int(f.Fd()), &st); err != nil {
			return err
		}
		if st.Size != 5 || st.Mode&syscall.S_IFMT != syscall.S_IFREG {
			return errorf("size %d mode %o", st.Size, st.Mode)
		}
		if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX); err != nil {
			return err
		}
		return syscall.Flock(int(f.Fd()), syscall.LOCK_UN)
	}())
	check("io.Copy file to file", func() error {
		src, err := os.Open(filepath.Join(dir, "big"))
		if err != nil {
			return err
		}
		defer src.Close()
		dst, err := os.Create(filepath.Join(dir, "big2"))
		if err != nil {
			return err
		}
		defer dst.Close()
		n, err := io.Copy(dst, src)
		if err != nil {
			return err
		}
		if n != 1<<20 {
			return errorf("copied %d", n)
		}
		a, _ := os.ReadFile(filepath.Join(dir, "big"))
		b, _ := os.ReadFile(filepath.Join(dir, "big2"))
		if !bytes.Equal(a, b) {
			return errorf("copy differs")
		}
		return nil
	}())
	check("CreateTemp", func() error {
		f, err := os.CreateTemp(dir, "tmp*.txt")
		if err != nil {
			return err
		}
		defer f.Close()
		if !strings.HasPrefix(filepath.Base(f.Name()), "tmp") {
			return errorf("name %q", f.Name())
		}
		_, err = os.Stat(f.Name())
		return err
	}())
	check("readlink /proc/self/fd", func() error {
		f, err := os.Open(q)
		if err != nil {
			return err
		}
		defer f.Close()
		l, err := os.Readlink(fmt.Sprintf("/proc/self/fd/%d", f.Fd()))
		if err != nil {
			return err
		}
		if l != q {
			return errorf("link %q want %q", l, q)
		}
		n, err := names("/proc/self/fd")
		if err != nil {
			return err
		}
		return hasAll(n, "0", "1", "2", fmt.Sprint(f.Fd()))
	}())
	check("open /dev/fd/N dups", func() error {
		r, w, err := os.Pipe()
		if err != nil {
			return err
		}
		defer r.Close()
		defer w.Close()
		w2, err := os.OpenFile(fmt.Sprintf("/dev/fd/%d", w.Fd()), os.O_WRONLY, 0)
		if err != nil {
			return err
		}
		if _, err := w2.WriteString("via /dev/fd"); err != nil {
			return err
		}
		w2.Close()
		b := make([]byte, 64)
		n, err := r.Read(b)
		if err != nil {
			return err
		}
		if string(b[:n]) != "via /dev/fd" {
			return errorf("got %q", b[:n])
		}
		return nil
	}())
	check("fs.WalkDir", func() error {
		count := 0
		err := fs.WalkDir(os.DirFS(dir), ".", func(path string, d fs.DirEntry, err error) error {
			if err != nil {
				return err
			}
			count++
			return nil
		})
		if err != nil {
			return err
		}
		if count < 15 {
			return errorf("only %d entries", count)
		}
		return nil
	}())
	check("os.Root", func() error {
		rd := filepath.Join(dir, "rootdir")
		if err := os.Mkdir(rd, 0o755); err != nil {
			return err
		}
		r, err := os.OpenRoot(rd)
		if err != nil {
			return err
		}
		defer r.Close()
		f, err := r.Create("x")
		if err != nil {
			return err
		}
		f.WriteString("in root")
		f.Close()
		if err := r.Mkdir("d", 0o755); err != nil {
			return err
		}
		fi, err := r.Stat("x")
		if err != nil {
			return err
		}
		if fi.Size() != 7 {
			return errorf("size %d", fi.Size())
		}
		g, err := r.Open("x")
		if err != nil {
			return err
		}
		b, _ := io.ReadAll(g)
		g.Close()
		if string(b) != "in root" {
			return errorf("got %q", b)
		}
		if _, err := r.Open("../b.txt"); err == nil {
			return errorf("escaped the root")
		}
		if err := r.Remove("x"); err != nil {
			return err
		}
		if err := r.Remove("d"); err != nil {
			return err
		}
		n, err := names(rd)
		if err != nil {
			return err
		}
		if len(n) != 0 {
			return errorf("left %v", n)
		}
		return nil
	}())
	check("FIFO", func() error {
		fp := filepath.Join(dir, "fifo")
		if err := syscall.Mkfifo(fp, 0o644); err != nil {
			return err
		}
		fi, err := os.Stat(fp)
		if err != nil {
			return err
		}
		if fi.Mode()&fs.ModeNamedPipe == 0 {
			return errorf("mode %v", fi.Mode())
		}
		done := make(chan error, 1)
		go func() {
			w, err := os.OpenFile(fp, os.O_WRONLY, 0)
			if err != nil {
				done <- err
				return
			}
			_, err = w.WriteString("through the fifo")
			w.Close()
			done <- err
		}()
		r, err := os.Open(fp)
		if err != nil {
			return err
		}
		defer r.Close()
		b, err := io.ReadAll(r)
		if err != nil {
			return err
		}
		if err := <-done; err != nil {
			return err
		}
		if string(b) != "through the fifo" {
			return errorf("got %q", b)
		}
		return nil
	}())
	check("statfs", func() error {
		var s syscall.Statfs_t
		if err := syscall.Statfs("/", &s); err != nil {
			return err
		}
		if s.Type != 0x01021994 || s.Bsize != 4096 || s.Blocks == 0 || s.Bfree > s.Blocks {
			return errorf("%+v", s)
		}
		return nil
	}())
	check("not exist", func() error {
		_, err := os.Open(filepath.Join(dir, "missing"))
		if !os.IsNotExist(err) {
			return errorf("want ENOENT, got %v", err)
		}
		return nil
	}())
	check("removeall", func() error {
		if err := os.RemoveAll(dir); err != nil {
			return err
		}
		if _, err := os.Stat(dir); !os.IsNotExist(err) {
			return errorf("dir still exists: %v", err)
		}
		return nil
	}())

	// The rest of the tree.
	check("readdir / /dev /proc /etc", func() error {
		for _, c := range []struct {
			dir  string
			want []string
		}{
			{"/", []string{"app", "dev", "etc", "proc", "root", "sys", "tmp"}},
			{"/dev", []string{"console", "fd", "full", "null", "random", "stderr", "stdin", "stdout", "urandom", "zero"}},
			{"/proc", []string{"1", "cpuinfo", "meminfo", "self", "sys"}},
			{"/proc/self", []string{"cwd", "exe", "fd", "maps", "stat", "status"}},
			{"/etc", []string{"group", "hosts", "nsswitch.conf", "passwd", "resolv.conf"}},
		} {
			n, err := names(c.dir)
			if err != nil {
				return err
			}
			if err := hasAll(n, c.want...); err != nil {
				return fmt.Errorf("%s: %w", c.dir, err)
			}
		}
		fi, err := os.Stat("/tmp")
		if err != nil {
			return err
		}
		if fi.Mode().Perm() != 0o777 || fi.Mode()&fs.ModeSticky == 0 {
			return errorf("/tmp mode %v", fi.Mode())
		}
		return nil
	}())
	check("read /etc files", func() error {
		for p, want := range map[string]string{
			"/etc/hosts":         "localhost",
			"/etc/resolv.conf":   "nameserver",
			"/etc/nsswitch.conf": "hosts:",
			"/etc/passwd":        "root:x:0:0",
		} {
			s, err := readString(p)
			if err != nil {
				return err
			}
			if !strings.Contains(s, want) {
				return errorf("%s: no %q in %q", p, want, s)
			}
		}
		u, err := user.Current()
		if err != nil {
			return err
		}
		if u.Username != "root" || u.HomeDir != "/root" {
			return errorf("user %+v", u)
		}
		h, err := os.Hostname()
		if err != nil {
			return err
		}
		if h != "patina" {
			return errorf("hostname %q", h)
		}
		return nil
	}())
	check("/proc/self/exe", func() error {
		l, err := os.Readlink("/proc/self/exe")
		if err != nil {
			return err
		}
		e, err := os.Executable()
		if err != nil {
			return err
		}
		if l != "/app" || e != "/app" {
			return errorf("readlink %q executable %q", l, e)
		}
		return nil
	}())
	check("/proc files", func() error {
		for p, want := range map[string]string{
			"/proc/self/maps":                 "[stack]",
			"/proc/cpuinfo":                   "processor",
			"/proc/meminfo":                   "MemTotal:",
			"/proc/self/status":               "Threads:",
			"/proc/self/stat":                 "1 (app) R",
			"/proc/uptime":                    ".",
			"/proc/version":                   "Linux version",
			"/proc/mounts":                    "rootfs / ",
			"/proc/sys/kernel/osrelease":      "patina",
			"/proc/sys/kernel/random/boot_id": "-",
			"/sys/kernel/mm/transparent_hugepage/hpage_pmd_size": "2097152\n",
		} {
			s, err := readString(p)
			if err != nil {
				return err
			}
			if !strings.Contains(s, want) {
				return errorf("%s: no %q in %q", p, want, s)
			}
		}
		maps, _ := readString("/proc/self/maps")
		if !strings.Contains(maps, "/app") {
			return errorf("no /app in maps:\n%s", maps)
		}
		st, _ := readString("/proc/self/stat")
		if n := len(strings.Fields(st)); n != 52 {
			return errorf("/proc/self/stat has %d fields", n)
		}
		return nil
	}())
	check("/dev/null /dev/zero /dev/urandom", func() error {
		n, err := os.OpenFile("/dev/null", os.O_RDWR, 0)
		if err != nil {
			return err
		}
		defer n.Close()
		if c, err := n.Read(make([]byte, 10)); c != 0 || err != io.EOF {
			return errorf("/dev/null read %d %v", c, err)
		}
		if c, err := n.WriteString("discard"); c != 7 || err != nil {
			return errorf("/dev/null write %d %v", c, err)
		}
		var st syscall.Stat_t
		if err := syscall.Fstat(int(n.Fd()), &st); err != nil {
			return err
		}
		if st.Mode&syscall.S_IFMT != syscall.S_IFCHR || st.Rdev != 0x103 {
			return errorf("/dev/null mode %o rdev %#x", st.Mode, st.Rdev)
		}
		z, err := os.Open("/dev/zero")
		if err != nil {
			return err
		}
		defer z.Close()
		zb := bytes.Repeat([]byte{1}, 4096)
		if _, err := io.ReadFull(z, zb); err != nil {
			return err
		}
		if !bytes.Equal(zb, make([]byte, 4096)) {
			return errorf("/dev/zero not zero")
		}
		u, err := os.Open("/dev/urandom")
		if err != nil {
			return err
		}
		defer u.Close()
		ub := make([]byte, 32)
		if _, err := io.ReadFull(u, ub); err != nil {
			return err
		}
		if bytes.Equal(ub, make([]byte, 32)) {
			return errorf("/dev/urandom all zero")
		}
		err = os.WriteFile("/dev/full", []byte("x"), 0)
		return wantErrno(err, syscall.ENOSPC)
	}())
	check("/dev/stdout", func() error {
		f, err := os.OpenFile("/dev/stdout", os.O_WRONLY, 0)
		if err != nil {
			return err
		}
		defer f.Close()
		_, err = f.WriteString("ok   (written through /dev/stdout)\n")
		return err
	}())

	if failed {
		fmt.Println("FAIL fileio")
		os.Exit(1)
	}
	fmt.Println("PASS fileio")
}
