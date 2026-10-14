//! RAII inode references over a lock-wrapped [`Fs`].

use crate::{Fs, Ino, Result};

/// Access to the shared [`Fs`] under its lock. The kernel implements this
/// for (a reference to) its global `SpinLock<Fs>`; tests use
/// `Arc<Mutex<Fs>>`. `with` must not be called while the same lock is
/// already held by the caller.
pub trait FsRef: Clone {
    fn with<R>(&self, f: impl FnOnce(&mut Fs) -> R) -> R;
}

/// One counted reference on an inode: keeps it (and its data) alive after
/// it is unlinked. Cloning takes another reference; dropping releases one
/// (locking the filesystem, so never drop a `Handle` while holding it).
pub struct Handle<P: FsRef> {
    /// `None` only after `into_raw`.
    fs: Option<P>,
    ino: Ino,
}

impl<P: FsRef> Handle<P> {
    /// Take a new reference on `ino`.
    pub fn new(fs: P, ino: Ino) -> Result<Self> {
        fs.with(|f| f.get_ref(ino))?;
        Ok(Self::adopt(fs, ino))
    }

    /// Wrap a reference the caller already owns (e.g. from [`Fs::open`]).
    pub fn adopt(fs: P, ino: Ino) -> Self {
        Handle { fs: Some(fs), ino }
    }

    pub fn ino(&self) -> Ino {
        self.ino
    }

    fn fsref(&self) -> &P {
        match &self.fs {
            Some(p) => p,
            None => unreachable!("Handle used after into_raw"),
        }
    }

    pub fn fs(&self) -> &P {
        self.fsref()
    }

    /// Run `f` with the filesystem locked.
    pub fn with<R>(&self, f: impl FnOnce(&mut Fs, Ino) -> R) -> R {
        let ino = self.ino;
        self.fsref().with(|fs| f(fs, ino))
    }

    /// Give up the wrapper without releasing the reference (the caller
    /// takes over responsibility for [`Fs::put_ref`]).
    pub fn into_raw(mut self) -> (P, Ino) {
        let fs = self.fs.take();
        match fs {
            Some(p) => (p, self.ino),
            None => unreachable!(),
        }
    }
}

impl<P: FsRef> Clone for Handle<P> {
    fn clone(&self) -> Self {
        let ino = self.ino;
        // The inode is alive while `self` holds its reference, so this
        // cannot fail.
        let _ = self.fsref().with(|f| f.get_ref(ino));
        Handle::adopt(self.fsref().clone(), ino)
    }
}

impl<P: FsRef> Drop for Handle<P> {
    fn drop(&mut self) {
        if let Some(p) = &self.fs {
            let ino = self.ino;
            p.with(|f| f.put_ref(ino));
        }
    }
}

impl<P: FsRef> core::fmt::Debug for Handle<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Handle").field("ino", &self.ino).finish()
    }
}
