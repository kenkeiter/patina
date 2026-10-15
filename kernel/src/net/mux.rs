//! The stack's smoltcp devices (design §9, D67): the NIC (if any) plus an
//! in-memory loopback queue.
//!
//! Transmit: smoltcp has no "deliver to self" path, so every transmitted
//! Ethernet frame is classified here: frames for one of our own addresses
//! (or 127/8, `::1`), ARP requests and neighbor solicitations for them, and
//! anything addressed to our own MAC go to the loopback queue and are
//! received by whoever drains next (smoltcp answers its own ARP and learns
//! the neighbor, as in its `examples/loopback.rs`). Everything else goes to
//! the NIC, or is counted and dropped without one. Each interface shard
//! (`stack::Shard`, one per CPU) has its own [`Tx`] — the device
//! `poll_egress` sends through, with its own frame buffer and its copy of
//! our addresses — over the shared [`Out`]: the loopback queue (behind its
//! own leaf lock) and the NIC (whose driver serializes its calls with its
//! own lock). So shards transmit in parallel.
//!
//! Receive: the stack takes one frame at a time ([`Out::next_frame`]:
//! loopback first, then the NIC) into a buffer owned by the caller (a
//! stack-local `[u8; MAX_FRAME]`: the frame is fed to its socket set after
//! the registry lock is dropped, or queued on the set's backlog), reads its
//! transport tuple to pick the socket set it belongs to (`stack`), and
//! feeds it to smoltcp through [`OneFrame`], a device that yields exactly
//! that frame once and transmits replies through the same classification
//! (or, `mute`, drops them: a neighbor frame shown to the other shards so
//! they learn the neighbor too).
//!
//! The one wrinkle: smoltcp never answers a neighbor solicitation for
//! `::1` (its solicited-node check skips the loopback address), so a
//! solicitation for `::1` is looped back re-addressed to `::1` itself.
//! smoltcp then records the soliciting address (`::1`) → our MAC from the
//! solicitation's link-layer option, which is exactly the entry the sender
//! was missing.
//!
//! Frames are copied into fixed buffers: the loopback ring is allocated
//! once (`LO_FRAMES` × `MAX_FRAME`). Shards check for room before taking a
//! frame to send, but they race: a frame that finds the ring full goes to
//! an overflow queue (allocated) rather than being dropped (smoltcp's
//! minimum RTO is 1 s), and a frame that finds the NIC's transmit ring full
//! waits briefly for a slot.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::net::{Ipv4Addr, Ipv6Addr};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::Icmpv6Packet;

use super::device::{MAX_FRAME, NetDevice};
use crate::sync::SpinLock;

/// Loopback queue depth (frames). Shards stop taking transmit tokens when
/// it is full, so smoltcp holds the data back.
pub const LO_FRAMES: usize = 256;
/// How long a frame waits for a free NIC transmit slot before it is
/// dropped (ns).
const NIC_WAIT_NS: u64 = 2_000_000;

const ETH_HDR: usize = 14;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const ETHERTYPE_IPV6: u16 = 0x86dd;
const ICMPV6: u8 = 58;
const NDISC_NS: u8 = 135;

/// A fixed ring of frames, plus an overflow queue for frames pushed by
/// racing shards while it is full.
struct LoQueue {
    frames: Box<[[u8; MAX_FRAME]]>,
    lens: Box<[u16]>,
    head: usize,
    len: usize,
    over: VecDeque<Box<[u8]>>,
}

impl LoQueue {
    fn new() -> LoQueue {
        LoQueue {
            frames: vec![[0u8; MAX_FRAME]; LO_FRAMES].into_boxed_slice(),
            lens: vec![0u16; LO_FRAMES].into_boxed_slice(),
            head: 0,
            len: 0,
            over: VecDeque::new(),
        }
    }

    fn ring_push(&mut self, f: &[u8]) {
        let i = (self.head + self.len) % LO_FRAMES;
        self.frames[i][..f.len()].copy_from_slice(f);
        self.lens[i] = f.len() as u16;
        self.len += 1;
    }

    fn push(&mut self, f: &[u8]) -> bool {
        if f.len() > MAX_FRAME {
            return false;
        }
        if self.len == LO_FRAMES || !self.over.is_empty() {
            self.over.push_back(Box::from(f));
        } else {
            self.ring_push(f);
        }
        true
    }

    fn pop_into(&mut self, out: &mut [u8; MAX_FRAME]) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let n = self.lens[self.head] as usize;
        out[..n].copy_from_slice(&self.frames[self.head][..n]);
        self.head = (self.head + 1) % LO_FRAMES;
        self.len -= 1;
        // Overflowed frames are newer than every frame in the ring.
        if let Some(f) = self.over.pop_front() {
            self.ring_push(&f);
        }
        Some(n)
    }

    fn total(&self) -> usize {
        self.len + self.over.len()
    }
}

/// Counters (`patina.stats=1` prints them).
#[derive(Default)]
pub struct Stats {
    pub lo_frames: AtomicU64,
    pub nic_tx: AtomicU64,
    pub nic_rx: AtomicU64,
    /// Frames for the NIC dropped because there is none.
    pub no_device_drops: AtomicU64,
    /// Frames the NIC refused (no free transmit slot within `NIC_WAIT_NS`).
    pub nic_tx_drops: AtomicU64,
    /// Frames that had to wait for a NIC transmit slot.
    pub nic_tx_waits: AtomicU64,
    pub lo_drops: AtomicU64,
    /// Frames that went to the loopback overflow queue.
    pub lo_overflow: AtomicU64,
}

/// What every shard shares: the loopback queue and the NIC.
pub struct Out {
    nic: Option<&'static dyn NetDevice>,
    lo: SpinLock<LoQueue>,
    /// Frames in the loopback queue (read without its lock).
    lo_len: AtomicUsize,
    pub mac: [u8; 6],
    pub mtu: usize,
    pub stats: Stats,
}

fn add(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

impl Out {
    pub fn new(nic: Option<Box<dyn NetDevice>>, mac: [u8; 6]) -> Out {
        let mtu = nic.as_ref().map_or(1500, |n| n.mtu()).min(1500);
        Out {
            nic: nic.map(|n| &*Box::leak(n)),
            lo: SpinLock::new(LoQueue::new()),
            lo_len: AtomicUsize::new(0),
            mac,
            mtu,
            stats: Stats::default(),
        }
    }

    /// Frames waiting in the loopback queue.
    pub fn lo_pending(&self) -> bool {
        self.lo_len.load(Ordering::Relaxed) > 0
    }

    pub fn has_nic(&self) -> bool {
        self.nic.is_some()
    }

    pub fn nic_event(&self) -> Option<&'static crate::drivers::Event> {
        self.nic.map(|n| n.event())
    }

    /// Let the NIC issue deferred doorbells ([`NetDevice::flush`]).
    pub fn flush(&self) {
        if let Some(n) = self.nic {
            n.flush();
        }
    }

    /// Take the next received frame into `buf`: the loopback queue first
    /// (if `lo`), then (if `nic`) the NIC. Returns its length and whether
    /// it came from the NIC. Callers serialize this (the stack's registry
    /// lock), so frames are taken in order.
    pub fn next_frame(
        &self,
        lo: bool,
        nic: bool,
        buf: &mut [u8; MAX_FRAME],
    ) -> Option<(usize, bool)> {
        if lo && self.lo_pending() {
            let mut q = self.lo.lock();
            if let Some(n) = q.pop_into(buf) {
                self.lo_len.store(q.total(), Ordering::Relaxed);
                return Some((n, false));
            }
        }
        if !nic {
            return None;
        }
        let t = super::prof::start(super::prof::on());
        let n = self.nic?.recv(&mut buf[..])?;
        super::prof::NIC_RECV.done(t);
        add(&self.stats.nic_rx);
        Some((n, true))
    }

    /// Whether a transmit token may be handed out now. Not while the
    /// loopback queue is full, nor while the NIC's transmit ring is full:
    /// without a token smoltcp keeps the data (the driver's
    /// transmit-completion interrupt wakes the net task to poll again).
    fn can_transmit(&self) -> bool {
        if self.lo_len.load(Ordering::Relaxed) >= LO_FRAMES {
            return false;
        }
        if let Some(n) = self.nic
            && !n.can_send()
        {
            return false;
        }
        true
    }
}

/// A shard's transmit side (the device `poll_egress` sends through).
pub struct Tx {
    out: &'static Out,
    own4: Vec<Ipv4Addr>,
    own6: Vec<Ipv6Addr>,
    buf: Box<[u8; MAX_FRAME]>,
}

impl Tx {
    pub fn new(out: &'static Out) -> Tx {
        Tx {
            out,
            own4: Vec::new(),
            own6: Vec::new(),
            buf: Box::new([0; MAX_FRAME]),
        }
    }

    pub fn out(&self) -> &'static Out {
        self.out
    }

    /// Our addresses (the interface's), for the loopback decision.
    pub fn set_own(&mut self, v4: Vec<Ipv4Addr>, v6: Vec<Ipv6Addr>) {
        self.own4 = v4;
        self.own6 = v6;
    }

    /// Send an ARP request for `tpa` from `spa`. Used once a DHCP lease is
    /// applied, for the gateway: the target of a request creates an entry
    /// for the sender (RFC 826; BSD `in_arpinput`), so a host's first packet
    /// to us is not lost to its own ARP resolution. Apple VZ NAT (macOS)
    /// drops that packet, which cost every first connection a 1 s SYN
    /// retransmit.
    pub fn arp_request(&mut self, spa: Ipv4Addr, tpa: Ipv4Addr) {
        let mac = self.out.mac;
        let f = &mut self.buf[..ETH_HDR + 28];
        f[..6].fill(0xff);
        f[6..12].copy_from_slice(&mac);
        f[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
        // htype 1, ptype IPv4, hlen 6, plen 4, op 1 (request).
        f[14..22].copy_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
        f[22..28].copy_from_slice(&mac);
        f[28..32].copy_from_slice(&spa.octets());
        f[32..38].fill(0);
        f[38..42].copy_from_slice(&tpa.octets());
        self.route(ETH_HDR + 28);
    }

    /// Route the frame in `self.buf[..len]`.
    fn route(&mut self, len: usize) {
        let Tx {
            out,
            own4,
            own6,
            buf,
        } = self;
        let mac = out.mac;
        let f = &mut buf[..len];
        if len < ETH_HDR {
            return;
        }
        let ethertype = u16::from_be_bytes([f[12], f[13]]);
        let mut local = f[..6] == mac;
        match ethertype {
            ETHERTYPE_ARP if len >= ETH_HDR + 28 => {
                let op = u16::from_be_bytes([f[20], f[21]]);
                let tpa = Ipv4Addr::new(f[38], f[39], f[40], f[41]);
                if op == 1 && (tpa.is_loopback() || own4.contains(&tpa)) {
                    local = true;
                }
            }
            ETHERTYPE_IPV4 if len >= ETH_HDR + 20 => {
                let dst = Ipv4Addr::new(f[30], f[31], f[32], f[33]);
                if dst.is_loopback() || own4.contains(&dst) {
                    local = true;
                }
            }
            ETHERTYPE_IPV6 if len >= ETH_HDR + 40 => {
                let mut d = [0u8; 16];
                d.copy_from_slice(&f[38..54]);
                let dst = Ipv6Addr::from(d);
                if dst.is_loopback() || own6.contains(&dst) {
                    local = true;
                } else if f[20] == ICMPV6 && len >= ETH_HDR + 40 + 24 && f[54] == NDISC_NS {
                    let mut t = [0u8; 16];
                    t.copy_from_slice(&f[62..78]);
                    let target = Ipv6Addr::from(t);
                    if target == Ipv6Addr::LOCALHOST {
                        // See the module docs: re-address it to ::1.
                        f[38..54].copy_from_slice(&target.octets());
                        let mut s = [0u8; 16];
                        s.copy_from_slice(&f[22..38]);
                        let src = Ipv6Addr::from(s);
                        let plen = u16::from_be_bytes([f[18], f[19]]) as usize;
                        let end = (ETH_HDR + 40 + plen).min(len);
                        Icmpv6Packet::new_unchecked(&mut f[ETH_HDR + 40..end])
                            .fill_checksum(&src, &target);
                        local = true;
                    } else if own6.contains(&target) {
                        local = true;
                    }
                }
            }
            _ => {}
        }
        if local {
            f[..6].copy_from_slice(&mac);
            let mut q = out.lo.lock();
            let full = q.len == LO_FRAMES;
            if q.push(f) {
                out.lo_len.store(q.total(), Ordering::Relaxed);
                drop(q);
                add(&out.stats.lo_frames);
                if full {
                    add(&out.stats.lo_overflow);
                }
            } else {
                drop(q);
                add(&out.stats.lo_drops);
            }
            return;
        }
        match out.nic {
            Some(n) => {
                let t = super::prof::start(super::prof::on());
                let mut ok = n.send(f);
                if !ok {
                    // Another shard took the last slot since this one got
                    // its token: wait for the device to complete one.
                    add(&out.stats.nic_tx_waits);
                    let t0 = crate::arch::timer::now_ns();
                    while !ok && crate::arch::timer::now_ns() - t0 < NIC_WAIT_NS {
                        core::hint::spin_loop();
                        ok = n.send(f);
                    }
                }
                super::prof::NIC_SEND.done(t);
                if ok {
                    add(&out.stats.nic_tx);
                } else {
                    add(&out.stats.nic_tx_drops);
                }
            }
            None => add(&out.stats.no_device_drops),
        }
    }
}

pub struct RxToken<'a>(&'a [u8]);

impl phy::RxToken for RxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.0)
    }
}

/// A transmit token; `mute` drops the frame.
pub struct TxToken<'a> {
    tx: &'a mut Tx,
    mute: bool,
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let len = len.min(MAX_FRAME);
        let r = f(&mut self.tx.buf[..len]);
        if !self.mute {
            self.tx.route(len);
        }
        r
    }
}

fn caps(mtu: usize) -> DeviceCapabilities {
    let mut caps = DeviceCapabilities::default();
    caps.medium = Medium::Ethernet;
    // For Ethernet smoltcp counts the 14-byte header in the MTU.
    caps.max_transmission_unit = mtu + ETH_HDR;
    caps
}

impl Tx {
    /// A device that yields `frame` once (and transmits replies, unless
    /// `mute`).
    pub fn one_frame<'a>(&'a mut self, frame: &'a [u8], mute: bool) -> OneFrame<'a> {
        OneFrame {
            frame: Some(frame),
            tx: self,
            mute,
        }
    }
}

/// One received frame as a smoltcp device (see the module docs).
pub struct OneFrame<'a> {
    frame: Option<&'a [u8]>,
    tx: &'a mut Tx,
    mute: bool,
}

impl Device for OneFrame<'_> {
    type RxToken<'a>
        = RxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _now: Instant) -> Option<(RxToken<'_>, TxToken<'_>)> {
        let f = self.frame.take()?;
        Some((
            RxToken(f),
            TxToken {
                tx: self.tx,
                mute: self.mute,
            },
        ))
    }

    fn transmit(&mut self, _now: Instant) -> Option<TxToken<'_>> {
        if !self.mute && !self.tx.out.can_transmit() {
            return None;
        }
        Some(TxToken {
            tx: self.tx,
            mute: self.mute,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        caps(self.tx.out.mtu)
    }
}

impl Device for Tx {
    type RxToken<'a> = RxToken<'a>;
    type TxToken<'a> = TxToken<'a>;

    /// Egress only: frames are received through [`Out::next_frame`] and
    /// [`OneFrame`].
    fn receive(&mut self, _now: Instant) -> Option<(RxToken<'_>, TxToken<'_>)> {
        None
    }

    fn transmit(&mut self, _now: Instant) -> Option<TxToken<'_>> {
        if !self.out.can_transmit() {
            return None;
        }
        Some(TxToken {
            tx: self,
            mute: false,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        caps(self.out.mtu)
    }
}
