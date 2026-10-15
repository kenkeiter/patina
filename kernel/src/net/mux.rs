//! The stack's smoltcp devices (design §9, D67): the NIC (if any) plus an
//! in-memory loopback queue.
//!
//! Transmit: smoltcp has no "deliver to self" path, so every transmitted
//! Ethernet frame is classified here: frames for one of our own addresses
//! (or 127/8, `::1`), ARP requests and neighbor solicitations for them, and
//! anything addressed to our own MAC go to the loopback queue and are
//! received by whoever polls next (smoltcp answers its own ARP and learns
//! the neighbor, as in its `examples/loopback.rs`). Everything else goes to
//! the NIC, or is counted and dropped without one. [`Mux`] is the device
//! `poll_egress` sends through (it never receives).
//!
//! Receive: the stack takes one frame at a time ([`Mux::next_frame`]:
//! loopback first, then the NIC) into a buffer owned by the caller (a
//! stack-local `[u8; MAX_FRAME]`: the frame may be fed to its socket set
//! after the interface lock was dropped and re-taken, or queued on the
//! set's backlog), reads its transport tuple to pick the socket set it
//! belongs to (`stack`), and feeds it to smoltcp through [`OneFrame`], a
//! device that yields exactly that frame once and transmits replies
//! through the same classification. The `Mux` lives under the stack's
//! interface lock; the NIC's own calls are serialized by its driver.
//!
//! The one wrinkle: smoltcp never answers a neighbor solicitation for
//! `::1` (its solicited-node check skips the loopback address), so a
//! solicitation for `::1` is looped back re-addressed to `::1` itself.
//! smoltcp then records the soliciting address (`::1`) → our MAC from the
//! solicitation's link-layer option, which is exactly the entry the sender
//! was missing.
//!
//! Frames are copied into fixed buffers: the loopback ring is allocated
//! once (`LO_FRAMES` × `MAX_FRAME`); nothing is allocated per packet.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::net::{Ipv4Addr, Ipv6Addr};

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::Icmpv6Packet;

use super::device::{MAX_FRAME, NetDevice};

/// Loopback queue depth (frames). `transmit` refuses a token when full, so
/// smoltcp holds the data back instead of losing it.
pub const LO_FRAMES: usize = 256;

const ETH_HDR: usize = 14;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const ETHERTYPE_IPV6: u16 = 0x86dd;
const ICMPV6: u8 = 58;
const NDISC_NS: u8 = 135;

/// A fixed ring of frames.
struct LoQueue {
    frames: Box<[[u8; MAX_FRAME]]>,
    lens: Box<[u16]>,
    head: usize,
    len: usize,
}

impl LoQueue {
    fn new() -> LoQueue {
        LoQueue {
            frames: vec![[0u8; MAX_FRAME]; LO_FRAMES].into_boxed_slice(),
            lens: vec![0u16; LO_FRAMES].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    fn is_full(&self) -> bool {
        self.len == LO_FRAMES
    }

    fn push(&mut self, f: &[u8]) -> bool {
        if self.is_full() || f.len() > MAX_FRAME {
            return false;
        }
        let i = (self.head + self.len) % LO_FRAMES;
        self.frames[i][..f.len()].copy_from_slice(f);
        self.lens[i] = f.len() as u16;
        self.len += 1;
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
        Some(n)
    }
}

/// Counters (reported nowhere yet; kept for debugging and a later
/// `/proc/net/dev`).
#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    pub lo_frames: u64,
    pub nic_tx: u64,
    pub nic_rx: u64,
    /// Frames for the NIC dropped because there is none.
    pub no_device_drops: u64,
    /// Frames the NIC refused (no free transmit slot).
    pub nic_tx_drops: u64,
    pub lo_drops: u64,
}

/// The transmit side (what a `TxToken` borrows).
pub struct MuxTx {
    nic: Option<Box<dyn NetDevice>>,
    lo: LoQueue,
    mac: [u8; 6],
    own4: Vec<Ipv4Addr>,
    own6: Vec<Ipv6Addr>,
    buf: Box<[u8; MAX_FRAME]>,
    pub stats: Stats,
}

pub struct Mux {
    pub tx: MuxTx,
    mtu: usize,
}

impl Mux {
    pub fn new(nic: Option<Box<dyn NetDevice>>, mac: [u8; 6]) -> Mux {
        let mtu = nic.as_ref().map_or(1500, |n| n.mtu()).min(1500);
        Mux {
            tx: MuxTx {
                nic,
                lo: LoQueue::new(),
                mac,
                own4: Vec::new(),
                own6: Vec::new(),
                buf: Box::new([0; MAX_FRAME]),
                stats: Stats::default(),
            },
            mtu,
        }
    }

    /// Our addresses (the interface's), for the loopback decision.
    pub fn set_own(&mut self, v4: Vec<Ipv4Addr>, v6: Vec<Ipv6Addr>) {
        self.tx.own4 = v4;
        self.tx.own6 = v6;
    }

    /// Frames waiting in the loopback queue.
    pub fn lo_pending(&self) -> bool {
        self.tx.lo.len > 0
    }

    pub fn has_nic(&self) -> bool {
        self.tx.nic.is_some()
    }

    pub fn nic_name(&self) -> Option<&'static str> {
        self.tx.nic.as_ref().map(|n| n.name())
    }

    pub fn nic_event(&self) -> Option<&'static crate::drivers::Event> {
        self.tx.nic.as_ref().map(|n| n.event())
    }

    /// End of a poll: let the NIC issue deferred doorbells
    /// ([`NetDevice::flush`]).
    pub fn flush(&mut self) {
        if let Some(n) = self.tx.nic.as_mut() {
            n.flush();
        }
    }
}

impl MuxTx {
    /// Send an ARP request for `tpa` from `spa`. Used once a DHCP lease is
    /// applied, for the gateway: the target of a request creates an entry
    /// for the sender (RFC 826; BSD `in_arpinput`), so a host's first packet
    /// to us is not lost to its own ARP resolution. Apple VZ NAT (macOS)
    /// drops that packet, which cost every first connection a 1 s SYN
    /// retransmit.
    pub fn arp_request(&mut self, spa: Ipv4Addr, tpa: Ipv4Addr) {
        let mac = self.mac;
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
        let MuxTx {
            nic,
            lo,
            mac,
            own4,
            own6,
            buf,
            stats,
        } = self;
        let f = &mut buf[..len];
        if len < ETH_HDR {
            return;
        }
        let ethertype = u16::from_be_bytes([f[12], f[13]]);
        let mut local = f[..6] == *mac;
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
            f[..6].copy_from_slice(mac);
            if lo.push(f) {
                stats.lo_frames += 1;
            } else {
                stats.lo_drops += 1;
            }
            return;
        }
        match nic.as_mut() {
            Some(n) => {
                if n.send(f) {
                    stats.nic_tx += 1;
                } else {
                    stats.nic_tx_drops += 1;
                }
            }
            None => stats.no_device_drops += 1,
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

pub struct TxToken<'a>(&'a mut MuxTx);

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let len = len.min(MAX_FRAME);
        let r = f(&mut self.0.buf[..len]);
        self.0.route(len);
        r
    }
}

impl MuxTx {
    /// Whether a transmit token may be handed out now. Not while the
    /// loopback queue is full, nor while the NIC's transmit ring is full:
    /// a token would make `route` drop the frame, which costs TCP a
    /// retransmission timeout (smoltcp's minimum RTO is 1 s; a 4 MiB send
    /// over VZ NAT stalled for 1 s in about half the runs). Without a token
    /// smoltcp keeps the data; the driver's transmit-completion interrupt
    /// wakes the net task to poll again.
    fn can_transmit(&mut self) -> bool {
        if self.lo.is_full() {
            return false;
        }
        if let Some(n) = self.nic.as_mut()
            && !n.can_send()
        {
            return false;
        }
        true
    }
}

fn caps(mtu: usize) -> DeviceCapabilities {
    let mut caps = DeviceCapabilities::default();
    caps.medium = Medium::Ethernet;
    // For Ethernet smoltcp counts the 14-byte header in the MTU.
    caps.max_transmission_unit = mtu + ETH_HDR;
    caps
}

impl Mux {
    /// Take the next received frame into `buf`: the loopback queue first
    /// (if `lo`), then (if `nic`) the NIC. Returns its length and whether
    /// it came from the NIC.
    pub fn next_frame(
        &mut self,
        lo: bool,
        nic: bool,
        buf: &mut [u8; MAX_FRAME],
    ) -> Option<(usize, bool)> {
        if lo && let Some(n) = self.tx.lo.pop_into(buf) {
            return Some((n, false));
        }
        if !nic {
            return None;
        }
        let n = self.tx.nic.as_mut()?.recv(&mut buf[..])?;
        self.tx.stats.nic_rx += 1;
        Some((n, true))
    }

    /// A device that yields `frame` once (and transmits replies).
    pub fn one_frame<'a>(&'a mut self, frame: &'a [u8]) -> OneFrame<'a> {
        OneFrame {
            frame: Some(frame),
            tx: &mut self.tx,
            mtu: self.mtu,
        }
    }
}

/// One received frame as a smoltcp device (see the module docs).
pub struct OneFrame<'a> {
    frame: Option<&'a [u8]>,
    tx: &'a mut MuxTx,
    mtu: usize,
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
        Some((RxToken(f), TxToken(self.tx)))
    }

    fn transmit(&mut self, _now: Instant) -> Option<TxToken<'_>> {
        if !self.tx.can_transmit() {
            return None;
        }
        Some(TxToken(self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        caps(self.mtu)
    }
}

impl Device for Mux {
    type RxToken<'a> = RxToken<'a>;
    type TxToken<'a> = TxToken<'a>;

    /// Egress only: frames are received through [`Mux::next_frame`] and
    /// [`OneFrame`].
    fn receive(&mut self, _now: Instant) -> Option<(RxToken<'_>, TxToken<'_>)> {
        None
    }

    fn transmit(&mut self, _now: Instant) -> Option<TxToken<'_>> {
        if !self.tx.can_transmit() {
            return None;
        }
        Some(TxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        caps(self.mtu)
    }
}
