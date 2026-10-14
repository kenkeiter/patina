//! virtio-vsock (virtio 1.2 §5.10), stream sockets (decisions D62, D76).
//! Verified on Apple VZ (D83-D85 for the control channel, D76 for
//! AF_VSOCK); written against the spec and Linux's
//! `net/vmw_vsock/virtio_transport{,_common}.c`.
//!
//! The driver multiplexes two users by destination port:
//!
//! * The control channel listens on one kernel-owned port ([`listen`],
//!   4242) with the connection logic below. One connection at a time: a new
//!   `REQUEST` replaces the current connection (the old peer gets `RST`).
//!   `RESPONSE`s (we never connect from that port) are answered with
//!   `RST`. A peer `SHUTDOWN` closes the connection and is answered with
//!   `RST`; a peer `RST` just closes it.
//! * Every other packet is handed raw to the AF_VSOCK socket layer
//!   (`net::vsock`, the `patina_inet::vsock::Core` state machine), which
//!   answers packets nobody takes with `RST` and transmits its own packets
//!   through [`try_send`].
//!
//! Receiving is done by one kernel thread (`net::vsock`'s `vsockd`, woken
//! by [`IRQ`]) through [`poll`]: control-channel bytes are buffered (64 KiB)
//! and [`EVENT`] is signalled for the control thread, which drains them
//! with [`service`]. An event-queue `TRANSPORT_RESET` (live migration)
//! drops the control connection, re-reads the guest CID and is reported to
//! the caller of [`poll`].
//!
//! Control-channel credit (§5.10.6.3): we advertise `buf_alloc` =
//! [`BUF_ALLOC`] and count consumed bytes in `fwd_cnt` as they arrive,
//! sending `CREDIT_UPDATE` after half of it was consumed since the last
//! update and on `CREDIT_REQUEST`. Sends respect the peer's credit
//! (`peer_buf_alloc - (tx_cnt - peer_fwd_cnt)`); without credit the data
//! is dropped and a `CREDIT_REQUEST` sent (control-channel events are small
//! and the host reads them promptly).
//!
//! Queues: rx (0) with [`RX_BUFS`] posted 4 KiB buffers (header + payload
//! in one device-writable descriptor), tx (1) with [`TX_SLOTS`] 4 KiB
//! slots (header + payload in one descriptor; completions interrupt so
//! queued socket packets go out as slots free), event (2) with 4-byte
//! buffers. Control-channel transmit is bounded-synchronous like the
//! console's: a send waits for a free slot, and [`flush`] waits until the
//! device has taken everything (used before poweroff for the `exit` event).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use patina_inet::vsock::{
    EVENT_TRANSPORT_RESET, F_STREAM, HDR_LEN as HDR, Hdr, OP_CREDIT_REQUEST, OP_CREDIT_UPDATE,
    OP_REQUEST, OP_RESPONSE, OP_RST, OP_RW, OP_SHUTDOWN, TYPE_STREAM,
};

use super::queue::{Buf, DmaBuf, VirtQueue};
use super::{DEVICE_VSOCK, Transport, attach_irq, driver_ok, negotiate, poll_until};
use crate::drivers::Event;
use crate::sync::{OnceCell, SpinLock, SpinLockGuard};

const PKT: usize = 4096;
const MAX_PAYLOAD: usize = PKT - HDR;
/// Largest payload of one transmitted packet (AF_VSOCK sockets split
/// their data to this).
pub const TX_PAYLOAD: usize = MAX_PAYLOAD;
const RX_BUFS: usize = 32;
/// At most 32 (the `free` bitmap is a u32; `(1 << 32) - 1` would overflow).
const TX_SLOTS: usize = 16;
const EV_BUFS: usize = 4;
/// Receive space advertised to the peer.
pub const BUF_ALLOC: u32 = 64 * 1024;
const HOST_CID: u64 = 2;

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
/// Signalled by the device interrupt (rx, tx completions, events); the
/// receiving thread (`net::vsock`) waits on it. Anyone may signal it to
/// make that thread run (queued transmits, a new timer).
pub static IRQ: Event = Event::new();
/// Signalled when control-channel bytes were buffered or the control
/// connection changed; the control channel thread waits on it.
pub static EVENT: Event = Event::new();
/// Bumped on every accepted / closed connection.
pub static CONN_GEN: AtomicU32 = AtomicU32::new(0);
/// Control-channel bytes received and not yet taken by [`service`].
static CTL_RX: SpinLock<VecDeque<u8>> = SpinLock::new(VecDeque::new());
/// Bound of [`CTL_RX`] (the credit the control connection advertises).
const CTL_RX_CAP: usize = BUF_ALLOC as usize;

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
    IRQ.signal();
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
        let txq = VirtQueue::new(&*t, 1, TX_SLOTS as u16)?;
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
    crate::kprintln!("[patina] virtio-vsock: guest cid {}", cid);
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

/// A new control connection, or the old one closed: forget its buffered
/// bytes and tell the control thread.
fn ctl_conn_changed() {
    CTL_RX.lock().clear();
    CONN_GEN.fetch_add(1, Ordering::SeqCst);
    EVENT.signal();
}

/// Buffer control-connection bytes for [`service`] (dropped beyond
/// [`CTL_RX_CAP`]: a host exceeding the advertised credit).
fn ctl_data(payload: &[u8]) {
    {
        let mut b = CTL_RX.lock();
        let n = payload.len().min(CTL_RX_CAP.saturating_sub(b.len()));
        b.extend(&payload[..n]);
    }
    EVENT.signal();
}

/// A packet for the control channel's port.
fn handle(v: &Vsock, h: &Hdr, payload: &[u8]) {
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
        ctl_conn_changed();
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
            ctl_data(payload);
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
            ctl_conn_changed();
        }
        OP_RST => {
            *g = None;
            drop(g);
            ctl_conn_changed();
        }
        _ => {}
    }
}

/// Process the device's events and received packets (the one receiving
/// thread, `net::vsock`). Packets for the control channel's port are
/// handled here (their bytes buffered for [`service`]); every other packet
/// (header and payload, trimmed to the header's `len`) goes to `sock`.
/// Returns the new guest CID after a `TRANSPORT_RESET` event.
pub fn poll(sock: &mut dyn FnMut(&[u8])) -> Option<u32> {
    let v = DEV.get()?;
    // Events first: a transport reset invalidates the connections.
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
    let mut new_cid = None;
    if reset {
        let cid = v.t.config_read64(0) & 0xffff_ffff;
        v.cid.store(cid, Ordering::Relaxed);
        new_cid = Some(cid as u32);
        if v.conn.lock().take().is_some() {
            ctl_conn_changed();
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
        let Some(h) = Hdr::decode(&pkt[..n]) else {
            continue;
        };
        let plen = (h.len as usize).min(n - HDR);
        let port = v.port.load(Ordering::Relaxed);
        if port != 0 && h.dst_port == port {
            handle(v, &h, &pkt[HDR..HDR + plen]);
        } else {
            sock(&pkt[..HDR + plen]);
        }
    }
    new_cid
}

/// Hand the control connection's buffered bytes to `on_data` (control
/// thread). [`EVENT`] says when there may be more.
pub fn service(on_data: &mut dyn FnMut(&[u8])) {
    if DEV.get().is_none() {
        return;
    }
    let mut chunk = [0u8; 1024];
    loop {
        let n = {
            let mut b = CTL_RX.lock();
            let n = b.len().min(chunk.len());
            for (d, s) in chunk[..n].iter_mut().zip(b.drain(..n)) {
                *d = s;
            }
            n
        };
        if n == 0 {
            return;
        }
        on_data(&chunk[..n]);
    }
}

/// The guest CID from the device's config space (`None`: no device).
pub fn guest_cid() -> Option<u32> {
    DEV.get().map(|v| v.cid.load(Ordering::Relaxed) as u32)
}

/// Queue one packet (`hdr.len` must equal `payload.len()`, at most
/// [`TX_PAYLOAD`]) if a transmit slot is free now; false otherwise (the
/// caller keeps it and retries after the next [`IRQ`]).
pub fn try_send(hdr: &[u8; HDR], payload: &[u8]) -> bool {
    let Some(v) = DEV.get() else {
        return false;
    };
    if payload.len() > MAX_PAYLOAD {
        return false;
    }
    let mut tx = lock(&v.tx);
    tx.reap();
    if tx.free == 0 {
        return false;
    }
    let slot = tx.free.trailing_zeros() as usize;
    tx.buf.write(slot * PKT, hdr);
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
