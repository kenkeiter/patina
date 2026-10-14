//! AF_VSOCK stream sockets for the program (design §7 "Optional —
//! AF_VSOCK", decision D76).
//!
//! The protocol lives in `patina_inet::vsock::Core` (host-tested): every
//! socket, the bound/connected tables, credit, timers and the loopback
//! transport. This module wires it up:
//!
//! * one `Core` behind one IRQ-safe lock ([`G`]), created at boot when the
//!   virtio-vsock device exists (guest CID from its config space, device
//!   payload = one 4 KiB tx slot minus the header, the control channel's
//!   port 4242 reserved so programs cannot bind it);
//! * `vsockd`, a kernel thread woken by the device interrupt
//!   (`drivers::virtio::vsock::IRQ`): it takes received packets from the
//!   driver (which keeps port 4242 for the control channel), feeds them to
//!   the core, runs its timers (connect timeout, orphaned closes) and
//!   retries queued transmits;
//! * after every core operation ([`with`]): loopback packets are fed
//!   straight back in, device packets go to the tx ring in order (a FIFO
//!   holds the ones that found no free slot until a tx completion wakes
//!   `vsockd`), and the poll hooks of every socket whose readiness may
//!   have changed are fired after the lock is released (fs/file.rs order);
//! * [`VsockSocket`]: the `fs::File` + `SocketOps` the socket syscalls use
//!   (non-blocking primitives; `syscall::net` does the blocking);
//! * `/dev/vsock` ([`VsockDev`], misc 10:121) answering
//!   `IOCTL_VM_SOCKETS_GET_LOCAL_CID`, as `mdlayher/vsock` expects.
//!
//! Without the device `socket(AF_VSOCK, ...)` is `EAFNOSUPPORT` and opening
//! `/dev/vsock` `ENODEV` (Linux without the vsock modules).

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use patina_abi::current::Stat;
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::*;
use patina_abi::fs::{S_IFCHR, S_IFSOCK};
use patina_abi::misc::FIONREAD;
use patina_abi::net::*;
use patina_inet::SockAddr;
use patina_inet::vsock::{Addr, Core, HDR_LEN, Hdr, Packet, SockId, Via};

use super::socket::{self as so, RecvInfo, SocketOps};
use crate::drivers::virtio::vsock as dev;
use crate::fs::file::{File, FileFlags, Notify, PollHook, PollList, alloc_ino, anon_stat};
use crate::fs::vfs::DevNode;
use crate::sync::SpinLock;

struct Glue {
    core: Core,
    /// Poll hooks per socket (sockets with an fd).
    hooks: BTreeMap<SockId, PollList>,
    /// Device packets waiting for a tx slot, in order.
    txq: VecDeque<Packet>,
}

static G: SpinLock<Option<Glue>> = SpinLock::new(None);
static PRESENT: AtomicBool = AtomicBool::new(false);
static SEEDED: AtomicBool = AtomicBool::new(false);
/// When `vsockd` next wakes on its own (monotonic ns).
static SLEEP_UNTIL: AtomicU64 = AtomicU64::new(0);

/// `vsockd`'s longest sleep without an interrupt (a safety net).
const IDLE_NS: u64 = 1_000_000_000;
/// Bound on loopback rounds per operation.
const MAX_LOOP_ROUNDS: usize = 4096;

/// Set up the socket layer if the vsock device exists (after
/// `drivers::init`).
pub fn init() {
    let Some(cid) = dev::guest_cid() else {
        return;
    };
    // Seeded for real at the first socket (the RNG is seeded later, by
    // the loader).
    let mut core = Core::new(0);
    core.set_device(Some(cid));
    core.set_dev_payload(dev::TX_PAYLOAD);
    core.reserve(patina_ctlproto::VSOCK_PORT);
    *G.lock() = Some(Glue {
        core,
        hooks: BTreeMap::new(),
        txq: VecDeque::new(),
    });
    PRESENT.store(true, Ordering::Release);
    crate::drivers::spawn(vsockd);
}

/// Whether AF_VSOCK sockets are available.
pub fn present() -> bool {
    PRESENT.load(Ordering::Acquire)
}

impl Glue {
    /// Move the core's output along and collect readiness notifications.
    fn pump(&mut self) -> Notify {
        for _ in 0..MAX_LOOP_ROUNDS {
            if !self.core.has_out() {
                break;
            }
            for p in self.core.take_out() {
                match p.via {
                    Via::Loopback => self.core.input(p),
                    Via::Device => self.txq.push_back(p),
                }
            }
        }
        while let Some(p) = self.txq.front() {
            if !dev::try_send(&p.hdr.encode(), &p.payload) {
                break;
            }
            self.txq.pop_front();
        }
        // Wake vsockd if it must retry transmits (a tx completion would
        // too) or a timer is now due before it planned to wake.
        let early = self
            .core
            .next_deadline()
            .is_some_and(|d| d < SLEEP_UNTIL.load(Ordering::Relaxed));
        if !self.txq.is_empty() || early {
            dev::IRQ.signal();
        }
        let mut n = Notify::NONE;
        for id in self.core.take_dirty() {
            if let Some(l) = self.hooks.get_mut(&id) {
                n = n.and(l.collect(EpollFlags::empty()));
            }
        }
        n
    }
}

/// Run `f` on the core, then pump and fire notifications.
fn with<R>(f: impl FnOnce(&mut Core) -> R) -> R {
    let (r, n) = {
        let mut g = G.lock();
        let g = g
            .as_mut()
            .expect("vsock sockets exist only with the device");
        let r = f(&mut g.core);
        (r, g.pump())
    };
    n.fire();
    r
}

fn now() -> u64 {
    crate::arch::timer::now_ns()
}

/// The receiving thread.
fn vsockd() {
    let mut truncated = 0u64;
    let mut drops = 0u64;
    loop {
        let mut pkts: Vec<Packet> = Vec::new();
        let reset = dev::poll(&mut |b| {
            if let Some(mut hdr) = Hdr::decode(b) {
                let payload = b[HDR_LEN..].to_vec();
                if hdr.len as usize != payload.len() {
                    // More than one rx buffer's worth (the device must
                    // split; none seen on VZ).
                    truncated += 1;
                    crate::kprintln!(
                        "[patina] vsock: packet len {} truncated to {} ({} so far)",
                        hdr.len,
                        payload.len(),
                        truncated
                    );
                }
                hdr.len = payload.len() as u32;
                pkts.push(Packet {
                    via: Via::Device,
                    hdr,
                    payload,
                });
            }
        });
        let t = now();
        let (next, d) = with(|c| {
            if let Some(cid) = reset {
                crate::kprintln!("[patina] vsock: transport reset, guest cid {}", cid);
                c.transport_reset(Some(cid));
            }
            for p in pkts {
                c.input(p);
            }
            c.tick(t);
            (c.next_deadline(), c.rx_drops())
        });
        if d != drops {
            drops = d;
            crate::kprintln!(
                "[patina] vsock: peer exceeded our credit, {} packet(s) dropped",
                d
            );
        }
        let until = next.unwrap_or(u64::MAX).min(t.saturating_add(IDLE_NS));
        SLEEP_UNTIL.store(until, Ordering::Relaxed);
        dev::IRQ.wait(Some(until));
    }
}

// ---------------------------------------------------------------------------
// Sockets.

/// An AF_VSOCK stream socket: an id in the core.
pub struct VsockSocket {
    id: SockId,
    ino: u64,
    released: AtomicBool,
    /// `SO_RCVTIMEO`, `SO_SNDTIMEO`.
    timeo: SpinLock<(Option<u64>, Option<u64>)>,
}

impl VsockSocket {
    fn new(id: SockId) -> Arc<VsockSocket> {
        Arc::new(VsockSocket {
            id,
            ino: alloc_ino(),
            released: AtomicBool::new(false),
            timeo: SpinLock::new((None, None)),
        })
    }
}

/// `socket(AF_VSOCK, ty, protocol)` (`ty` without the flag bits).
/// Linux `vsock_create`: a protocol other than 0 / `PF_VSOCK` is
/// `EPROTONOSUPPORT`; `SOCK_DGRAM` needs a datagram transport, which
/// virtio has not (`vsock_assign_transport`: `ENODEV`); `SOCK_SEQPACKET`
/// needs `VIRTIO_VSOCK_F_SEQPACKET`, not negotiated (`ESOCKTNOSUPPORT`,
/// which Linux reports at connect/listen time); other types
/// `ESOCKTNOSUPPORT`.
pub fn create(ty: u32, protocol: i32) -> Result<Arc<dyn File>, Errno> {
    if !present() {
        return Err(Errno(EAFNOSUPPORT));
    }
    if protocol != 0 && protocol != AF_VSOCK as i32 {
        return Err(Errno(EPROTONOSUPPORT));
    }
    match ty {
        SOCK_STREAM => {}
        SOCK_DGRAM => return Err(Errno(ENODEV)),
        _ => return Err(Errno(ESOCKTNOSUPPORT)),
    }
    let seed = (!SEEDED.swap(true, Ordering::Relaxed)).then(|| {
        let mut b = [0u8; 8];
        crate::random::fill(&mut b);
        u64::from_ne_bytes(b)
    });
    let id = with(|c| {
        if let Some(s) = seed {
            c.set_seed(s);
        }
        c.create()
    });
    Ok(VsockSocket::new(id))
}

fn vm(a: Addr) -> SockAddr {
    SockAddr::Vsock {
        cid: a.cid,
        port: a.port,
        flags: 0,
    }
}

fn addr_of(a: &SockAddr) -> Result<Addr, Errno> {
    match *a {
        SockAddr::Vsock { cid, port, .. } => Ok(Addr::new(cid, port)),
        // `vsock_addr_cast` failures are EINVAL for bind and connect.
        _ => Err(Errno(EINVAL)),
    }
}

/// A `struct timeval` (old and new layouts agree on 64-bit) as ns;
/// Linux `vsock_connectible_setsockopt`: negative or out-of-range parts
/// are `ERANGE`, zero means the default.
fn timeval_ns(val: &[u8]) -> Result<u64, Errno> {
    if val.len() < 16 {
        return Err(Errno(EINVAL));
    }
    let sec = i64::from_ne_bytes(val[..8].try_into().unwrap());
    let usec = i64::from_ne_bytes(val[8..16].try_into().unwrap());
    if sec < 0 || !(0..1_000_000).contains(&usec) {
        return Err(Errno(ERANGE));
    }
    Ok((sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(usec as u64 * 1000))
}

impl SocketOps for VsockSocket {
    fn domain(&self) -> u16 {
        AF_VSOCK
    }

    fn sock_type(&self) -> u32 {
        SOCK_STREAM
    }

    fn protocol(&self) -> i32 {
        0
    }

    fn bind(&self, addr: &SockAddr) -> Result<(), Errno> {
        let a = addr_of(addr)?;
        with(|c| c.bind(self.id, a))
    }

    fn listen(&self, backlog: i32) -> Result<(), Errno> {
        with(|c| c.listen(self.id, backlog))
    }

    fn accept(&self) -> Result<(Arc<dyn File>, SockAddr), Errno> {
        let (id, peer) = with(|c| c.accept(self.id))?;
        Ok((VsockSocket::new(id), vm(peer)))
    }

    fn connect(&self, addr: &SockAddr) -> Result<(), Errno> {
        let a = addr_of(addr)?;
        let t = now();
        with(|c| c.connect(self.id, a, t))
    }

    fn connect_result(&self) -> Option<Result<(), Errno>> {
        with(|c| c.connect_result(self.id))
    }

    fn send(
        &self,
        data: &[u8],
        _to: Option<&SockAddr>,
        flags: MsgFlags,
        _control: &[u8],
    ) -> Result<usize, Errno> {
        if flags.contains(MsgFlags::OOB) {
            return Err(Errno(EOPNOTSUPP));
        }
        with(|c| c.send(self.id, data))
    }

    fn recv(&self, buf: &mut [u8], flags: MsgFlags) -> Result<RecvInfo, Errno> {
        if flags.contains(MsgFlags::OOB) {
            return Err(Errno(EOPNOTSUPP));
        }
        let peek = flags.contains(MsgFlags::PEEK);
        let n = with(|c| c.recv(self.id, buf, peek))?;
        Ok(RecvInfo {
            len: n,
            full: n,
            from: None,
            control: Vec::new(),
        })
    }

    fn shutdown(&self, how: i32) -> Result<(), Errno> {
        with(|c| c.shutdown(self.id, how))
    }

    fn local_addr(&self) -> Result<SockAddr, Errno> {
        with(|c| c.local_addr(self.id)).map(vm)
    }

    fn peer_addr(&self) -> Result<SockAddr, Errno> {
        with(|c| c.peer_addr(self.id)).map(vm)
    }

    fn setsockopt(&self, level: i32, name: i32, val: &[u8]) -> Result<(), Errno> {
        let u64_opt = |val: &[u8]| -> Result<u64, Errno> {
            match val.get(..8) {
                Some(b) => Ok(u64::from_ne_bytes(b.try_into().unwrap())),
                None => Err(Errno(EINVAL)),
            }
        };
        match (level, name) {
            (SOL_SOCKET, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) => {
                self.timeo.lock().0 = so::timeval_opt(val)?;
            }
            (SOL_SOCKET, SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW) => {
                self.timeo.lock().1 = so::timeval_opt(val)?;
            }
            // Generic options Linux's `sock_setsockopt` accepts that mean
            // nothing to vsock here.
            (
                SOL_SOCKET,
                SO_REUSEADDR | SO_REUSEPORT | SO_KEEPALIVE | SO_RCVBUF | SO_SNDBUF | SO_RCVBUFFORCE
                | SO_SNDBUFFORCE | SO_PRIORITY | SO_RCVLOWAT | SO_OOBINLINE,
            ) => {
                so::int_opt(val)?;
            }
            (SOL_SOCKET, SO_LINGER) => {
                if val.len() < 8 {
                    return Err(Errno(EINVAL));
                }
            }
            (l, SO_VM_SOCKETS_BUFFER_SIZE..=SO_VM_SOCKETS_BUFFER_MAX_SIZE)
                if l == AF_VSOCK as i32 =>
            {
                let v = u64_opt(val)?;
                with(|c| c.set_buffer(self.id, name as u32, v))?;
            }
            (l, SO_VM_SOCKETS_CONNECT_TIMEOUT_OLD | SO_VM_SOCKETS_CONNECT_TIMEOUT_NEW)
                if l == AF_VSOCK as i32 =>
            {
                let ns = timeval_ns(val)?;
                with(|c| c.set_connect_timeout(self.id, ns))?;
            }
            _ => return Err(Errno(ENOPROTOOPT)),
        }
        Ok(())
    }

    fn getsockopt(&self, level: i32, name: i32, out: &mut [u8]) -> Result<usize, Errno> {
        let int = |v: i32, out: &mut [u8]| Ok(so::int_bytes(v, out));
        match (level, name) {
            (SOL_SOCKET, SO_TYPE) => int(SOCK_STREAM as i32, out),
            (SOL_SOCKET, SO_DOMAIN) => int(AF_VSOCK as i32, out),
            (SOL_SOCKET, SO_PROTOCOL) => int(0, out),
            (SOL_SOCKET, SO_ERROR) => int(with(|c| c.take_error(self.id)), out),
            (SOL_SOCKET, SO_ACCEPTCONN) => int(with(|c| c.is_listening(self.id)) as i32, out),
            (SOL_SOCKET, SO_RCVBUF | SO_SNDBUF) => {
                let b = with(|c| c.buffer(self.id, 0))?;
                int(b.min(i32::MAX as u64) as i32, out)
            }
            (SOL_SOCKET, SO_REUSEADDR | SO_REUSEPORT | SO_KEEPALIVE | SO_OOBINLINE) => int(0, out),
            (SOL_SOCKET, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) => {
                Ok(so::timeval_bytes(self.timeo.lock().0, out))
            }
            (SOL_SOCKET, SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW) => {
                Ok(so::timeval_bytes(self.timeo.lock().1, out))
            }
            (l, SO_VM_SOCKETS_BUFFER_SIZE..=SO_VM_SOCKETS_BUFFER_MAX_SIZE)
                if l == AF_VSOCK as i32 =>
            {
                let v = with(|c| c.buffer(self.id, name as u32))?;
                out[..8].copy_from_slice(&v.to_ne_bytes());
                Ok(8)
            }
            (l, SO_VM_SOCKETS_CONNECT_TIMEOUT_OLD | SO_VM_SOCKETS_CONNECT_TIMEOUT_NEW)
                if l == AF_VSOCK as i32 =>
            {
                let ns = with(|c| c.connect_timeout(self.id))?;
                Ok(so::timeval_bytes(Some(ns), out))
            }
            _ => Err(Errno(ENOPROTOOPT)),
        }
    }

    fn timeouts(&self) -> (Option<u64>, Option<u64>) {
        *self.timeo.lock()
    }
}

impl File for VsockSocket {
    fn kind(&self) -> &'static str {
        "socket"
    }

    fn read(&self, buf: &mut [u8], _flags: FileFlags) -> Result<usize, Errno> {
        self.recv(buf, MsgFlags::empty()).map(|r| r.len)
    }

    fn write(&self, buf: &[u8], _flags: FileFlags) -> Result<usize, Errno> {
        self.send(buf, None, MsgFlags::empty(), &[])
    }

    fn poll(&self, hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        let mut g = G.lock();
        let Some(g) = g.as_mut() else {
            return EpollFlags::ERR | EpollFlags::HUP;
        };
        if let Some(h) = hook {
            g.hooks.entry(self.id).or_default().add(h);
        }
        g.core.poll(self.id)
    }

    fn ioctl(&self, cmd: u32, arg: u64) -> Result<isize, Errno> {
        if cmd != FIONREAD {
            return Err(Errno(ENOTTY));
        }
        let n = with(|c| {
            if c.is_listening(self.id) {
                Err(Errno(EINVAL))
            } else {
                Ok(c.rx_bytes(self.id) as i32)
            }
        })?;
        crate::syscall::with_space(|s| s.write_val(arg as usize, &n))?;
        Ok(0)
    }

    fn stat(&self) -> Result<Stat, Errno> {
        Ok(anon_stat(S_IFSOCK | 0o777, self.ino))
    }

    fn proc_name(&self) -> Vec<u8> {
        format!("socket:[{}]", self.ino).into_bytes()
    }

    fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let t = now();
        let n = {
            let mut g = G.lock();
            let Some(g) = g.as_mut() else {
                return;
            };
            g.core.release(self.id, t);
            g.hooks.remove(&self.id);
            g.pump()
        };
        n.fire();
    }

    fn as_socket(&self) -> Option<&dyn SocketOps> {
        Some(self)
    }
}

impl Drop for VsockSocket {
    fn drop(&mut self) {
        // A socket dropped without ever being installed as an fd.
        self.release();
    }
}

// ---------------------------------------------------------------------------
// /dev/vsock.

/// `/dev/vsock`'s device number (Linux registers it as a misc device;
/// 10:121 is what its dynamic minor is on common kernels).
pub const DEV_VSOCK: u64 = patina_abi::fs::makedev(10, 121);

/// The `/dev/vsock` driver (`vsock_device_ops`).
pub struct VsockDev;

impl DevNode for VsockDev {
    fn open(&self, _ino: patina_vfs::Ino, _flags: u32) -> Result<Arc<dyn File>, Errno> {
        if !present() {
            return Err(Errno(ENODEV));
        }
        Ok(Arc::new(VsockDevFile))
    }
}

struct VsockDevFile;

impl File for VsockDevFile {
    fn kind(&self) -> &'static str {
        "chr"
    }

    fn poll(&self, _hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        EpollFlags::IN | EpollFlags::RDNORM | EpollFlags::OUT | EpollFlags::WRNORM
    }

    /// `vsock_dev_do_ioctl`: only `IOCTL_VM_SOCKETS_GET_LOCAL_CID`.
    fn ioctl(&self, cmd: u32, arg: u64) -> Result<isize, Errno> {
        if cmd != IOCTL_VM_SOCKETS_GET_LOCAL_CID {
            return Err(Errno(ENOTTY));
        }
        let cid = G
            .lock()
            .as_ref()
            .map_or(VMADDR_CID_ANY, |g| g.core.local_cid());
        crate::syscall::with_space(|s| s.write_val(arg as usize, &cid))?;
        Ok(0)
    }

    fn stat(&self) -> Result<Stat, Errno> {
        Ok(anon_stat(S_IFCHR | 0o666, alloc_ino()))
    }
}
