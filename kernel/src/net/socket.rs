//! The socket seam between the syscall layer (`syscall::net`) and socket
//! implementations (D68).
//!
//! A socket is an `fs::File` (read/write/poll/ioctl/release go through the
//! generic fd paths) that also implements [`SocketOps`], reached with
//! `File::as_socket`. The syscall layer owns everything that is the same
//! for every family: copying addresses, iovecs and option values to and
//! from user memory, blocking (`wait_ready` with `SO_RCVTIMEO` /
//! `SO_SNDTIMEO`, `MSG_DONTWAIT`, `O_NONBLOCK`, EINTR/ERESTARTSYS),
//! `MSG_WAITALL` looping and `SIGPIPE`. An implementation only provides
//! non-blocking primitives that return `EAGAIN` when they would block.
//!
//! Today the only implementation is `net::inet::InetSocket` (AF_INET /
//! AF_INET6, TCP and UDP over smoltcp). AF_UNIX plugs in as a second
//! implementor: decode its address from [`SockAddr::Unix`], implement these
//! methods, and add it to `syscall::net::socket`/`socketpair`. Ancillary
//! data (SCM_RIGHTS) reaches [`SocketOps::send`] as the raw `msg_control`
//! bytes and leaves [`SocketOps::recv`] through [`RecvInfo::control`].

use alloc::sync::Arc;
use alloc::vec::Vec;

use patina_abi::errno::Errno;
use patina_abi::net::MsgFlags;
pub use patina_inet::SockAddr;

use crate::fs::file::File;

/// What a successful non-blocking receive produced.
pub struct RecvInfo {
    /// Bytes copied into the caller's buffer.
    pub len: usize,
    /// Size of the datagram (≥ `len`; `MSG_TRUNC` reporting). Equal to
    /// `len` for streams.
    pub full: usize,
    /// Source address (datagram sockets; `None` for streams).
    pub from: Option<SockAddr>,
    /// Ancillary data to hand back (`msg_control`), already encoded as
    /// cmsg records. Empty for inet sockets.
    pub control: Vec<u8>,
}

/// Socket operations. All are non-blocking: `Err(EAGAIN)` means "would
/// block" and the syscall layer waits for readiness (`IN` for receive and
/// accept, `OUT` for send and connect).
pub trait SocketOps: Send + Sync {
    /// `SO_DOMAIN`.
    fn domain(&self) -> u16;
    /// `SO_TYPE` (`SOCK_STREAM`, `SOCK_DGRAM`).
    fn sock_type(&self) -> u32;
    /// `SO_PROTOCOL`.
    fn protocol(&self) -> i32;
    /// Is this a byte stream (partial sends continue, `MSG_WAITALL` loops)?
    fn is_stream(&self) -> bool {
        self.sock_type() == patina_abi::net::SOCK_STREAM
    }

    fn bind(&self, addr: &SockAddr) -> Result<(), Errno>;
    fn listen(&self, backlog: i32) -> Result<(), Errno>;
    /// Take one pending connection: the new socket and its peer address.
    fn accept(&self) -> Result<(Arc<dyn File>, SockAddr), Errno>;
    /// Start connecting. Stream sockets return `Err(EINPROGRESS)` when the
    /// connection was started and is not complete yet (the caller waits for
    /// `OUT` and asks [`SocketOps::connect_result`] unless non-blocking).
    fn connect(&self, addr: &SockAddr) -> Result<(), Errno>;
    /// The outcome of a started connect: `None` while still in progress.
    fn connect_result(&self) -> Option<Result<(), Errno>> {
        Some(Ok(()))
    }
    /// Queue `data` (one datagram, or a prefix of a stream). `to`: the
    /// destination of `sendto`/`sendmsg`; `control`: raw `msg_control`.
    fn send(
        &self,
        data: &[u8],
        to: Option<&SockAddr>,
        flags: MsgFlags,
        control: &[u8],
    ) -> Result<usize, Errno>;
    /// Receive into `buf` (`MSG_PEEK`, `MSG_TRUNC` honoured here).
    fn recv(&self, buf: &mut [u8], flags: MsgFlags) -> Result<RecvInfo, Errno>;
    fn shutdown(&self, how: i32) -> Result<(), Errno>;
    fn local_addr(&self) -> Result<SockAddr, Errno>;
    fn peer_addr(&self) -> Result<SockAddr, Errno>;
    /// `val` holds the caller's `optlen` bytes.
    fn setsockopt(&self, level: i32, name: i32, val: &[u8]) -> Result<(), Errno>;
    /// Write the option into `out` (sized for the largest option) and
    /// return its length; the caller truncates to the user's `optlen`.
    fn getsockopt(&self, level: i32, name: i32, out: &mut [u8]) -> Result<usize, Errno>;
    /// `SO_RCVTIMEO` / `SO_SNDTIMEO` in ns (`None`: block indefinitely).
    fn timeouts(&self) -> (Option<u64>, Option<u64>);
}

/// Decode a `struct timeval` option value (`SO_RCVTIMEO`/`SO_SNDTIMEO`,
/// old and new numbers have the same layout on 64-bit) into ns; zero is
/// "no timeout".
pub fn timeval_opt(val: &[u8]) -> Result<Option<u64>, Errno> {
    use patina_abi::errno::{EDOM, EINVAL};
    if val.len() < 16 {
        return Err(Errno(EINVAL));
    }
    let sec = i64::from_ne_bytes(val[..8].try_into().unwrap());
    let usec = i64::from_ne_bytes(val[8..16].try_into().unwrap());
    if !(0..1_000_000).contains(&usec) {
        return Err(Errno(EDOM));
    }
    if sec < 0 || (sec == 0 && usec == 0) {
        // Linux: a negative timeout means "no timeout" (with a warning).
        return Ok(None);
    }
    Ok(Some(
        (sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(usec as u64 * 1000),
    ))
}

/// Encode a timeout as `struct timeval`.
pub fn timeval_bytes(t: Option<u64>, out: &mut [u8]) -> usize {
    let ns = t.unwrap_or(0);
    out[..8].copy_from_slice(&((ns / 1_000_000_000) as i64).to_ne_bytes());
    out[8..16].copy_from_slice(&((ns % 1_000_000_000 / 1000) as i64).to_ne_bytes());
    16
}

/// Read an `int` option value.
pub fn int_opt(val: &[u8]) -> Result<i32, Errno> {
    match val.get(..4) {
        Some(b) => Ok(i32::from_ne_bytes(b.try_into().unwrap())),
        None => Err(Errno(patina_abi::errno::EINVAL)),
    }
}

/// Write an `int` option value.
pub fn int_bytes(v: i32, out: &mut [u8]) -> usize {
    out[..4].copy_from_slice(&v.to_ne_bytes());
    4
}
