//! AF_UNIX names: filesystem paths (an `S_IFSOCK` inode whose special
//! carries the socket id), the abstract namespace (leading NUL, an
//! in-kernel table) and autobind.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use patina_abi::errno::*;
use patina_abi::fs::{AT_FDCWD, S_IFSOCK};
use patina_abi::net::UNIX_PATH_MAX;
use patina_vfs::{FileType, Special};

use super::World;
use crate::fs::namei::{self, Base, Node};
use crate::fs::vfs;
use crate::net::socket::SockAddr;
use crate::task;

/// The special of a socket inode: the id of the socket bound there. A
/// stale id (the socket was closed) reads as "nobody listening".
pub struct SockSpecial(pub u64);

impl Special for SockSpecial {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// A decoded AF_UNIX address.
pub enum Name {
    /// `addrlen == sizeof(sa_family_t)`: autobind (bind) / invalid.
    Unnamed,
    Path(Vec<u8>),
    /// Leading NUL included.
    Abstract(Vec<u8>),
}

/// Decode a `sockaddr` given to bind/connect/sendto (`EINVAL` for other
/// families and over-long names).
pub fn decode(a: &SockAddr) -> Result<Name, Errno> {
    let SockAddr::Unix(p) = a else {
        return Err(Errno(EINVAL));
    };
    if p.len() > UNIX_PATH_MAX {
        return Err(Errno(EINVAL));
    }
    Ok(match p.first() {
        None => Name::Unnamed,
        Some(0) => Name::Abstract(p.clone()),
        Some(_) => Name::Path(p.clone()),
    })
}

/// The `SockAddr` of a bound name (empty: unnamed).
pub fn addr(name: &[u8]) -> SockAddr {
    SockAddr::Unix(name.to_vec())
}

/// Resolve a pathname to the socket bound there (FS lock only):
/// `ENOENT` & co from the lookup, `ECONNREFUSED` for a non-socket inode
/// or a socket inode nobody is bound to any more (Linux
/// `unix_find_bsd`).
pub fn lookup_path(p: &[u8]) -> Result<u64, Errno> {
    namei::with_node(AT_FDCWD, p, true, false, |n| match n {
        Node::Vfs(fs, ino) => {
            if fs.file_type(ino)? != FileType::Socket {
                return Err(Errno(ECONNREFUSED));
            }
            fs.special(ino)
                .and_then(|s| s.as_any().downcast_ref::<SockSpecial>().map(|x| x.0))
                .ok_or(Errno(ECONNREFUSED))
        }
        _ => Err(Errno(ECONNREFUSED)),
    })
}

/// A destination: a path already resolved to an id, or an abstract name
/// still to be looked up under the table lock.
pub enum Dest {
    Id(u64),
    Abstract(Vec<u8>),
}

/// Resolve the path part of a destination (outside the table lock).
pub fn dest(a: &SockAddr) -> Result<Dest, Errno> {
    match decode(a)? {
        Name::Unnamed => Err(Errno(EINVAL)),
        Name::Path(p) => lookup_path(&p).map(Dest::Id),
        Name::Abstract(n) => Ok(Dest::Abstract(n)),
    }
}

impl World {
    /// The socket a destination names (`ECONNREFUSED` if none).
    pub(super) fn target(&self, d: &Dest) -> Result<u64, Errno> {
        let id = match d {
            Dest::Id(id) => *id,
            Dest::Abstract(n) => *self.abstract_names.get(n).ok_or(Errno(ECONNREFUSED))?,
        };
        if self.socks.contains_key(&id) {
            Ok(id)
        } else {
            Err(Errno(ECONNREFUSED))
        }
    }

    /// Bind socket `id` to an abstract name.
    pub(super) fn bind_abstract(&mut self, id: u64, n: Vec<u8>) -> Result<(), Errno> {
        let s = self.sock(id)?;
        if s.name.is_some() || s.binding {
            return Err(Errno(EINVAL));
        }
        if self.abstract_names.contains_key(&n) {
            return Err(Errno(EADDRINUSE));
        }
        self.abstract_names.insert(n.clone(), id);
        self.sock(id)?.name = Some(n);
        Ok(())
    }

    /// Linux `unix_autobind`: an abstract name of five hex digits, if the
    /// socket has no name yet.
    pub(super) fn autobind(&mut self, id: u64) -> Result<(), Errno> {
        let s = self.sock(id)?;
        if s.name.is_some() || s.binding {
            return Ok(());
        }
        for _ in 0..0x10_0000 {
            let k = self.next_autobind;
            self.next_autobind = (k + 1) & 0xf_ffff;
            let mut n = Vec::from([0u8]);
            n.extend_from_slice(format!("{:05x}", k).as_bytes());
            if !self.abstract_names.contains_key(&n) {
                return self.bind_abstract(id, n);
            }
        }
        Err(Errno(ENOSPC))
    }
}

/// Bind socket `id` to the pathname `p`: create the `S_IFSOCK` inode
/// (mode 0777 & ~umask; `EADDRINUSE` if the name exists).
pub fn bind_path(id: u64, p: Vec<u8>) -> Result<(), Errno> {
    super::with(|w, _| -> Result<(), Errno> {
        let s = w.sock(id)?;
        if s.name.is_some() || s.binding {
            return Err(Errno(EINVAL));
        }
        s.binding = true;
        Ok(())
    })?;
    let umask = task::process().umask.load(Ordering::Relaxed);
    let r = Base::new(AT_FDCWD, &p).and_then(|base| {
        vfs::with(|fs| {
            let start = base.start(fs)?;
            let sp: Arc<dyn Special> = Arc::new(SockSpecial(id));
            fs.mknod(start, &p, S_IFSOCK | (0o777 & !umask), 0, Some(sp))
                .map_err(|e| if e == Errno(EEXIST) { Errno(EADDRINUSE) } else { e })
        })
    });
    super::with(|w, _| {
        if let Ok(s) = w.sock(id) {
            s.binding = false;
            if r.is_ok() {
                s.name = Some(p);
            }
        }
    });
    r.map(|_| ())
}
