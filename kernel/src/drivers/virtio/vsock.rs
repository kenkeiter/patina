//! virtio-vsock (virtio 1.2 §5.10), stream sockets, listen-only
//! (decision D62). UNVERIFIED: QEMU on the macOS dev host has no
//! vhost-vsock, so this was written against the spec and Linux's
//! `net/vmw_vsock/virtio_transport{,_common}.c` and only compile-tested.
//!
//! The control channel listens on one port ([`listen`], 4242). One
//! connection at a time: a new `REQUEST` replaces the current connection
//! (the old peer gets `RST`). Requests for other ports, data for unknown
//! connections and `RESPONSE`s (we never connect) are answered with `RST`.
//! A peer `SHUTDOWN` closes the connection and is answered with `RST`; a
//! peer `RST` just closes it. An event-queue `TRANSPORT_RESET` (live
//! migration) drops the connection and re-reads the guest CID.
//!
//! Credit (§5.10.6.3): we advertise `buf_alloc` = [`BUF_ALLOC`] and count
//! consumed bytes in `fwd_cnt` (data is handed to the caller as it
//! arrives, so the receive space is never really occupied), sending
//! `CREDIT_UPDATE` after half of it was consumed since the last update and
//! on `CREDIT_REQUEST`. Sends respect the peer's credit
//! (`peer_buf_alloc - (tx_cnt - peer_fwd_cnt)`); without credit the data is dropped and a
//! `CREDIT_REQUEST` sent (control-channel events are small and the host
//! reads them promptly).
//!
//! Queues: rx (0) with [`RX_BUFS`] posted 4 KiB buffers (header + payload
//! in one device-writable descriptor), tx (1) with [`TX_SLOTS`] 4 KiB
//! slots (header + payload in one descriptor), event (2) with 4-byte
//! buffers. Transmit is bounded-synchronous like the console's: a send
//! waits for a free slot, and [`flush`] waits until the device has taken
//! everything (used before poweroff for the `exit` event).

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::queue::{Buf, DmaBuf, VirtQueue};
use super::{DEVICE_VSOCK, Transport, attach_irq, driver_ok, negotiate, poll_until};
use crate::drivers::Event;
use crate::sync::{OnceCell, SpinLock, SpinLockGuard};

const F_STREAM: u64 = 1 << 0;

const HDR: usize = 44;
const PKT: usize = 4096;
const MAX_PAYLOAD: usize = PKT - HDR;
const RX_BUFS: usize = 16;
const TX_SLOTS: usize = 8;
const EV_BUFS: usize = 4;
/// Receive space advertised to the peer.
pub const BUF_ALLOC: u32 = 64 * 1024;

const TYPE_STREAM: u16 = 1;
const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;
const EVENT_TRANSPORT_RESET: u32 = 0;
const HOST_CID: u64 = 2;

#[derive(Clone, Copy, Debug, Default)]
struct Hdr {
    src_cid: u64,
    dst_cid: u64,
    src_port: u32,
    dst_port: u32,
    len: u32,
    ty: u16,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}

impl Hdr {
    fn parse(b: &[u8]) -> Hdr {
        let u64_ = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or([0; 8]));
        let u32_ = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u16_ = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        Hdr {
            src_cid: u64_(0),
            dst_cid: u64_(8),
            src_port: u32_(16),
            dst_port: u32_(20),
            len: u32_(24),
            ty: u16_(28),
            op: u16_(30),
            flags: u32_(32),
            buf_alloc: u32_(36),
            fwd_cnt: u32_(40),
        }
    }

    fn encode(&self) -> [u8; HDR] {
        let mut b = [0u8; HDR];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.ty.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }
}

/// The one accepted connection.
#[derive(Clone, Copy, Debug)]
struct Conn {
    peer_cid: u64,
    peer_port: u32,
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    tx_cnt: u32,
    fwd_cnt: u32,
    last_fwd_sent: u32,
}

struct Rx {
    q: VirtQueue,
    buf: DmaBuf,
    slot_of: [u8; 256],
}

struct Tx {
    q: VirtQueue,
    buf: DmaBuf,
    /// Slot of each in-flight descriptor head (u8::MAX = none).
    slot_of: [u8; 256],
    free: u32,
}

impl Tx {
    fn reap(&mut self) {
        while let Some((h, _)) = self.q.pop_used() {
            let s = core::mem::replace(&mut self.slot_of[h as usize], u8::MAX);
            if s != u8::MAX {
                self.free |= 1 << s;
            }
        }
    }

    fn idle(&mut self) -> bool {
        self.reap();
        self.free == (1 << TX_SLOTS) - 1
    }
}

struct Ev {
    q: VirtQueue,
    buf: DmaBuf,
    slot_of: [u8; 256],
}

struct Vsock {
    t: Box<dyn Transport>,
    rx: SpinLock<Rx>,
    tx: SpinLock<Tx>,
    ev: SpinLock<Ev>,
    conn: SpinLock<Option<Conn>>,
    cid: AtomicU64,
    port: AtomicU32,
}

static DEV: OnceCell<Vsock> = OnceCell::new();
/// Signalled by the device interrupt; the control channel thread waits on it.
pub static EVENT: Event = Event::new();
/// Bumped on every accepted / closed connection.
pub static CONN_GEN: AtomicU32 = AtomicU32::new(0);

fn lock<T>(l: &SpinLock<T>) -> SpinLockGuard<'_, T> {
    if !crate::task::sched::exiting() {
        return l.lock();
    }
    for _ in 0..1_000_000 {
        if let Some(g) = l.try_lock() {
            return g;
        }
        core::hint::spin_loop();
    }
    // SAFETY: only on the no-return exit/panic path (see console.rs).
    unsafe { l.force_unlock() };
    l.lock()
}

fn irq() {
    if let Some(d) = DEV.get() {
        d.t.ack_interrupt();
    }
    EVENT.signal();
}

/// Claim and start the first virtio-vsock device, if any.
pub fn init() {
    let Some(dev) = super::take(DEVICE_VSOCK) else {
        return;
    };
    let t = dev.transport;
    if let Err(e) = negotiate(&*t, F_STREAM) {
        crate::kprintln!("[patina] virtio-vsock: {}", e);
        return;
    }
    let mk = || -> Option<(Rx, Tx, Ev)> {
        let mut rx = Rx {
            q: VirtQueue::new(&*t, 0, RX_BUFS as u16)?,
            buf: DmaBuf::new(RX_BUFS * PKT)?,
            slot_of: [0; 256],
        };
        let mut txq = VirtQueue::new(&*t, 1, TX_SLOTS as u16)?;
        txq.set_no_interrupt(true);
        let tx = Tx {
            q: txq,
            buf: DmaBuf::new(TX_SLOTS * PKT)?,
            slot_of: [u8::MAX; 256],
            free: (1 << TX_SLOTS) - 1,
        };
        let mut ev = Ev {
            q: VirtQueue::new(&*t, 2, EV_BUFS as u16)?,
            buf: DmaBuf::new(EV_BUFS * 4)?,
            slot_of: [0; 256],
        };
        for s in 0..(rx.q.size() as usize).min(RX_BUFS) {
            post_rx(&mut rx, s);
        }
        for s in 0..(ev.q.size() as usize).min(EV_BUFS) {
            post_ev(&mut ev, s);
        }
        Some((rx, tx, ev))
    };
    let Some((rx, tx, ev)) = mk() else {
        crate::kprintln!("[patina] virtio-vsock: queue setup failed");
        return;
    };
    driver_ok(&*t);
    rx.q.kick(&*t);
    ev.q.kick(&*t);
    let cid = t.config_read64(0) & 0xffff_ffff;
    let v = Vsock {
        t,
        rx: SpinLock::new(rx),
        tx: SpinLock::new(tx),
        ev: SpinLock::new(ev),
        conn: SpinLock::new(None),
        cid: AtomicU64::new(cid),
        port: AtomicU32::new(0),
    };
    if DEV.set(v).is_err() {
        return;
    }
    if let Some(line) = dev.irq {
        attach_irq(line, irq);
    }
    crate::kprintln!(
        "[patina] virtio-vsock: guest cid {} (UNVERIFIED driver)",
        cid
    );
}

fn post_rx(rx: &mut Rx, slot: usize) {
    let b = Buf {
        addr: rx.buf.pa(slot * PKT),
        len: PKT as u32,
        write: true,
    };
    if let Some(h) = rx.q.add(&[b]) {
        rx.slot_of[h as usize] = slot as u8;
    }
}

fn post_ev(ev: &mut Ev, slot: usize) {
    let b = Buf {
        addr: ev.buf.pa(slot * 4),
        len: 4,
        write: true,
    };
    if let Some(h) = ev.q.add(&[b]) {
        ev.slot_of[h as usize] = slot as u8;
    }
}

/// Whether a vsock device is driven.
pub fn present() -> bool {
    DEV.get().is_some()
}

/// Accept connections on `port` from now on.
pub fn listen(port: u32) {
    if let Some(v) = DEV.get() {
        v.port.store(port, Ordering::Relaxed);
    }
}

/// Whether a host connection is established.
pub fn connected() -> bool {
    DEV.get().is_some_and(|v| v.conn.lock().is_some())
}

/// Queue one packet; waits up to `timeout_ns` for a free tx slot.
fn send(v: &Vsock, h: &Hdr, payload: &[u8], timeout_ns: u64) -> bool {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    let mut tx = lock(&v.tx);
    if !poll_until(timeout_ns, || {
        tx.reap();
        tx.free != 0
    }) {
        return false;
    }
    let slot = tx.free.trailing_zeros() as usize;
    let mut h = *h;
    h.len = payload.len() as u32;
    tx.buf.write(slot * PKT, &h.encode());
    tx.buf.write(slot * PKT + HDR, payload);
    let b = Buf {
        addr: tx.buf.pa(slot * PKT),
        len: (HDR + payload.len()) as u32,
        write: false,
    };
    let Some(head) = tx.q.add(&[b]) else {
        return false;
    };
    tx.slot_of[head as usize] = slot as u8;
    tx.free &= !(1 << slot);
    tx.q.kick(&*v.t);
    true
}

fn reply_hdr(v: &Vsock, to_cid: u64, to_port: u32, from_port: u32, op: u16) -> Hdr {
    Hdr {
        src_cid: v.cid.load(Ordering::Relaxed),
        dst_cid: to_cid,
        src_port: from_port,
        dst_port: to_port,
        ty: TYPE_STREAM,
        op,
        buf_alloc: BUF_ALLOC,
        ..Hdr::default()
    }
}

fn rst(v: &Vsock, h: &Hdr) {
    if h.op == OP_RST {
        return; // never answer a reset
    }
    let r = reply_hdr(v, h.src_cid, h.src_port, h.dst_port, OP_RST);
    send(v, &r, &[], 10_000_000);
}

fn conn_hdr(v: &Vsock, c: &mut Conn, op: u16) -> Hdr {
    let mut h = reply_hdr(
        v,
        c.peer_cid,
        c.peer_port,
        v.port.load(Ordering::Relaxed),
        op,
    );
    h.fwd_cnt = c.fwd_cnt;
    c.last_fwd_sent = c.fwd_cnt;
    h
}

fn handle(v: &Vsock, h: &Hdr, payload: &[u8], on_data: &mut dyn FnMut(&[u8])) {
    let port = v.port.load(Ordering::Relaxed);
    let me = v.cid.load(Ordering::Relaxed);
    if h.ty != TYPE_STREAM || port == 0 || h.dst_port != port || (h.dst_cid != me && me != 0) {
        rst(v, h);
        return;
    }
    let mut g = v.conn.lock();
    let ours = g
        .as_ref()
        .is_some_and(|c| c.peer_cid == h.src_cid && c.peer_port == h.src_port);
    if h.op == OP_REQUEST {
        if let Some(mut old) = g.take() {
            let r = conn_hdr(v, &mut old, OP_RST);
            send(v, &r, &[], 10_000_000);
        }
        let mut c = Conn {
            peer_cid: h.src_cid,
            peer_port: h.src_port,
            peer_buf_alloc: h.buf_alloc,
            peer_fwd_cnt: h.fwd_cnt,
            tx_cnt: 0,
            fwd_cnt: 0,
            last_fwd_sent: 0,
        };
        let r = conn_hdr(v, &mut c, OP_RESPONSE);
        if send(v, &r, &[], 10_000_000) {
            *g = Some(c);
        }
        drop(g);
        CONN_GEN.fetch_add(1, Ordering::SeqCst);
        return;
    }
    if !ours {
        drop(g);
        rst(v, h);
        return;
    }
    let c = g.as_mut().expect("checked above");
    c.peer_buf_alloc = h.buf_alloc;
    c.peer_fwd_cnt = h.fwd_cnt;
    match h.op {
        OP_RW => {
            c.fwd_cnt = c.fwd_cnt.wrapping_add(payload.len() as u32);
            let stale = c.fwd_cnt.wrapping_sub(c.last_fwd_sent) >= BUF_ALLOC / 2;
            let upd = stale.then(|| conn_hdr(v, c, OP_CREDIT_UPDATE));
            drop(g);
            on_data(payload);
            if let Some(u) = upd {
                send(v, &u, &[], 10_000_000);
            }
        }
        OP_CREDIT_REQUEST => {
            let u = conn_hdr(v, c, OP_CREDIT_UPDATE);
            drop(g);
            send(v, &u, &[], 10_000_000);
        }
        OP_CREDIT_UPDATE => {}
        OP_SHUTDOWN | OP_RESPONSE => {
            let mut c = g.take().expect("checked above");
            drop(g);
            let r = conn_hdr(v, &mut c, OP_RST);
            send(v, &r, &[], 10_000_000);
            CONN_GEN.fetch_add(1, Ordering::SeqCst);
        }
        OP_RST => {
            *g = None;
            drop(g);
            CONN_GEN.fetch_add(1, Ordering::SeqCst);
        }
        _ => {}
    }
}

/// Process received packets and events (control-channel thread);
/// connection data goes to `on_data`.
pub fn service(on_data: &mut dyn FnMut(&[u8])) {
    let Some(v) = DEV.get() else {
        return;
    };
    // Events first: a transport reset invalidates the connection.
    let mut reset = false;
    {
        let mut ev = v.ev.lock();
        let mut popped = false;
        while let Some((h, _)) = ev.q.pop_used() {
            let slot = ev.slot_of[h as usize] as usize;
            let mut id = [0u8; 4];
            ev.buf.read(slot * 4, &mut id);
            if u32::from_le_bytes(id) == EVENT_TRANSPORT_RESET {
                reset = true;
            }
            post_ev(&mut ev, slot);
            popped = true;
        }
        if popped {
            ev.q.kick(&*v.t);
        }
    }
    if reset {
        v.cid
            .store(v.t.config_read64(0) & 0xffff_ffff, Ordering::Relaxed);
        if v.conn.lock().take().is_some() {
            CONN_GEN.fetch_add(1, Ordering::SeqCst);
        }
    }
    let mut pkt = [0u8; PKT];
    loop {
        let n = {
            let mut rx = v.rx.lock();
            let Some((h, len)) = rx.q.pop_used() else {
                break;
            };
            let slot = rx.slot_of[h as usize] as usize;
            let n = (len as usize).min(PKT);
            rx.buf.read(slot * PKT, &mut pkt[..n]);
            post_rx(&mut rx, slot);
            rx.q.kick(&*v.t);
            n
        };
        if n < HDR {
            continue;
        }
        let h = Hdr::parse(&pkt[..HDR]);
        let plen = (h.len as usize).min(n - HDR);
        handle(v, &h, &pkt[HDR..HDR + plen], on_data);
    }
}

/// Send `data` on the connection (split into packets). False if there is
/// no connection, no credit or no tx slot within `timeout_ns`.
pub fn write(data: &[u8], timeout_ns: u64) -> bool {
    let Some(v) = DEV.get() else {
        return false;
    };
    for chunk in data.chunks(MAX_PAYLOAD) {
        let mut g = lock(&v.conn);
        let Some(c) = g.as_mut() else {
            return false;
        };
        let in_flight = c.tx_cnt.wrapping_sub(c.peer_fwd_cnt);
        let credit = c.peer_buf_alloc.saturating_sub(in_flight) as usize;
        if credit < chunk.len() {
            let r = conn_hdr(v, c, OP_CREDIT_REQUEST);
            drop(g);
            send(v, &r, &[], timeout_ns);
            return false;
        }
        c.tx_cnt = c.tx_cnt.wrapping_add(chunk.len() as u32);
        let h = conn_hdr(v, c, OP_RW);
        drop(g);
        if !send(v, &h, chunk, timeout_ns) {
            return false;
        }
    }
    true
}

/// Wait up to `timeout_ns` until the device has consumed every queued
/// packet (before poweroff).
pub fn flush(timeout_ns: u64) -> bool {
    let Some(v) = DEV.get() else {
        return true;
    };
    let mut tx = lock(&v.tx);
    poll_until(timeout_ns, || tx.idle())
}

/// The host-side CID the control channel expects to talk to (2).
pub const fn host_cid() -> u64 {
    HOST_CID
}
