//! Thread credentials and Linux's rules for changing them: the
//! `set*uid` / `set*gid` / `setgroups` family (`kernel/sys.c`,
//! `kernel/groups.c`) and the capability fix-ups that go with a uid change
//! (`security/commoncap.c: cap_emulate_setxuid`).
//!
//! Capabilities are modelled as all-or-nothing: the process starts as
//! root with every capability permitted and effective, and a uid change
//! can drop the effective set (euid 0 → non-zero), restore it (back to
//! euid 0) or drop the permitted set too (no uid left at 0, unless
//! `PR_SET_KEEPCAPS`). `CAP_SETUID`/`CAP_SETGID` are "the effective set is
//! full". The values are reported by `getuid` & co., `/proc/<pid>/status`
//! and `capget`; nothing else checks them (patina does not enforce file
//! permissions).

use crate::errno::{Errno, EINVAL, EPERM};

/// `NGROUPS_MAX` (`include/uapi/linux/limits.h`).
pub const NGROUPS_MAX: usize = 65536;

/// `-1` as a `uid_t`/`gid_t` argument: leave this id unchanged.
pub const ID_UNCHANGED: u32 = u32::MAX;

/// Effective/permitted capability mask with every capability of Linux 6.x
/// (`CAP_LAST_CAP` = 40: `0x1ff_ffff_ffff`).
pub const CAP_FULL: u64 = (1 << 41) - 1;

/// Real, effective, saved and filesystem ids (`struct cred`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ids {
    pub r: u32,
    pub e: u32,
    pub s: u32,
    pub fs: u32,
}

impl Ids {
    fn has(&self, id: u32) -> bool {
        id == self.r || id == self.e || id == self.s
    }
}

/// One thread's credentials (supplementary groups are kept separately by
/// the kernel; see [`check_setgroups`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cred {
    pub uid: Ids,
    pub gid: Ids,
    /// Effective / permitted capability sets (all or nothing: 0 or
    /// [`CAP_FULL`]).
    pub cap_eff: u64,
    pub cap_prm: u64,
    /// `PR_SET_KEEPCAPS` (`SECURE_KEEP_CAPS`).
    pub keepcaps: bool,
}

impl Default for Cred {
    fn default() -> Self {
        Self::root()
    }
}

fn valid(id: u32) -> Result<u32, Errno> {
    if id == ID_UNCHANGED {
        Err(Errno(EINVAL))
    } else {
        Ok(id)
    }
}

impl Cred {
    /// uid/gid 0 with every capability.
    pub const fn root() -> Cred {
        Cred {
            uid: Ids {
                r: 0,
                e: 0,
                s: 0,
                fs: 0,
            },
            gid: Ids {
                r: 0,
                e: 0,
                s: 0,
                fs: 0,
            },
            cap_eff: CAP_FULL,
            cap_prm: CAP_FULL,
            keepcaps: false,
        }
    }

    /// `CAP_SETUID` / `CAP_SETGID` (and every other capability).
    pub fn privileged(&self) -> bool {
        self.cap_eff == CAP_FULL
    }

    /// `cap_emulate_setxuid` after a uid change from `old`.
    fn fix_caps(&mut self, old: &Ids) {
        let new = self.uid;
        if (old.r == 0 || old.e == 0 || old.s == 0)
            && new.r != 0
            && new.e != 0
            && new.s != 0
            && !self.keepcaps
        {
            self.cap_prm = 0;
            self.cap_eff = 0;
        }
        if old.e == 0 && new.e != 0 {
            self.cap_eff = 0;
        }
        if old.e != 0 && new.e == 0 {
            self.cap_eff = self.cap_prm;
        }
    }

    fn commit_uid(&mut self, uid: Ids) {
        let old = self.uid;
        self.uid = uid;
        self.fix_caps(&old);
    }

    /// `setuid(uid)`.
    pub fn setuid(&mut self, uid: u32) -> Result<(), Errno> {
        let uid = valid(uid)?;
        let mut n = self.uid;
        if self.privileged() {
            n.r = uid;
            n.s = uid;
        } else if uid != n.r && uid != n.s {
            return Err(Errno(EPERM));
        }
        n.e = uid;
        n.fs = uid;
        self.commit_uid(n);
        Ok(())
    }

    /// `setreuid(ruid, euid)` (`-1` = unchanged).
    pub fn setreuid(&mut self, r: u32, e: u32) -> Result<(), Errno> {
        let (o, p) = (self.uid, self.privileged());
        let mut n = o;
        if r != ID_UNCHANGED {
            n.r = r;
            if o.r != r && o.e != r && !p {
                return Err(Errno(EPERM));
            }
        }
        if e != ID_UNCHANGED {
            n.e = e;
            if !o.has(e) && !p {
                return Err(Errno(EPERM));
            }
        }
        if r != ID_UNCHANGED || (e != ID_UNCHANGED && e != o.r) {
            n.s = n.e;
        }
        n.fs = n.e;
        self.commit_uid(n);
        Ok(())
    }

    /// `setresuid(ruid, euid, suid)` (`-1` = unchanged).
    pub fn setresuid(&mut self, r: u32, e: u32, s: u32) -> Result<(), Errno> {
        let o = self.uid;
        if !self.privileged() {
            for id in [r, e, s] {
                if id != ID_UNCHANGED && !o.has(id) {
                    return Err(Errno(EPERM));
                }
            }
        }
        let mut n = o;
        if r != ID_UNCHANGED {
            n.r = r;
        }
        if e != ID_UNCHANGED {
            n.e = e;
        }
        if s != ID_UNCHANGED {
            n.s = s;
        }
        n.fs = n.e;
        self.commit_uid(n);
        Ok(())
    }

    /// `setfsuid(uid)`: returns the previous fsuid whether or not the
    /// change was allowed.
    pub fn setfsuid(&mut self, uid: u32) -> u32 {
        let old = self.uid.fs;
        if uid != ID_UNCHANGED && (self.uid.has(uid) || uid == old || self.privileged()) {
            self.uid.fs = uid;
        }
        old
    }

    /// `setgid(gid)`.
    pub fn setgid(&mut self, gid: u32) -> Result<(), Errno> {
        let gid = valid(gid)?;
        let n = &mut self.gid;
        if self.cap_eff == CAP_FULL {
            n.r = gid;
            n.s = gid;
        } else if gid != n.r && gid != n.s {
            return Err(Errno(EPERM));
        }
        n.e = gid;
        n.fs = gid;
        Ok(())
    }

    /// `setregid(rgid, egid)`.
    pub fn setregid(&mut self, r: u32, e: u32) -> Result<(), Errno> {
        let (o, p) = (self.gid, self.privileged());
        let mut n = o;
        if r != ID_UNCHANGED {
            n.r = r;
            if o.r != r && o.e != r && !p {
                return Err(Errno(EPERM));
            }
        }
        if e != ID_UNCHANGED {
            n.e = e;
            if !o.has(e) && !p {
                return Err(Errno(EPERM));
            }
        }
        if r != ID_UNCHANGED || (e != ID_UNCHANGED && e != o.r) {
            n.s = n.e;
        }
        n.fs = n.e;
        self.gid = n;
        Ok(())
    }

    /// `setresgid(rgid, egid, sgid)`.
    pub fn setresgid(&mut self, r: u32, e: u32, s: u32) -> Result<(), Errno> {
        let o = self.gid;
        if !self.privileged() {
            for id in [r, e, s] {
                if id != ID_UNCHANGED && !o.has(id) {
                    return Err(Errno(EPERM));
                }
            }
        }
        let n = &mut self.gid;
        if r != ID_UNCHANGED {
            n.r = r;
        }
        if e != ID_UNCHANGED {
            n.e = e;
        }
        if s != ID_UNCHANGED {
            n.s = s;
        }
        n.fs = n.e;
        Ok(())
    }

    /// `setfsgid(gid)`: returns the previous fsgid.
    pub fn setfsgid(&mut self, gid: u32) -> u32 {
        let old = self.gid.fs;
        if gid != ID_UNCHANGED && (self.gid.has(gid) || gid == old || self.privileged()) {
            self.gid.fs = gid;
        }
        old
    }

    /// `PR_SET_KEEPCAPS`.
    pub fn set_keepcaps(&mut self, v: u64) -> Result<(), Errno> {
        if v > 1 {
            return Err(Errno(EINVAL));
        }
        self.keepcaps = v == 1;
        Ok(())
    }
}

/// `setgroups(size, list)` checks: `CAP_SETGID` and the size limit.
pub fn check_setgroups(c: &Cred, size: usize) -> Result<(), Errno> {
    if size > NGROUPS_MAX {
        return Err(Errno(EINVAL));
    }
    if !c.privileged() {
        return Err(Errno(EPERM));
    }
    Ok(())
}

/// `getgroups(size, list)` with `n` groups: how many to copy (0 with
/// `size` 0 means "just report n"), or `EINVAL` if they do not fit.
pub fn getgroups_count(size: i32, n: usize) -> Result<usize, Errno> {
    if size < 0 {
        return Err(Errno(EINVAL));
    }
    if size == 0 {
        return Ok(0);
    }
    if (size as usize) < n {
        return Err(Errno(EINVAL));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: u32 = ID_UNCHANGED;

    fn uid(c: &Cred) -> [u32; 4] {
        [c.uid.r, c.uid.e, c.uid.s, c.uid.fs]
    }
    fn gid(c: &Cred) -> [u32; 4] {
        [c.gid.r, c.gid.e, c.gid.s, c.gid.fs]
    }

    /// The sequence of Go's syscall TestSetuidEtc, with the
    /// `/proc/<pid>/status` `Uid:`/`Gid:` values it expects.
    #[test]
    fn go_setuid_etc_sequence() {
        let mut c = Cred::root();
        // Setegid(1) = setresgid(-1, 1, -1).
        c.setresgid(U, 1, U).unwrap();
        assert_eq!(gid(&c), [0, 1, 0, 1]);
        c.setresgid(U, 0, U).unwrap();
        assert_eq!(gid(&c), [0, 0, 0, 0]);
        // Seteuid(1): caps dropped from the effective set only.
        c.setresuid(U, 1, U).unwrap();
        assert_eq!(uid(&c), [0, 1, 0, 1]);
        assert!(!c.privileged());
        assert_eq!(c.cap_prm, CAP_FULL);
        // Setuid(0) unprivileged: euid/fsuid only; caps come back.
        c.setuid(0).unwrap();
        assert_eq!(uid(&c), [0, 0, 0, 0]);
        assert!(c.privileged());
        c.setgid(1).unwrap();
        assert_eq!(gid(&c), [1, 1, 1, 1]);
        c.setgid(0).unwrap();
        c.setregid(101, 0).unwrap();
        assert_eq!(gid(&c), [101, 0, 0, 0]);
        c.setregid(0, 102).unwrap();
        assert_eq!(gid(&c), [0, 102, 102, 102]);
        c.setregid(0, 0).unwrap();
        assert_eq!(gid(&c), [0, 0, 0, 0]);
        c.setreuid(1, 0).unwrap();
        assert_eq!(uid(&c), [1, 0, 0, 0]);
        c.setreuid(0, 2).unwrap();
        assert_eq!(uid(&c), [0, 2, 2, 2]);
        // ruid 0 left: permitted caps kept, effective dropped.
        assert_eq!((c.cap_prm, c.cap_eff), (CAP_FULL, 0));
        c.setreuid(0, 0).unwrap();
        assert_eq!(uid(&c), [0, 0, 0, 0]);
        assert!(c.privileged());
        c.setresgid(101, 0, 102).unwrap();
        assert_eq!(gid(&c), [101, 0, 102, 0]);
        c.setresgid(0, 102, 101).unwrap();
        assert_eq!(gid(&c), [0, 102, 101, 102]);
        c.setresgid(0, 0, 0).unwrap();
        c.setresuid(1, 0, 2).unwrap();
        assert_eq!(uid(&c), [1, 0, 2, 0]);
        c.setresuid(0, 2, 1).unwrap();
        assert_eq!(uid(&c), [0, 2, 1, 2]);
        c.setresuid(0, 0, 0).unwrap();
        assert_eq!(uid(&c), [0, 0, 0, 0]);
        assert_eq!(c, Cred::root());
    }

    #[test]
    fn dropping_root_for_good() {
        let mut c = Cred::root();
        c.setuid(1000).unwrap();
        assert_eq!(uid(&c), [1000; 4]);
        assert_eq!((c.cap_prm, c.cap_eff), (0, 0));
        assert_eq!(c.setuid(0), Err(Errno(EPERM)));
        assert_eq!(c.setresuid(U, 0, U), Err(Errno(EPERM)));
        assert_eq!(c.setgid(5), Err(Errno(EPERM)));
        assert_eq!(check_setgroups(&c, 1), Err(Errno(EPERM)));
        // setfsuid: refused, still returns the old value.
        assert_eq!(c.setfsuid(0), 1000);
        assert_eq!(c.uid.fs, 1000);
        assert_eq!(c.setuid(U), Err(Errno(EINVAL)));
    }

    #[test]
    fn keepcaps() {
        let mut c = Cred::root();
        assert_eq!(c.set_keepcaps(2), Err(Errno(EINVAL)));
        c.set_keepcaps(1).unwrap();
        c.setresuid(1000, 1000, 1000).unwrap();
        // Permitted kept; effective cleared by euid 0 -> 1000.
        assert_eq!((c.cap_prm, c.cap_eff), (CAP_FULL, 0));
    }

    #[test]
    fn unprivileged_swaps() {
        let mut c = Cred::root();
        c.setresuid(1, 2, 3).unwrap();
        assert!(!c.privileged());
        // Any of r/e/s may be swapped around.
        c.setresuid(3, 1, 2).unwrap();
        assert_eq!(uid(&c), [3, 1, 2, 1]);
        assert_eq!(c.setresuid(4, U, U), Err(Errno(EPERM)));
        // setreuid: ruid may become the euid; euid any of r/e/s.
        c.setreuid(1, 2).unwrap();
        assert_eq!(uid(&c), [1, 2, 2, 2]);
        assert_eq!(c.setreuid(7, U), Err(Errno(EPERM)));
    }

    #[test]
    fn groups_limits() {
        let c = Cred::root();
        assert_eq!(check_setgroups(&c, NGROUPS_MAX + 1), Err(Errno(EINVAL)));
        assert_eq!(check_setgroups(&c, NGROUPS_MAX), Ok(()));
        assert_eq!(getgroups_count(0, 4), Ok(0));
        assert_eq!(getgroups_count(3, 4), Err(Errno(EINVAL)));
        assert_eq!(getgroups_count(4, 4), Ok(4));
        assert_eq!(getgroups_count(-1, 0), Err(Errno(EINVAL)));
    }
}
