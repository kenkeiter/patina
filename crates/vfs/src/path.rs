//! Linux path resolution (`fs/namei.c` semantics for the common cases).
//!
//! Resolution is split like the kernel's: [`Fs::walk_parent`] resolves every
//! component but the last (following symlinks in intermediate positions)
//! and classifies the last one; the operation then decides what to do with
//! it. [`Fs::resolve`] is the common "look the whole thing up" case.

use alloc::vec::Vec;

use crate::fs::Kind;
use crate::{e, Fs, Ino, Result, MAXSYMLINKS, NAME_MAX, PATH_MAX, ROOT_INO};

/// The final component of a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Last {
    Normal(Vec<u8>),
    Dot,
    DotDot,
    /// The path was only slashes.
    Root,
}

#[derive(Clone, Debug)]
pub(crate) struct Parent {
    /// Directory containing the last component.
    pub dir: Ino,
    pub last: Last,
    /// The path ended in one or more `/` after the last component.
    pub trailing: bool,
}

impl Parent {
    pub fn normal(&self) -> Option<&[u8]> {
        match &self.last {
            Last::Normal(n) => Some(n),
            _ => None,
        }
    }
}

/// Validate a user-supplied path.
pub(crate) fn check_path(path: &[u8]) -> Result<()> {
    if path.is_empty() {
        return Err(e::ENOENT);
    }
    if path.len() >= PATH_MAX {
        return Err(e::ENAMETOOLONG);
    }
    Ok(())
}

impl Fs {
    /// Parent of directory `ino` (`..`); the root is its own parent.
    pub(crate) fn dotdot(&self, ino: Ino) -> Result<Ino> {
        if ino == ROOT_INO {
            return Ok(ROOT_INO);
        }
        Ok(self.dir(ino)?.parent)
    }

    /// Look `name` up in directory `dir`.
    pub(crate) fn lookup_in(&self, dir: Ino, name: &[u8]) -> Result<Ino> {
        if name.len() > NAME_MAX {
            return Err(e::ENAMETOOLONG);
        }
        self.dir(dir)?.get(name).ok_or(e::ENOENT)
    }

    fn symlink_target(&self, ino: Ino) -> Option<&[u8]> {
        match &self.inodes.get(&ino)?.kind {
            Kind::Symlink(t) => Some(t),
            _ => None,
        }
    }

    /// Resolve all but the last component of `path`, relative to `start`
    /// (ignored for absolute paths). `links` counts symlinks followed so far
    /// in this resolution.
    pub(crate) fn walk_parent(&self, start: Ino, path: &[u8], links: &mut u32) -> Result<Parent> {
        if path.is_empty() {
            return Err(e::ENOENT);
        }
        let mut buf: Vec<u8> = path.to_vec();
        let mut pos = 0usize;
        let mut cur = if buf[0] == b'/' { ROOT_INO } else { start };
        if !self.node(cur)?.is_dir() {
            return Err(e::ENOTDIR);
        }
        loop {
            while pos < buf.len() && buf[pos] == b'/' {
                pos += 1;
            }
            if pos == buf.len() {
                // Only slashes (from the start, or after a symlink target
                // like "/"): the path names the directory itself.
                return Ok(Parent {
                    dir: cur,
                    last: Last::Root,
                    trailing: false,
                });
            }
            let cstart = pos;
            while pos < buf.len() && buf[pos] != b'/' {
                pos += 1;
            }
            let comp = &buf[cstart..pos];
            if comp.len() > NAME_MAX {
                return Err(e::ENAMETOOLONG);
            }
            let is_last = buf[pos..].iter().all(|&b| b == b'/');
            if is_last {
                let last = match comp {
                    b"." => Last::Dot,
                    b".." => Last::DotDot,
                    n => Last::Normal(n.to_vec()),
                };
                return Ok(Parent {
                    dir: cur,
                    last,
                    trailing: pos < buf.len(),
                });
            }
            match comp {
                b"." => {}
                b".." => cur = self.dotdot(cur)?,
                name => {
                    let child = self.lookup_in(cur, name)?;
                    let node = self.node(child)?;
                    if let Some(target) = self.symlink_target(child) {
                        *links += 1;
                        if *links > MAXSYMLINKS {
                            return Err(e::ELOOP);
                        }
                        if target.is_empty() {
                            return Err(e::ENOENT);
                        }
                        // Continue with target + rest (rest starts with '/').
                        let mut nb = Vec::with_capacity(target.len() + buf.len() - pos);
                        nb.extend_from_slice(target);
                        nb.extend_from_slice(&buf[pos..]);
                        if nb[0] == b'/' {
                            cur = ROOT_INO;
                        }
                        buf = nb;
                        pos = 0;
                    } else if node.is_dir() {
                        cur = child;
                    } else {
                        return Err(e::ENOTDIR);
                    }
                }
            }
        }
    }

    /// The inode the last component names, without following a final
    /// symlink. `ENOENT` if it does not exist.
    pub(crate) fn last_ino(&self, p: &Parent) -> Result<Ino> {
        match &p.last {
            // `.` of a removed directory still names it (stat works, as on
            // Linux); lookups *in* it fail because it has no entries.
            Last::Root | Last::Dot => Ok(p.dir),
            Last::DotDot => self.dotdot(p.dir),
            Last::Normal(n) => self.lookup_in(p.dir, n),
        }
    }

    /// Starting from a parsed parent, look the last component up, following
    /// a final symlink if `follow` (always when the path had a trailing
    /// slash). Returns the inode and the `Parent` it was found through.
    pub(crate) fn finish_lookup(
        &self,
        mut p: Parent,
        follow: bool,
        links: &mut u32,
    ) -> Result<(Ino, Parent)> {
        loop {
            let ino = self.last_ino(&p)?;
            if let Some(target) = self.symlink_target(ino) {
                if follow || p.trailing {
                    *links += 1;
                    if *links > MAXSYMLINKS {
                        return Err(e::ELOOP);
                    }
                    if target.is_empty() {
                        return Err(e::ENOENT);
                    }
                    let mut t = target.to_vec();
                    if p.trailing {
                        t.push(b'/');
                    }
                    p = self.walk_parent(p.dir, &t, links)?;
                    continue;
                }
            }
            if p.trailing && !self.node(ino)?.is_dir() {
                return Err(e::ENOTDIR);
            }
            return Ok((ino, p));
        }
    }

    /// Resolve `path` relative to directory `start`. `follow` controls a
    /// final symlink (`!AT_SYMLINK_NOFOLLOW`).
    pub fn resolve(&self, start: Ino, path: &[u8], follow: bool) -> Result<Ino> {
        check_path(path)?;
        let mut links = 0;
        let p = self.walk_parent(start, path, &mut links)?;
        Ok(self.finish_lookup(p, follow, &mut links)?.0)
    }

    /// Path of directory `ino` from the root (`getcwd`). `ENOENT` if it has
    /// been removed, `ENOTDIR` if it is not a directory.
    pub fn getcwd(&self, ino: Ino) -> Result<Vec<u8>> {
        let n = self.node(ino)?;
        if !n.is_dir() {
            return Err(e::ENOTDIR);
        }
        if n.nlink == 0 {
            return Err(e::ENOENT);
        }
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut cur = ino;
        while cur != ROOT_INO {
            let d = self.dir(cur)?;
            parts.push(&d.name);
            cur = d.parent;
            if parts.len() > PATH_MAX {
                return Err(e::ENAMETOOLONG);
            }
        }
        let mut out = Vec::new();
        for p in parts.iter().rev() {
            out.push(b'/');
            out.extend_from_slice(p);
        }
        if out.is_empty() {
            out.push(b'/');
        }
        Ok(out)
    }
}
