// fileio exercises the VFS: create/write/read/append/seek/rename/readdir/
// stat/truncate/symlink/remove under /tmp, and reads /etc/hosts.
package main

import (
	"bytes"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

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

func main() {
	dir, err := os.MkdirTemp("/tmp", "fileio-")
	check("mkdtemp /tmp", err)
	if err != nil {
		os.Exit(1)
	}
	p := filepath.Join(dir, "a.txt")

	check("write", os.WriteFile(p, []byte("hello\n"), 0o644))
	check("read", func() error {
		b, err := os.ReadFile(p)
		if err != nil {
			return err
		}
		if string(b) != "hello\n" {
			return errorf("got %q", b)
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
	q := filepath.Join(dir, "b.txt")
	check("rename", os.Rename(p, q))
	check("truncate", func() error {
		if err := os.Truncate(q, 5); err != nil {
			return err
		}
		b, err := os.ReadFile(q)
		if err != nil {
			return err
		}
		if string(b) != "hello" {
			return errorf("got %q", b)
		}
		return nil
	}())
	check("mkdir+readdir", func() error {
		if err := os.MkdirAll(filepath.Join(dir, "sub", "deeper"), 0o755); err != nil {
			return err
		}
		ents, err := os.ReadDir(dir)
		if err != nil {
			return err
		}
		var names []string
		for _, e := range ents {
			names = append(names, e.Name())
		}
		sort.Strings(names)
		if strings.Join(names, ",") != "b.txt,sub" {
			return errorf("entries %v", names)
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
		b, err := os.ReadFile(l)
		if err != nil {
			return err
		}
		if t != "b.txt" || string(b) != "hello" {
			return errorf("target %q content %q", t, b)
		}
		return nil
	}())
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
	check("read /etc/hosts", func() error {
		b, err := os.ReadFile("/etc/hosts")
		if err != nil {
			return err
		}
		if !strings.Contains(string(b), "localhost") {
			return errorf("no localhost entry in %q", b)
		}
		return nil
	}())
	check("getwd", func() error {
		wd, err := os.Getwd()
		if err != nil {
			return err
		}
		if !filepath.IsAbs(wd) {
			return errorf("cwd %q", wd)
		}
		return nil
	}())

	if failed {
		fmt.Println("FAIL fileio")
		os.Exit(1)
	}
	fmt.Println("PASS fileio")
}
