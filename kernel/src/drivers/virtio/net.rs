//! virtio-net (virtio 1.2 §5.1), decisions D69 and D88: up to one
//! receive/transmit queue pair per vCPU, no offloads, behind the
//! `net::device::NetDevice` seam.
//!
//! * Features: `VIRTIO_NET_F_MAC`, `VIRTIO_NET_F_STATUS` and
//!   `VIRTIO_NET_F_MTU` when offered, plus `VIRTIO_NET_F_MQ` +
//!   `VIRTIO_NET_F_CTRL_VQ` when both are offered and there is more than
//!   one vCPU (and `patina.net.queues` is not 1); no `MRG_RXBUF`,
//!   checksum or GSO offloads. The MAC comes from config space (or is
//!   generated, locally administered, when the device does not offer
//!   one). `F_MTU` is accepted because Apple VZ fails `FEATURES_OK`
//!   without it; the device's MTU (config offset 10) is logged, and one
//!   below 1500 (our stack's) gets a warning.
//! * Queue pairs: P = min(vCPUs, `max_virtqueue_pairs` (config offset 8),
//!   `patina.net.queues`, [`MAX_PAIRS`]); receiveq*k* is queue 2*k*,
//!   transmitq*k* 2*k*+1, the control queue 2 × `max_virtqueue_pairs`.
//!   After `DRIVER_OK` the driver sends `VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET`
//!   P on the control queue and polls (100 ms at most; QEMU answers
//!   within the notify) for `VIRTIO_NET_OK`; anything else leaves one pair
//!   in use. Without MQ there is exactly one pair, as before.
//! * Header (`virtio_net_hdr`): 10 bytes on a legacy transport, 12 (with
//!   `num_buffers`) when `VIRTIO_F_VERSION_1` was negotiated. Transmit
//!   headers are all zero (no checksum offload, `GSO_NONE`); receive
//!   headers are skipped.
//! * Receive: per pair up to 128 posted 2 KiB device-writable buffers, one
//!   descriptor each (header + frame). [`NetDevice::recv`] takes one used
//!   buffer from the pairs in turn (round robin, so one busy queue cannot
//!   starve the others), copies the frame out, re-posts the buffer and
//!   kicks that queue. All pairs feed the one smoltcp interface (D67).
//! * Transmit: per pair up to 32 slots of 2 KiB (header + frame, one
//!   device-readable descriptor). [`NetDevice::send`] uses the pair of the
//!   current CPU (`cpu % P`), or the next pair with a free slot when that
//!   one is full; `can_send` is false only when every pair is full, which
//!   keeps D85's "no transmit token while the NIC cannot take a frame".
//! * Interrupts: the callback only reads+acknowledges the interrupt status
//!   and signals [`EVENT`] (D59); it never takes the driver lock, so a CPU
//!   holding that lock with IRQs masked cannot deadlock against it. On
//!   virtio-pci with MSI-X and P > 1, each pair gets its own vector
//!   ([`Transport::alloc_vector`]; config changes and the control queue
//!   keep vector 0), and pair *k*'s vector is moved to CPU *k* once that
//!   CPU is online (checked on the receive path; secondaries start after
//!   the drivers): `GICD_IROUTER` for GICv2m SPIs, a rewritten message
//!   address on x86. Otherwise (virtio-mmio, INTx, ITS LPIs) all queues
//!   share the device's one interrupt on the boot CPU.
//!
//! State: the transport lives in a [`OnceCell`] (the IRQ callback reads it
//! without a lock); the rings of every pair live in one
//! `SpinLock<Option<Net>>` (the stack calls the device under its own single
//! lock anyway, D67). The registered `NetDevice` is a handle onto that
//! lock. Counters for `patina.stats` are lock-free.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use super::queue::{Buf, DmaBuf, VirtQueue};
use super::{DEVICE_NET, F_VERSION_1, Transport, attach_irq, driver_ok, negotiate_with};
use crate::drivers::Event;
use crate::net::device::{MAX_FRAME, NetDevice};
use crate::sync::{OnceCell, SpinLock};

/// VIRTIO_NET_F_MAC: config space holds the MAC.
const F_MAC: u64 = 1 << 5;
/// VIRTIO_NET_F_STATUS: config space holds the link status.
const F_STATUS: u64 = 1 << 16;
/// VIRTIO_NET_F_MTU: config space holds the device's maximum MTU.
const F_MTU: u64 = 1 << 3;
/// VIRTIO_NET_F_CTRL_VQ: a control queue.
const F_CTRL_VQ: u64 = 1 << 17;
/// VIRTIO_NET_F_MQ: several queue pairs (needs CTRL_VQ).
const F_MQ: u64 = 1 << 22;
/// `status` bit VIRTIO_NET_S_LINK_UP.
const S_LINK_UP: u16 = 1;

/// Control queue: class VIRTIO_NET_CTRL_MQ, command
/// VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET, ack VIRTIO_NET_OK.
const CTRL_MQ: u8 = 4;
const CTRL_MQ_VQ_PAIRS_SET: u8 = 0;
const CTRL_OK: u8 = 0;

/// Most queue pairs driven (also the per-pair counter arrays).
pub const MAX_PAIRS: usize = 8;
/// CPUs with a NIC interrupt counter (`patina.stats`).
const IRQ_CPUS: usize = 16;

/// Bytes per receive buffer / transmit slot.
const BUF: usize = 2048;
const RX_WANT: u16 = 128;
const TX_WANT: u16 = 32;

/// One receive/transmit queue pair.
struct Pair {
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
    /// MSI-X vector of both queues (0: the shared interrupt).
    vector: u16,
}

struct Net {
    /// `virtio_net_hdr` size: 10 (legacy) or 12 (VERSION_1).
    hdr: usize,
    mac: [u8; 6],
    /// Every pair set up; the first `active` are in use.
    pairs: Vec<Pair>,
    active: usize,
    /// Next pair `recv` looks at first.
    rx_next: usize,
    /// Pairs whose vector still waits for its CPU to come online.
    affinity_pending: u32,
    /// The control queue and its command buffer (kept: the device knows
    /// their addresses).
    _ctrl: Option<(VirtQueue, DmaBuf)>,
}

static DEV: OnceCell<Box<dyn Transport>> = OnceCell::new();
static NET: SpinLock<Option<Net>> = SpinLock::new(None);
/// Signalled by the interrupt (receive completions, freed transmit slots).
static EVENT: Event = Event::new();
/// Interrupt callbacks taken (for the self-test and diagnostics).
static IRQS: AtomicU32 = AtomicU32::new(0);
/// `patina.stats`: pairs in use, frames per pair, interrupts per CPU.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static RX_FRAMES: [AtomicU64; MAX_PAIRS] = [const { AtomicU64::new(0) }; MAX_PAIRS];
static TX_FRAMES: [AtomicU64; MAX_PAIRS] = [const { AtomicU64::new(0) }; MAX_PAIRS];
static IRQ_CPU: [AtomicU32; IRQ_CPUS] = [const { AtomicU32::new(0) }; IRQ_CPUS];

fn irq() {
    IRQS.fetch_add(1, Ordering::Relaxed);
    if let Some(c) = IRQ_CPU.get(crate::arch::cpu::id()) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(t) = DEV.get() {
        t.ack_interrupt();
    }
    EVENT.signal();
}

fn transport() -> &'static dyn Transport {
    &**DEV.get().expect("virtio-net: no transport")
}

impl Pair {
    /// Set up pair `k` (queues 2k, 2k+1) on `vector` and post every
    /// receive buffer.
    fn new(t: &dyn Transport, k: u16, vector: u16) -> Option<Pair> {
        if vector != 0 {
            t.set_queue_vector(2 * k, vector);
            t.set_queue_vector(2 * k + 1, vector);
        }
        let rx = VirtQueue::new(t, 2 * k, RX_WANT)?;
        let tx = VirtQueue::new(t, 2 * k + 1, TX_WANT)?;
        let tx_slots = (tx.size() as usize).min(TX_WANT as usize);
        let mut p = Pair {
            rx_buf: DmaBuf::new(rx.size() as usize * BUF)?,
            rx,
            rx_slot_of: [0; 128],
            tx_buf: DmaBuf::new(tx_slots * BUF)?,
            tx,
            tx_slot_of: [u8::MAX; 128],
            tx_free: ((1u64 << tx_slots) - 1) as u32,
            tx_slots,
            vector,
        };
        for s in 0..p.rx.size() as usize {
            p.post_rx(s);
        }
        Some(p)
    }

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

    fn recv(&mut self, hdr: usize, buf: &mut [u8]) -> Option<usize> {
        let mut reposted = false;
        let mut out = None;
        while let Some((h, len)) = self.rx.pop_used() {
            let slot = self.rx_slot_of[h as usize] as usize;
            let len = (len as usize).min(BUF);
            let ok = len > hdr && len - hdr <= buf.len();
            if ok {
                let n = len - hdr;
                self.rx_buf.read(slot * BUF + hdr, &mut buf[..n]);
                out = Some(n);
            }
            reposted |= self.post_rx(slot);
            if ok {
                break;
            }
        }
        if reposted {
            let t = crate::net::prof::start(crate::net::prof::on());
            self.rx.kick(transport());
            crate::net::prof::NIC_RX_KICK.done(t);
        }
        out
    }

    /// Stage and kick `frame`; false when no slot is free.
    fn send(&mut self, hdr: usize, frame: &[u8]) -> bool {
        if !self.can_send() {
            return false;
        }
        let slot = self.tx_free.trailing_zeros() as usize;
        let off = slot * BUF;
        self.tx_buf.write(off, &[0u8; 12][..hdr]);
        self.tx_buf.write(off + hdr, frame);
        let b = Buf {
            addr: self.tx_buf.pa(off),
            len: (hdr + frame.len()) as u32,
            write: false,
        };
        let Some(h) = self.tx.add(&[b]) else {
            return false;
        };
        self.tx_slot_of[h as usize] = slot as u8;
        self.tx_free &= !(1 << slot);
        let t = crate::net::prof::start(crate::net::prof::on());
        self.tx.kick(transport());
        crate::net::prof::NIC_TX_KICK.done(t);
        true
    }
}

impl Net {
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        if self.affinity_pending != 0 {
            self.apply_affinity();
        }
        let n = self.active;
        for i in 0..n {
            let k = (self.rx_next + i) % n;
            if let Some(len) = self.pairs[k].recv(self.hdr, buf) {
                self.rx_next = (k + 1) % n;
                RX_FRAMES[k].fetch_add(1, Ordering::Relaxed);
                return Some(len);
            }
        }
        None
    }

    fn can_send(&mut self) -> bool {
        self.pairs[..self.active].iter_mut().any(|p| p.can_send())
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() > BUF - self.hdr {
            return true; // cannot be sent at all: drop it, as a NIC would
        }
        let n = self.active;
        let first = crate::arch::cpu::id() % n;
        for i in 0..n {
            let k = (first + i) % n;
            if self.pairs[k].send(self.hdr, frame) {
                TX_FRAMES[k].fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    /// Move pair k's vector to CPU k for every such CPU that is online by
    /// now (bounded work: at most one pass over the pending pairs).
    fn apply_affinity(&mut self) {
        let mut m = self.affinity_pending;
        while m != 0 {
            let k = m.trailing_zeros() as usize;
            m &= m - 1;
            if !crate::arch::smp::is_online(k) {
                continue;
            }
            self.affinity_pending &= !(1 << k);
            let v = self.pairs[k].vector;
            if !transport().set_vector_affinity(v, k) {
                crate::kprintln!(
                    "[patina] virtio-net: pair {} interrupt stays on cpu 0 (cannot be routed to cpu {})",
                    k,
                    k
                );
            }
        }
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

/// Send `VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET pairs` on control queue `q` and
/// poll for the device's answer (100 ms at most). Returns the queue and
/// its buffer (to keep) and whether the device said `VIRTIO_NET_OK`.
fn set_pairs(
    t: &dyn Transport,
    q: u16,
    pairs: u16,
) -> (Option<(VirtQueue, DmaBuf)>, Result<(), &'static str>) {
    let Some(buf) = DmaBuf::new(64) else {
        return (None, Err("no memory"));
    };
    let Some(mut cq) = VirtQueue::new(t, q, 8) else {
        return (None, Err("no control queue"));
    };
    // Header (class, command) at 0, data (le16 pairs) at 16, ack at 32.
    buf.write(0, &[CTRL_MQ, CTRL_MQ_VQ_PAIRS_SET]);
    buf.write(16, &pairs.to_le_bytes());
    buf.write(32, &[0xff]);
    let chain = [
        Buf {
            addr: buf.pa(0),
            len: 2,
            write: false,
        },
        Buf {
            addr: buf.pa(16),
            len: 2,
            write: false,
        },
        Buf {
            addr: buf.pa(32),
            len: 1,
            write: true,
        },
    ];
    // Polled, not interrupt driven: the shared vector / line still fires
    // and only wakes the (not yet running) net task.
    cq.set_no_interrupt(true);
    let r = if cq.add(&chain).is_none() {
        Err("control queue full")
    } else {
        cq.kick(t);
        if !super::poll_until(100_000_000, || cq.has_used()) {
            Err("no answer within 100 ms")
        } else {
            let _ = cq.pop_used();
            let mut ack = [0xffu8];
            buf.read(32, &mut ack);
            if ack[0] == CTRL_OK {
                Ok(())
            } else {
                Err("device answered VIRTIO_NET_ERR")
            }
        }
    };
    (Some((cq, buf)), r)
}

/// Claim and start the first virtio-net device, if any, and register it
/// with `net::device`. `patina.net.queues=N` caps the queue pairs (1: do
/// not negotiate multiqueue).
pub fn init(cmdline: &patina_cmdline::Cmdline) {
    let Some(dev) = super::take(DEVICE_NET) else {
        return;
    };
    let ncpu = crate::arch::smp::cpu_count().max(1);
    let cap = match cmdline.get("patina.net.queues") {
        Some(Some(v)) => match v.parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => {
                crate::kprintln!("[patina] virtio-net: ignoring patina.net.queues={:?}", v);
                MAX_PAIRS
            }
        },
        _ => MAX_PAIRS,
    }
    .min(MAX_PAIRS);
    let want_mq = ncpu > 1 && cap > 1;
    // The transport goes into DEV first, so the interrupt callbacks can be
    // attached (and acknowledge) before the device may interrupt.
    if DEV.set(dev.transport).is_err() {
        return;
    }
    let t = transport();
    let feats = match negotiate_with(t, |offered| {
        let mq = F_MQ | F_CTRL_VQ;
        F_MAC | F_STATUS | F_MTU | if want_mq && offered & mq == mq { mq } else { 0 }
    }) {
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
    let mtu = (feats & F_MTU != 0).then(|| t.config_read16(10));
    if let Some(m) = mtu
        && (m as usize) < MAX_FRAME - 14
    {
        crate::kprintln!(
            "[patina] virtio-net: warning: device mtu {} is below the stack's {}",
            m,
            MAX_FRAME - 14
        );
    }
    let mq = feats & F_MQ != 0;
    // max_virtqueue_pairs: 1..=0x8000 (virtio 1.2 §5.1.4).
    let max_pairs = if mq {
        t.config_read16(8).clamp(1, 0x8000)
    } else {
        1
    };
    let npairs = ncpu.min(max_pairs as usize).min(cap);

    // One MSI-X vector per pair when there are several and the table has
    // room for all of them; otherwise every queue shares vector 0 / the line.
    let mut vectors = Vec::new();
    if npairs > 1 {
        for _ in 0..npairs {
            match t.alloc_vector() {
                Some(v) => vectors.push(v),
                None => break,
            }
        }
        if vectors.len() < npairs {
            vectors.clear(); // the entries stay unused
        }
    }
    let mut pairs = Vec::with_capacity(npairs);
    for k in 0..npairs {
        let v = vectors.get(k).map_or(0, |v| v.0);
        match Pair::new(t, k as u16, v) {
            Some(p) => pairs.push(p),
            None => break,
        }
    }
    if pairs.is_empty() {
        crate::kprintln!("[patina] virtio-net: queue setup failed");
        t.set_status(super::STATUS_FAILED);
        return;
    }
    // Pairs set up with their own vector (pair 0's is used even if
    // VQ_PAIRS_SET fails below).
    let per_pair = !vectors.is_empty();
    // Interrupts before DRIVER_OK: an edge-triggered line (virtio-mmio on
    // QEMU `virt`) whose first edge came before the GIC was configured
    // would otherwise stay high, unacknowledged, and never interrupt again.
    if let Some(line) = dev.irq {
        attach_irq(line, irq);
    }
    if per_pair {
        for (_, line) in &vectors[..pairs.len()] {
            attach_irq(*line, irq);
        }
    }
    driver_ok(t);
    let mut active = 1;
    let mut ctrl = None;
    if mq {
        let (c, r) = set_pairs(t, 2 * max_pairs, pairs.len() as u16);
        ctrl = c;
        match r {
            Ok(()) => active = pairs.len(),
            Err(e) => crate::kprintln!(
                "[patina] virtio-net: VQ_PAIRS_SET {} failed ({}), using 1 pair",
                pairs.len(),
                e
            ),
        }
    }
    let (nrx, ntx) = (pairs[0].rx.size(), pairs[0].tx_slots);
    let mut loc = String::new();
    let _ = t.describe(&mut loc);
    for p in &pairs[..active] {
        p.rx.kick(t);
    }
    let affinity_pending = if per_pair && active > 1 {
        // Pair 0 is on the boot CPU already.
        (((1u64 << active) - 1) & !1) as u32
    } else {
        0
    };
    ACTIVE.store(active, Ordering::Relaxed);
    *NET.lock() = Some(Net {
        hdr,
        mac,
        pairs,
        active,
        rx_next: 0,
        affinity_pending,
        _ctrl: ctrl,
    });
    crate::kprintln!(
        "[patina] virtio-net: mac {}, queues rx {} / tx {}, pairs {}{}{}, link {}{} ({}, hdr {} B{})",
        fmt_mac(&mac),
        nrx,
        ntx,
        active,
        if mq {
            alloc::format!(" of {}", max_pairs)
        } else {
            String::new()
        },
        if per_pair && active > 1 {
            ", msi-x vector per pair"
        } else {
            ""
        },
        if link_up { "up" } else { "down" },
        mtu.map_or(String::new(), |m| alloc::format!(", mtu {}", m)),
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

/// `patina.stats=1` at exit (other CPUs stopped; no locks taken): pairs,
/// frames per pair, NIC interrupts per CPU.
pub fn print_stats() {
    let n = ACTIVE.load(Ordering::Relaxed);
    if n == 0 {
        return;
    }
    let list = |a: &[AtomicU64]| {
        let mut s = String::new();
        for (i, c) in a.iter().enumerate() {
            if i > 0 {
                s.push('/');
            }
            let _ =
                core::fmt::Write::write_fmt(&mut s, format_args!("{}", c.load(Ordering::Relaxed)));
        }
        s
    };
    let (mut per_cpu, mut cpus) = (String::new(), String::new());
    let ncpu = crate::arch::smp::cpu_count().clamp(1, IRQ_CPUS);
    for (c, cnt) in IRQ_CPU[..ncpu].iter().enumerate() {
        let v = cnt.load(Ordering::Relaxed);
        if c > 0 {
            per_cpu.push('/');
        }
        let _ = core::fmt::Write::write_fmt(&mut per_cpu, format_args!("{}", v));
        if v > 0 {
            if !cpus.is_empty() {
                cpus.push(',');
            }
            let _ = core::fmt::Write::write_fmt(&mut cpus, format_args!("{}", c));
        }
    }
    crate::console::panic_write_fmt(format_args!(
        "[patina] stats net: virtio-net pairs {}, rx frames {}, tx frames {}, irqs per cpu {}, irq cpus {}\n",
        n,
        list(&RX_FRAMES[..n]),
        list(&TX_FRAMES[..n]),
        per_cpu,
        if cpus.is_empty() { "-" } else { &cpus },
    ));
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
