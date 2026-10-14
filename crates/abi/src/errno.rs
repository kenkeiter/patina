//! errno values. Identical on x86_64 and aarch64 (`asm-generic/errno*.h`).
//!
//! Syscalls return `-errno` in the result register; [`Errno::to_ret`] does
//! that conversion.

include!("errno_gen.rs");

/// A Linux errno value (always positive).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Errno(pub i32);

impl Errno {
    /// The syscall return value for this error (`-errno`).
    #[inline]
    pub const fn to_ret(self) -> isize {
        -(self.0 as isize)
    }

    /// Decode a raw syscall return value: `Some(errno)` when it lies in the
    /// kernel's error range `[-4095, -1]`.
    #[inline]
    pub const fn from_ret(ret: isize) -> Option<Errno> {
        if ret < 0 && ret >= -4095 {
            Some(Errno(-ret as i32))
        } else {
            None
        }
    }

    /// Symbolic name (`"ENOENT"`), if known.
    pub const fn name(self) -> Option<&'static str> {
        name(self.0)
    }
}

impl core::fmt::Display for Errno {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.name() {
            Some(n) => f.write_str(n),
            None => write!(f, "errno {}", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values() {
        assert_eq!(EPERM, 1);
        assert_eq!(ENOENT, 2);
        assert_eq!(EINTR, 4);
        assert_eq!(EBADF, 9);
        assert_eq!(ECHILD, 10);
        assert_eq!(EAGAIN, 11);
        assert_eq!(EWOULDBLOCK, EAGAIN);
        assert_eq!(ENOMEM, 12);
        assert_eq!(EFAULT, 14);
        assert_eq!(EEXIST, 17);
        assert_eq!(ENOTDIR, 20);
        assert_eq!(EISDIR, 21);
        assert_eq!(EINVAL, 22);
        assert_eq!(ENOTTY, 25);
        assert_eq!(ESPIPE, 29);
        assert_eq!(EPIPE, 32);
        assert_eq!(ERANGE, 34);
        assert_eq!(ENOSYS, 38);
        assert_eq!(ENOTEMPTY, 39);
        assert_eq!(ELOOP, 40);
        assert_eq!(ENOTSOCK, 88);
        assert_eq!(EOPNOTSUPP, 95);
        assert_eq!(EAFNOSUPPORT, 97);
        assert_eq!(EADDRINUSE, 98);
        assert_eq!(ECONNRESET, 104);
        assert_eq!(EISCONN, 106);
        assert_eq!(ENOTCONN, 107);
        assert_eq!(ETIMEDOUT, 110);
        assert_eq!(ECONNREFUSED, 111);
        assert_eq!(EINPROGRESS, 115);
        assert_eq!(EHWPOISON, 133);
    }

    #[test]
    fn names_and_ret() {
        assert_eq!(Errno(ENOSYS).name(), Some("ENOSYS"));
        assert_eq!(Errno(95).name(), Some("EOPNOTSUPP"));
        assert_eq!(Errno(11).name(), Some("EAGAIN"));
        assert_eq!(Errno(9999).name(), None);
        assert_eq!(Errno(ENOENT).to_ret(), -2);
        assert_eq!(Errno::from_ret(-2), Some(Errno(ENOENT)));
        assert_eq!(Errno::from_ret(-4096), None);
        assert_eq!(Errno::from_ret(5), None);
    }
}
