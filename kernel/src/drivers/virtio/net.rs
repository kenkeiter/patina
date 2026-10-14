//! virtio-net (virtio 1.2 §5.1), decision D69: one receive/transmit queue
//! pair, no offloads, behind the `net::device::NetDevice` seam.
//!
//! * Features: `VIRTIO_NET_F_MAC` and `VIRTIO_NET_F_STATUS` when offered;
//!   nothing else (no `MRG_RXBUF`, checksum/GSO offloads, control queue or
//!   multiqueue). The MAC comes from config space (or is generated,
//!   locally administered, when the device does not offer one).
//! * Header (`virtio_net_hdr`): 10 bytes on a legacy transport, 12 (with
//!   `num_buffers`) when `VIRTIO_F_VERSION_1` was negotiated. Transmit
//!   headers are all zero (no checksum offload, `GSO_NONE`); receive
//!   headers are skipped.
//! * Receive (queue 0): up to 128 posted 2 KiB device-writable buffers, one
//!   descriptor each (header + frame). [`NetDevice::recv`] pops one used
//!   buffer, copies the frame out, re-posts the buffer and kicks.
//! * Transmit (queue 1): up to 32 slots of 2 KiB (header + frame, one
//!   device-readable descriptor). [`NetDevice::send`] reclaims completed
//!   slots first, then stages the frame and kicks; `false` when no slot is
//!   free.
//! * Interrupt: the callback only reads+acknowledges the interrupt status
//!   and signals [`EVENT`] (D59); it never takes the driver lock, so a CPU
//!   holding that lock with IRQs masked cannot deadlock against it.
//!
//! State: the transport lives in a [`OnceCell`] (the IRQ callback reads it
//! without a lock); the rings live in one `SpinLock<Option<Net>>`. The
//! registered `NetDevice` is a handle onto that lock.

use alloc::boxed::Box;
use alloc::string::String;
use core::sync::atomic::{AtomicU32, Ordering};

use super::queue::{Buf, DmaBuf, VirtQueue};
use super::{DEVICE_NET, F_VERSION_1, Transport, attach_irq, driver_ok, negotiate};
use crate::drivers::Event;
use crate::net::device::{MAX_FRAME, NetDevice};
use crate::sync::{OnceCell, SpinLock};

/// VIRTIO_NET_F_MAC: config space holds the MAC.
const F_MAC: u64 = 1 << 5;
/// VIRTIO_NET_F_STATUS: config space holds the link status.
const F_STATUS: u64 = 1 << 16;
/// `status` bit VIRTIO_NET_S_LINK_UP.
const S_LINK_UP: u16 = 1;

/// Bytes per receive buffer / transmit slot.
const BUF: usize = 2048;
const RX_WANT: u16 = 128;
const TX_WANT: u16 = 32;

struct Net {
    /// `virtio_net_hdr` size: 10 (legacy) or 12 (VERSION_1).
    hdr: usize,
    mac: [u8; 6],
    rx: VirtQueue,
    rx_buf: DmaBuf,
    /// Receive slot of each descriptor head.
    rx_slot_of: [u8; 128],
    tx: VirtQueue,
    tx_buf: DmaBuf,
    /// Transmit slot of each in-flight descriptor head (u8::MAX = none).
    tx_slot_of: [u8; 128],
    /// Bit per free transmit slot.
    tx_free: u32,
    tx_slots: usize,
}

static DEV: OnceCell<Box<dyn Transport>> = OnceCell::new();
static NET: SpinLock<Option<Net>> = SpinLock::new(None);
/// Signalled by the interrupt (receive completions, freed transmit slots).
static EVENT: Event = Event::new();
/// Interrupt callbacks taken (for the self-test and diagnostics).
static IRQS: AtomicU32 = AtomicU32::new(0);

fn irq() {
    IRQS.fetch_add(1, Ordering::Relaxed);
    if let Some(t) = DEV.get() {
        t.ack_interrupt();
    }
    EVENT.signal();
}

fn transport() -> &'static dyn Transport {
    &**DEV.get().expect("virtio-net: no transport")
}

impl Net {
    fn post_rx(&mut self, slot: usize) -> bool {
        let b = Buf {
            addr: self.rx_buf.pa(slot * BUF),
            len: BUF as u32,
            write: true,
        };
        match self.rx.add(&[b]) {
            Some(h) => {
                self.rx_slot_of[h as usize] = slot as u8;
                true
            }
            None => false,
        }
    }

    fn reclaim_tx(&mut self) {
        while let Some((h, _)) = self.tx.pop_used() {
            let s = core::mem::replace(&mut self.tx_slot_of[h as usize], u8::MAX);
            if s != u8::MAX {
                self.tx_free |= 1 << s;
            }
        }
    }

    fn can_send(&mut self) -> bool {
        self.reclaim_tx();
        self.tx_free != 0 && self.tx.num_free() > 0
    }

    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        let mut reposted = false;
        let mut out = None;
        while let Some((h, len)) = self.rx.pop_used() {
            let slot = self.rx_slot_of[h as usize] as usize;
            let len = (len as usize).min(BUF);
            let ok = len > self.hdr && len - self.hdr <= buf.len();
            if ok {
                let n = len - self.hdr;
                self.rx_buf.read(slot * BUF + self.hdr, &mut buf[..n]);
                out = Some(n);
            }
            reposted |= self.post_rx(slot);
            if ok {
                break;
            }
        }
        if reposted {
            self.rx.kick(transport());
        }
        out
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() > BUF - self.hdr {
            return true; // cannot be sent at all: drop it, as a NIC would
        }
        if !self.can_send() {
            return false;
        }
        let slot = self.tx_free.trailing_zeros() as usize;
        let off = slot * BUF;
        self.tx_buf.write(off, &[0u8; 12][..self.hdr]);
        self.tx_buf.write(off + self.hdr, frame);
        let b = Buf {
            addr: self.tx_buf.pa(off),
            len: (self.hdr + frame.len()) as u32,
            write: false,
        };
        let Some(h) = self.tx.add(&[b]) else {
            return false;
        };
        self.tx_slot_of[h as usize] = slot as u8;
        self.tx_free &= !(1 << slot);
        self.tx.kick(transport());
        true
    }
}

/// The registered device: a handle onto [`NET`].
struct VirtioNet {
    mac: [u8; 6],
}

impl NetDevice for VirtioNet {
    fn name(&self) -> &'static str {
        "virtio-net"
    }

    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        NET.lock().as_mut()?.recv(buf)
    }

    fn can_send(&mut self) -> bool {
        NET.lock().as_mut().is_some_and(|n| n.can_send())
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        NET.lock().as_mut().is_some_and(|n| n.send(frame))
    }

    fn event(&self) -> &'static Event {
        &EVENT
    }
}

/// A locally administered unicast MAC (02:xx:xx:xx:xx:xx).
fn generated_mac() -> [u8; 6] {
    let r = crate::arch::cpu::hw_random().unwrap_or(0)
        ^ crate::arch::timer::now_ns().wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let b = r.to_le_bytes();
    [0x02, b[0], b[1], b[2], b[3], b[4]]
}

fn fmt_mac(m: &[u8]) -> String {
    alloc::format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0],
        m[1],
        m[2],
        m[3],
        m[4],
        m[5]
    )
}

/// Claim and start the first virtio-net device, if any, and register it
/// with `net::device`.
pub fn init() {
    let Some(dev) = super::take(DEVICE_NET) else {
        return;
    };
    let t = dev.transport;
    let feats = match negotiate(&*t, F_MAC | F_STATUS) {
        Ok(f) => f,
        Err(e) => {
            crate::kprintln!("[patina] virtio-net: {}", e);
            return;
        }
    };
    let hdr = if feats & F_VERSION_1 != 0 { 12 } else { 10 };
    let mac = if feats & F_MAC != 0 {
        core::array::from_fn(|i| t.config_read8(i))
    } else {
        let m = generated_mac();
        crate::kprintln!(
            "[patina] virtio-net: device has no MAC, using generated {}",
            fmt_mac(&m)
        );
        m
    };
    let link_up = feats & F_STATUS == 0 || t.config_read16(6) & S_LINK_UP != 0;
    let mk = || -> Option<Net> {
        let rx = VirtQueue::new(&*t, 0, RX_WANT)?;
        let tx = VirtQueue::new(&*t, 1, TX_WANT)?;
        let tx_slots = (tx.size() as usize).min(TX_WANT as usize);
        let mut n = Net {
            hdr,
            mac,
            rx_buf: DmaBuf::new(rx.size() as usize * BUF)?,
            rx,
            rx_slot_of: [0; 128],
            tx_buf: DmaBuf::new(tx_slots * BUF)?,
            tx,
            tx_slot_of: [u8::MAX; 128],
            tx_free: ((1u64 << tx_slots) - 1) as u32,
            tx_slots,
        };
        for s in 0..n.rx.size() as usize {
            n.post_rx(s);
        }
        Some(n)
    };
    let Some(n) = mk() else {
        crate::kprintln!("[patina] virtio-net: queue setup failed");
        t.set_status(super::STATUS_FAILED);
        return;
    };
    driver_ok(&*t);
    let (nrx, ntx) = (n.rx.size(), n.tx_slots);
    let mut loc = String::new();
    let _ = t.describe(&mut loc);
    let Ok(t) = DEV.set(t) else {
        return;
    };
    n.rx.kick(&**t);
    *NET.lock() = Some(n);
    if let Some(line) = dev.irq {
        attach_irq(line, irq);
    }
    crate::kprintln!(
        "[patina] virtio-net: mac {}, queues rx {} / tx {}, link {} ({}, hdr {} B{})",
        fmt_mac(&mac),
        nrx,
        ntx,
        if link_up { "up" } else { "down" },
        loc,
        hdr,
        if dev.irq.is_some() { "" } else { ", no irq" }
    );
    crate::net::device::register(Box::new(VirtioNet { mac }));
}

/// Whether a virtio-net device is driven.
pub fn present() -> bool {
    NET.lock().is_some()
}

// ---- self-test (`patina.selftest=net`) ----

const GW_IP: [u8; 4] = [10, 0, 2, 2];
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];

/// Ethernet + ARP who-has `GW_IP` tell `GUEST_IP`, padded to 60 bytes.
fn arp_request(mac: [u8; 6]) -> [u8; 60] {
    let mut f = [0u8; 60];
    f[0..6].copy_from_slice(&[0xff; 6]);
    f[6..12].copy_from_slice(&mac);
    f[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
    f[14..16].copy_from_slice(&1u16.to_be_bytes()); // htype Ethernet
    f[16..18].copy_from_slice(&0x0800u16.to_be_bytes()); // ptype IPv4
    f[18] = 6;
    f[19] = 4;
    f[20..22].copy_from_slice(&1u16.to_be_bytes()); // request
    f[22..28].copy_from_slice(&mac);
    f[28..32].copy_from_slice(&GUEST_IP);
    // tha zero
    f[38..42].copy_from_slice(&GW_IP);
    f
}

/// The sender MAC of an ARP reply from `GW_IP` to us, if `f` is one.
fn arp_reply(f: &[u8], mac: [u8; 6]) -> Option<[u8; 6]> {
    if f.len() < 42
        || f[12..14] != [0x08, 0x06]
        || f[20..22] != [0, 2]
        || f[28..32] != GW_IP
        || f[32..38] != mac
    {
        return None;
    }
    f[22..28].try_into().ok()
}

fn now() -> u64 {
    crate::arch::timer::now_ns()
}

/// Receive frames until `n` ARP replies arrived or `ns` passed; returns
/// (replies, last sender MAC).
fn collect_replies(mac: [u8; 6], n: usize, ns: u64) -> (usize, Option<[u8; 6]>) {
    let mut got = 0;
    let mut last = None;
    let mut f = [0u8; MAX_FRAME];
    let t0 = now();
    while got < n && now() - t0 < ns {
        let r = NET.lock().as_mut().and_then(|d| d.recv(&mut f));
        match r {
            Some(len) => {
                if let Some(m) = arp_reply(&f[..len], mac) {
                    got += 1;
                    last = Some(m);
                }
            }
            None => core::hint::spin_loop(),
        }
    }
    (got, last)
}

fn send_wait(frame: &[u8], ns: u64) -> bool {
    let t0 = now();
    loop {
        if NET.lock().as_mut().is_some_and(|d| d.send(frame)) {
            return true;
        }
        if now() - t0 >= ns {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// ARP round trips against QEMU's user-mode network (gateway 10.0.2.2):
/// one request, then 64 back to back (transmit slot reuse), with the
/// timer started so device interrupts are taken. Never returns.
pub fn selftest() -> ! {
    let Some(mac) = NET.lock().as_ref().map(|n| n.mac) else {
        crate::kprintln!("[patina] selftest net: no device");
        crate::exit(0);
    };
    // Unmask IRQs (and start the tick) so the device interrupt is taken.
    crate::arch::timer::init();
    let irq0 = IRQS.load(Ordering::Relaxed);
    let req = arp_request(mac);
    let t0 = now();
    assert!(send_wait(&req, 100_000_000), "selftest net: send failed");
    let (got, from) = collect_replies(mac, 1, 2_000_000_000);
    let us = (now() - t0) / 1000;
    let Some(from) = from.filter(|_| got == 1) else {
        panic!("selftest net: no ARP reply from 10.0.2.2 within 2 s");
    };
    crate::kprintln!(
        "[patina] selftest net: arp reply 10.0.2.2 is-at {} after {} us",
        fmt_mac(&from),
        us
    );

    const N: usize = 64;
    let mut sent = 0;
    for _ in 0..N {
        if !send_wait(&req, 500_000_000) {
            break;
        }
        sent += 1;
    }
    let (got, _) = collect_replies(mac, N, 2_000_000_000);
    let irqs = IRQS.load(Ordering::Relaxed) - irq0;
    crate::kprintln!(
        "[patina] selftest net: sent {}/{}, replies {}, irqs {}",
        sent,
        N,
        got,
        irqs
    );
    assert!(
        sent == N && got == N,
        "selftest net: {} sent, {} replies (want {})",
        sent,
        got,
        N
    );
    assert!(irqs > 0, "selftest net: no device interrupt taken");
    crate::kprintln!(
        "[patina] selftest net: ok ({}/{} replies, irqs {})",
        got,
        N,
        irqs
    );
    crate::exit(0)
}
