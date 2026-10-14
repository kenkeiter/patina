//! AF_UNIX building blocks that need no kernel (host-tested): the stream
//! receive buffer with ancillary-data marks, and the `cmsg` codec used by
//! `sendmsg`/`recvmsg` (Linux `__scm_send`, `put_cmsg`, `scm_detach_fds`).

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use patina_abi::net::{Cmsghdr, cmsg_align, cmsg_len, cmsg_space};

// ---------------------------------------------------------------------------
// Stream receive buffer.

/// One send that carried ancillary data: the bytes `[start, end)` (absolute
/// stream offsets) and the payload (e.g. the passed files).
#[derive(Debug)]
struct Mark<T> {
    start: u64,
    end: u64,
    val: T,
}

/// A bounded byte FIFO in which a send's ancillary data travels with its
/// bytes, like Linux's per-skb `UNIXCB(skb).fp` on a stream socket:
///
/// * a read that reaches a marked segment's first byte takes the payload
///   and stops at the end of that segment (Linux breaks out of
///   `unix_stream_read_generic` after the skb that carried fds), so the
///   payload arrives with the first byte of the send it was sent with;
/// * a payload is handed out once (a peek clones it, as `MSG_PEEK` dups
///   the fds).
#[derive(Debug)]
pub struct StreamBuf<T> {
    data: VecDeque<u8>,
    cap: usize,
    /// Absolute offset of `data[0]`.
    head: u64,
    marks: VecDeque<Mark<T>>,
}

impl<T> StreamBuf<T> {
    pub const fn new(cap: usize) -> StreamBuf<T> {
        StreamBuf {
            data: VecDeque::new(),
            cap,
            head: 0,
            marks: VecDeque::new(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn space(&self) -> usize {
        self.cap - self.data.len()
    }

    /// Append as much of `data` as fits. `val` is attached to the bytes
    /// taken; it is handed back when nothing could be taken.
    pub fn push(&mut self, data: &[u8], val: Option<T>) -> (usize, Option<T>) {
        let n = data.len().min(self.space());
        if n == 0 {
            return (0, val);
        }
        if let Some(val) = val {
            let start = self.head + self.data.len() as u64;
            self.marks.push_back(Mark {
                start,
                end: start + n as u64,
                val,
            });
        }
        self.data.extend(&data[..n]);
        (n, None)
    }

    /// How many bytes one read of up to `want` bytes takes, and whether it
    /// reaches the first mark.
    fn span(&self, want: usize) -> (usize, bool) {
        match self.marks.front() {
            None => (want.min(self.data.len()), false),
            Some(m) => {
                let to_start = (m.start - self.head) as usize;
                let n = want.min((m.end - self.head) as usize);
                (n, n > to_start)
            }
        }
    }

    fn copy_out(&self, out: &mut [u8]) {
        let (a, b) = self.data.as_slices();
        let n = out.len();
        let first = n.min(a.len());
        out[..first].copy_from_slice(&a[..first]);
        out[first..n].copy_from_slice(&b[..n - first]);
    }

    /// Remove up to `out.len()` bytes into `out`: the count, and the
    /// payload of the mark the read reached (if any).
    pub fn pop(&mut self, out: &mut [u8]) -> (usize, Option<T>) {
        let (n, hit) = self.span(out.len());
        self.copy_out(&mut out[..n]);
        self.data.drain(..n);
        self.head += n as u64;
        let val = if hit {
            self.marks.pop_front().map(|m| m.val)
        } else {
            None
        };
        (n, val)
    }

    /// Like [`StreamBuf::pop`] without consuming (`MSG_PEEK`).
    pub fn peek(&self, out: &mut [u8]) -> (usize, Option<&T>) {
        let (n, hit) = self.span(out.len());
        self.copy_out(&mut out[..n]);
        let val = if hit {
            self.marks.front().map(|m| &m.val)
        } else {
            None
        };
        (n, val)
    }

    /// Drop all data; the payloads still queued are returned (so the
    /// caller can release them outside its locks).
    pub fn clear(&mut self) -> Vec<T> {
        self.head += self.data.len() as u64;
        self.data.clear();
        self.marks.drain(..).map(|m| m.val).collect()
    }
}

// ---------------------------------------------------------------------------
// cmsg decoding (sendmsg).

/// Size of `struct cmsghdr`.
pub const CMSG_HDR: usize = core::mem::size_of::<Cmsghdr>();

/// `SCM_MAX_FD`: most fds one message may carry.
pub const SCM_MAX_FD: usize = 253;

/// One control message: level, type, data.
pub type Cmsg<'a> = (i32, i32, &'a [u8]);

/// Split a `msg_control` buffer into its messages, walking it like
/// `for_each_cmsghdr` + `CMSG_OK`. `Err(())` (`EINVAL`) when a header's
/// `cmsg_len` is shorter than the header or runs past the buffer.
#[allow(clippy::result_unit_err)]
pub fn parse_cmsgs(buf: &[u8]) -> Result<Vec<Cmsg<'_>>, ()> {
    let mut out = Vec::new();
    let mut off = 0usize;
    // CMSG_FIRSTHDR / CMSG_NXTHDR: a header must fit entirely.
    while buf.len() >= off + CMSG_HDR {
        let len = u64::from_ne_bytes(buf[off..off + 8].try_into().unwrap());
        let level = i32::from_ne_bytes(buf[off + 8..off + 12].try_into().unwrap());
        let ty = i32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap());
        if len < CMSG_HDR as u64 || len > (buf.len() - off) as u64 {
            return Err(());
        }
        let len = len as usize;
        out.push((level, ty, &buf[off + cmsg_align(CMSG_HDR)..off + len]));
        off += cmsg_align(len);
    }
    Ok(out)
}

/// The fds of an `SCM_RIGHTS` payload (`(len - CMSG_LEN(0)) / 4` ints).
pub fn rights_fds(data: &[u8]) -> impl Iterator<Item = i32> + '_ {
    data.chunks_exact(4)
        .map(|c| i32::from_ne_bytes(c.try_into().unwrap()))
}

// ---------------------------------------------------------------------------
// cmsg encoding (recvmsg).

/// Builds the `msg_control` bytes of one receive into `room` bytes the way
/// Linux does (`put_cmsg`, `scm_detach_fds`): records that do not fit are
/// cut (`cmsg_len` shows the cut length) and set [`CmsgWriter::ctrunc`].
#[derive(Debug)]
pub struct CmsgWriter {
    pub out: Vec<u8>,
    room: usize,
    pub ctrunc: bool,
}

impl CmsgWriter {
    pub fn new(room: usize) -> CmsgWriter {
        CmsgWriter {
            out: Vec::new(),
            room,
            ctrunc: false,
        }
    }

    /// Bytes still free.
    pub fn room(&self) -> usize {
        self.room
    }

    fn header(&mut self, len: usize, level: i32, ty: i32) {
        self.out.extend_from_slice(&(len as u64).to_ne_bytes());
        self.out.extend_from_slice(&level.to_ne_bytes());
        self.out.extend_from_slice(&ty.to_ne_bytes());
    }

    /// `put_cmsg`.
    pub fn put(&mut self, level: i32, ty: i32, data: &[u8]) {
        if self.room < CMSG_HDR {
            self.ctrunc = true;
            return;
        }
        let mut cmlen = cmsg_len(data.len());
        if self.room < cmlen {
            self.ctrunc = true;
            cmlen = self.room;
        }
        self.header(cmlen, level, ty);
        self.out.extend_from_slice(&data[..cmlen - CMSG_HDR]);
        let used = cmsg_space(data.len()).min(self.room);
        self.out.resize(self.out.len() + (used - cmlen), 0);
        self.room -= used;
    }

    /// How many fds an `SCM_RIGHTS` record may carry (`fdmax`).
    pub fn fd_room(&self) -> usize {
        if self.room > CMSG_HDR {
            ((self.room - CMSG_HDR) / 4).min(SCM_MAX_FD)
        } else {
            0
        }
    }

    /// `scm_detach_fds` after installing `fds` of `total` passed files.
    pub fn put_rights(&mut self, level: i32, ty: i32, fds: &[i32], total: usize) {
        if !fds.is_empty() {
            let cmlen = cmsg_len(fds.len() * 4);
            self.header(cmlen, level, ty);
            for fd in fds {
                self.out.extend_from_slice(&fd.to_ne_bytes());
            }
            let used = cmsg_space(fds.len() * 4).min(self.room);
            self.out.resize(self.out.len() + (used - cmlen), 0);
            self.room -= used;
        }
        if fds.len() < total {
            self.ctrunc = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use patina_abi::net::{SCM_CREDENTIALS, SCM_RIGHTS, SOL_SOCKET};

    #[test]
    fn stream_plain() {
        let mut b: StreamBuf<u32> = StreamBuf::new(8);
        assert_eq!(b.push(b"hello world", None), (8, None));
        assert_eq!(b.space(), 0);
        assert_eq!(b.push(b"x", Some(7)), (0, Some(7)));
        let mut o = [0u8; 5];
        assert_eq!(b.peek(&mut o), (5, None));
        assert_eq!(&o, b"hello");
        assert_eq!(b.pop(&mut o), (5, None));
        assert_eq!(b.len(), 3);
        assert_eq!(b.push(b"abcdef", None).0, 5);
        let mut o = [0u8; 16];
        assert_eq!(b.pop(&mut o).0, 8);
        assert_eq!(&o[..8], b" woabcde");
        assert!(b.is_empty());
    }

    #[test]
    fn stream_marks() {
        let mut b: StreamBuf<u32> = StreamBuf::new(64);
        b.push(b"aaaa", None);
        b.push(b"bbbb", Some(1));
        b.push(b"cccc", Some(2));
        b.push(b"dddd", None);
        let mut o = [0u8; 2];
        // A short read before the mark: no payload.
        assert_eq!(b.pop(&mut o), (2, None));
        let mut o = [0u8; 64];
        // Reads through the first mark's segment and stops at its end.
        assert_eq!(b.peek(&mut o), (6, Some(&1)));
        assert_eq!(b.pop(&mut o), (6, Some(1)));
        assert_eq!(&o[..6], b"aabbbb");
        // A partial read of a marked segment takes the payload.
        let mut o2 = [0u8; 1];
        assert_eq!(b.pop(&mut o2), (1, Some(2)));
        assert_eq!(b.pop(&mut o), (7, None));
        assert_eq!(&o[..7], b"cccdddd");
        b.push(b"e", Some(3));
        assert_eq!(b.clear(), [3]);
        assert!(b.is_empty());
        b.push(b"f", Some(4));
        assert_eq!(b.pop(&mut o), (1, Some(4)));
    }

    fn rec(level: i32, ty: i32, data: &[u8]) -> Vec<u8> {
        let mut w = CmsgWriter::new(1024);
        w.put(level, ty, data);
        w.out
    }

    #[test]
    fn cmsg_roundtrip() {
        let mut buf = rec(SOL_SOCKET, SCM_RIGHTS, &[3, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0]);
        assert_eq!(buf.len(), cmsg_space(12));
        buf.extend(rec(SOL_SOCKET, SCM_CREDENTIALS, &[0; 12]));
        let v = parse_cmsgs(&buf).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].0, v[0].1), (SOL_SOCKET, SCM_RIGHTS));
        assert_eq!(rights_fds(v[0].2).collect::<Vec<_>>(), [3, 4, 5]);
        assert_eq!(v[1].2.len(), 12);
        // Trailing bytes too short for a header are ignored.
        buf.extend([0u8; 8]);
        assert_eq!(parse_cmsgs(&buf).unwrap().len(), 2);
        // A record running past the end is EINVAL.
        let mut bad = rec(SOL_SOCKET, SCM_RIGHTS, &[1, 0, 0, 0]);
        bad[0] = 64;
        assert!(parse_cmsgs(&bad).is_err());
        bad[0] = 4;
        assert!(parse_cmsgs(&bad).is_err());
        assert!(parse_cmsgs(&[]).unwrap().is_empty());
    }

    #[test]
    fn cmsg_truncation() {
        // put_cmsg into too little room: cut, CTRUNC.
        let mut w = CmsgWriter::new(20);
        w.put(SOL_SOCKET, SCM_CREDENTIALS, &[9; 12]);
        assert!(w.ctrunc);
        assert_eq!(w.out.len(), 20);
        assert_eq!(u64::from_ne_bytes(w.out[..8].try_into().unwrap()), 20);
        // Rights: CMSG_SPACE(4) = 24 holds one fd of three.
        let mut w = CmsgWriter::new(24);
        assert_eq!(w.fd_room(), 2);
        w.put_rights(SOL_SOCKET, SCM_RIGHTS, &[7], 3);
        assert!(w.ctrunc);
        assert_eq!(w.out.len(), 24);
        assert_eq!(u64::from_ne_bytes(w.out[..8].try_into().unwrap()), 20);
        assert_eq!(w.room(), 0);
        let mut w = CmsgWriter::new(16);
        assert_eq!(w.fd_room(), 0);
        w.put_rights(SOL_SOCKET, SCM_RIGHTS, &[], 1);
        assert!(w.ctrunc && w.out.is_empty());
        // Exact fit, no truncation; CMSG_LEN(8) = 24 needs no padding.
        let mut w = CmsgWriter::new(24);
        w.put_rights(SOL_SOCKET, SCM_RIGHTS, &[1, 2], 2);
        assert!(!w.ctrunc);
        assert_eq!(w.out.len(), 24);
    }
}
