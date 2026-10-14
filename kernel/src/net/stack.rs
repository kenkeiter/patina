//! The TCP/IP stack (design §9, D67): one smoltcp `Interface` over the
//! [`Mux`] device (NIC + loopback), one `SocketSet`, the per-socket state of
//! every inet socket (`inet::Sock`) and the bind table, all behind one
//! IRQ-safe `SpinLock<Stack>`.
//!
//! Who runs the interface:
//!
//! * the **net task** (a kernel thread): poll, compute readiness changes,
//!   fire them, then sleep on an `Event` until `poll_delay` (at most 1 s).
//!   The event is the NIC's (its IRQ handler signals it and never takes the
//!   stack lock) or a stack-owned one without a NIC;
//! * every **socket syscall** that changes state (send, recv, connect,
//!   listen, close, ...): it polls inline under the lock right after the
//!   change, so a write is on the wire (or in the peer's receive buffer,
//!   over loopback) before the syscall returns. If the poll leaves an
//!   earlier deadline than the one the task sleeps on, it signals the
//!   event.
//!
//! [`Stack::poll_all`] drains the loopback queue (bounded rounds) and feeds
//! ingress one packet at a time, refilling TCP listen pools after each
//! packet (`inet`), so a burst of SYNs finds a listening socket for each.
//! Readiness notifications are collected under the lock and fired after
//! it is released (fs/file.rs lock order).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::net::{Ipv4Addr, Ipv6Addr};

use smoltcp::iface::{
    Config, Interface, PollIngressSingleResult, PollResult, SocketHandle, SocketSet,
};
use smoltcp::socket::dhcpv4;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Cidr, Ipv6Cidr};

use super::inet::Sock;
use super::mux::Mux;
use crate::drivers::Event;
use crate::fs::file::Notify;
use crate::sync::{OnceCell, SpinLock};
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, Ordering};
use patina_inet::PortTable;

/// Longest the net task sleeps between polls.
const MAX_WAIT_NS: u64 = 1_000_000_000;
/// Loopback delivery rounds per [`Stack::poll_all`].
const MAX_ROUNDS: usize = 64;
/// Frames ingested per round (bounds the time spent under the lock).
const MAX_INGRESS: usize = 1024;

/// MAC used when there is no NIC: locally administered, unicast.
const LO_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];

/// How long boot waits for a DHCP lease before user mode starts anyway.
const DHCP_WAIT_NS: u64 = 10_000_000_000;
/// DHCP options requested: subnet mask, router, DNS, host name, domain.
static DHCP_PARAMS: [u8; 5] = [1, 3, 6, 12, 15];

/// The effective IPv4 configuration: the static `ip=` values, replaced by
/// a DHCP lease when there is one. `/etc/resolv.conf` and `/etc/hosts` are
/// generated from it (`fs::rootfs`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetConfig {
    /// Interface address and prefix length (not loopback / link-local).
    pub addr: Option<(Ipv4Addr, u8)>,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    /// Host name from the DHCP server (option 12), if any.
    pub hostname: Option<String>,
    /// Domain name from the DHCP server (option 15), if any.
    pub domain: Option<String>,
    /// True when `addr` comes from a DHCP lease.
    pub leased: bool,
}

/// The current effective configuration (default without a stack).
pub fn effective_config() -> NetConfig {
    get().map_or_else(NetConfig::default, |l| l.lock().cfg.clone())
}

pub struct Stack {
    pub iface: Interface,
    pub mux: Mux,
    pub sockets: SocketSet<'static>,
    pub ports: PortTable,
    /// Per-socket state, by socket id (also the bind-table owner id).
    pub socks: BTreeMap<u64, Sock>,
    pub next_id: u64,
    /// Ids of listening sockets (their pools are refilled per packet).
    pub listeners: Vec<u64>,
    /// Closed TCP connections finishing their shutdown: (handle, give-up
    /// deadline in ns).
    pub zombies: Vec<(smoltcp::iface::SocketHandle, u64)>,
    /// Static IPv4 default gateway (`ip=`), for route checks.
    pub gw4: Option<Ipv4Addr>,
    /// When the net task will next poll on its own (ns).
    pub task_deadline: u64,
    /// The DHCPv4 client socket (`ip=dhcp`), kept for renewals.
    pub dhcp: Option<SocketHandle>,
    /// When the DHCP client started (ns), for the lease log line.
    pub dhcp_t0: u64,
    /// Effective configuration (see [`NetConfig`]).
    pub cfg: NetConfig,
    /// DNS servers from `ip=` (used when a lease names none).
    pub static_dns: Vec<Ipv4Addr>,
}

/// `ip=` asked for DHCP (whether or not there is a NIC to run it on).
static DHCP_REQUESTED: AtomicBool = AtomicBool::new(false);

static STACK: OnceCell<SpinLock<Stack>> = OnceCell::new();
static LOCAL_EVENT: Event = Event::new();
static EVENT: OnceCell<&'static Event> = OnceCell::new();

pub fn now() -> Instant {
    Instant::from_micros((crate::arch::timer::now_ns() / 1000) as i64)
}

/// The stack, once [`init`] ran.
pub fn get() -> Option<&'static SpinLock<Stack>> {
    STACK.get()
}

fn event() -> &'static Event {
    EVENT.get().copied().unwrap_or(&LOCAL_EVENT)
}

/// Run `f` on the stack; then, if `poll`, run the interface. Either way,
/// readiness changes are collected and fired after the lock is released,
/// and the net task is woken if it should poll earlier than planned.
pub fn with<R>(poll: bool, f: impl FnOnce(&mut Stack) -> R) -> R {
    let lock = get().expect("net: stack not initialised");
    let (r, notify, kick) = {
        let mut s = lock.lock();
        let r = f(&mut s);
        let mut kick = false;
        if poll {
            s.poll_all();
            kick = s.next_deadline() < s.task_deadline;
        }
        let n = s.refresh_all();
        (r, n, kick)
    };
    notify.fire();
    if kick {
        event().signal();
    }
    r
}

impl Stack {
    /// Run the interface until the loopback queue is empty (bounded), then
    /// update socket bookkeeping (`inet::track`).
    pub fn poll_all(&mut self) {
        let now = now();
        self.iface.poll_maintenance(now);
        for _ in 0..MAX_ROUNDS {
            for _ in 0..MAX_INGRESS {
                match self
                    .iface
                    .poll_ingress_single(now, &mut self.mux, &mut self.sockets)
                {
                    PollIngressSingleResult::None => break,
                    _ => self.refill_listeners(),
                }
            }
            while self
                .iface
                .poll_egress(now, &mut self.mux, &mut self.sockets)
                != PollResult::None
            {}
            if !self.mux.lo_pending() {
                break;
            }
        }
        self.dhcp_events();
        self.track(crate::arch::timer::now_ns());
    }

    /// Apply a DHCP client event (lease acquired, renewed with changes, or
    /// lost). Runs after every poll, so whoever polls applies it.
    fn dhcp_events(&mut self) {
        let Some(h) = self.dhcp else {
            return;
        };
        let lease = match self.sockets.get_mut::<dhcpv4::Socket>(h).poll() {
            None => return,
            Some(dhcpv4::Event::Deconfigured) => None,
            Some(dhcpv4::Event::Configured(c)) => {
                let (hostname, domain) = c.packet.as_ref().map_or((None, None), dhcp_names);
                Some(NetConfig {
                    addr: Some((c.address.address(), c.address.prefix_len())),
                    gateway: c.router,
                    dns: c.dns_servers.iter().copied().collect(),
                    hostname,
                    domain,
                    leased: true,
                })
            }
        };
        match lease {
            Some(mut new) => {
                if new.dns.is_empty() {
                    new.dns = self.static_dns.clone();
                }
                if new == self.cfg {
                    return; // a renewal with nothing new
                }
                let first = !self.cfg.leased;
                self.set_v4(new.addr, new.gateway);
                let ms = crate::arch::timer::now_ns().saturating_sub(self.dhcp_t0) / 1_000_000;
                let mut dns = String::new();
                for d in &new.dns {
                    if !dns.is_empty() {
                        dns.push(' ');
                    }
                    dns.push_str(&format!("{}", d));
                }
                let (a, p) = new.addr.unwrap_or((Ipv4Addr::UNSPECIFIED, 0));
                crate::kprintln!(
                    "[patina] net: dhcp {} {}/{} via {}, dns {}{}{} ({} ms)",
                    if first { "lease" } else { "lease changed:" },
                    a,
                    p,
                    new.gateway
                        .map_or(String::from("none"), |g| format!("{}", g)),
                    if dns.is_empty() { "none" } else { &dns },
                    new.hostname
                        .as_ref()
                        .map_or(String::new(), |h| format!(", host {}", h)),
                    new.domain
                        .as_ref()
                        .map_or(String::new(), |d| format!(", domain {}", d)),
                    ms
                );
                self.cfg = new;
            }
            None => {
                // Also the client's first event, before any lease.
                if self.cfg.leased {
                    crate::kprintln!("[patina] net: dhcp lease lost, no IPv4 address");
                    self.set_v4(None, None);
                    self.cfg = NetConfig {
                        dns: self.static_dns.clone(),
                        ..NetConfig::default()
                    };
                }
            }
        }
    }

    /// Replace the interface's (non-loopback) IPv4 address and default
    /// route.
    fn set_v4(&mut self, addr: Option<(Ipv4Addr, u8)>, gw: Option<Ipv4Addr>) {
        self.iface.update_ip_addrs(|a| {
            a.retain(|c| !matches!(c, IpCidr::Ipv4(c) if !c.address().is_loopback()));
            if let Some((ip, p)) = addr {
                let _ = a.push(IpCidr::Ipv4(Ipv4Cidr::new(ip, p)));
            }
        });
        self.sync_own_addrs();
        self.iface.routes_mut().remove_default_ipv4_route();
        self.gw4 = None;
        if let Some(g) = gw
            && self.iface.routes_mut().add_default_ipv4_route(g).is_ok()
        {
            self.gw4 = Some(g);
        }
    }

    /// When the interface wants to be polled next (ns, capped).
    pub fn next_deadline(&mut self) -> u64 {
        let now_ns = crate::arch::timer::now_ns();
        if self.mux.lo_pending() {
            return now_ns;
        }
        let wait = match self.iface.poll_delay(now(), &self.sockets) {
            Some(d) => (d.total_micros().saturating_mul(1000)).min(MAX_WAIT_NS),
            None => MAX_WAIT_NS,
        };
        now_ns + wait
    }

    /// Recompute every socket's readiness; notify hooks of increases.
    pub fn refresh_all(&mut self) -> Notify {
        let mut n = Notify::NONE;
        for s in self.socks.values_mut() {
            n = n.and(s.refresh(&self.sockets));
        }
        n
    }

    pub fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Is `ip` one of the interface's addresses? Only 127.0.0.1 of 127/8:
    /// smoltcp accepts packets for its exact addresses, so binding or
    /// connecting to 127.0.0.2 fails at once (`EADDRNOTAVAIL` /
    /// `ENETUNREACH`) instead of hanging.
    pub fn is_local(&self, ip: core::net::IpAddr) -> bool {
        self.iface.has_ip_addr(ip)
    }

    /// Can we send to `ip` at all (a local address, a directly connected
    /// subnet or a default route, on an interface that exists)?
    pub fn routable(&self, ip: core::net::IpAddr) -> bool {
        if self.is_local(ip) {
            return true;
        }
        if !self.mux.has_nic() {
            return false;
        }
        match ip {
            core::net::IpAddr::V4(a) => {
                self.gw4.is_some()
                    || self.iface.ip_addrs().iter().any(|c| match c {
                        IpCidr::Ipv4(c) => !c.address().is_loopback() && c.contains_addr(&a),
                        _ => false,
                    })
            }
            core::net::IpAddr::V6(a) => {
                (a.segments()[0] & 0xffc0) == 0xfe80
                    || self.iface.routes().get_default_ipv6_route().is_some()
            }
        }
    }

    fn sync_own_addrs(&mut self) {
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        for c in self.iface.ip_addrs() {
            match c {
                IpCidr::Ipv4(c) => v4.push(c.address()),
                IpCidr::Ipv6(c) => v6.push(c.address()),
            }
        }
        self.mux.set_own(v4, v6);
    }

    /// Add an address to the interface (no-op when the table is full).
    pub fn add_addr(&mut self, cidr: IpCidr) {
        self.iface.update_ip_addrs(|a| {
            if !a.contains(&cidr) {
                let _ = a.push(cidr);
            }
        });
        self.sync_own_addrs();
    }
}

/// Host name (option 12) and domain name (option 15) of a DHCP packet.
fn dhcp_names(p: &smoltcp::wire::DhcpPacket<&[u8]>) -> (Option<String>, Option<String>) {
    let text = |d: &[u8]| {
        core::str::from_utf8(d)
            .ok()
            .map(|t| t.trim_end_matches('\0'))
            .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_graphic()))
            .map(String::from)
    };
    let mut host = None;
    let mut dom = None;
    for o in p.options() {
        match o.kind {
            12 => host = text(o.data),
            15 => dom = text(o.data),
            _ => {}
        }
    }
    (host, dom)
}

fn mac_str(m: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    )
}

/// EUI-64 link-local address for `mac`.
fn link_local(mac: &[u8; 6]) -> Ipv6Addr {
    Ipv6Addr::new(
        0xfe80,
        0,
        0,
        0,
        u16::from_be_bytes([mac[0] ^ 0x02, mac[1]]),
        u16::from_be_bytes([mac[2], 0xff]),
        u16::from_be_bytes([0xfe, mac[3]]),
        u16::from_be_bytes([mac[4], mac[5]]),
    )
}

/// Bring up the stack: claim the NIC registered by `drivers::init` (if
/// any), configure loopback and the static `ip=` address, start the net
/// task. Called from `kernel_main` after `drivers::init`, before the
/// loader starts thread 0.
pub fn init(cmdline: &patina_cmdline::Cmdline) {
    let nic = super::device::take();
    let mac = nic.as_ref().map_or(LO_MAC, |n| n.mac());
    let nic_name = nic.as_ref().map(|n| n.name());
    let mut mux = Mux::new(nic, mac);

    let mut seed = [0u8; 8];
    crate::random::fill(&mut seed);
    let seed = u64::from_ne_bytes(seed);
    let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    cfg.random_seed = seed;
    let iface = Interface::new(cfg, &mut mux, now());

    let mut s = Stack {
        iface,
        mux,
        sockets: SocketSet::new(Vec::new()),
        ports: PortTable::new(seed.rotate_left(17)),
        socks: BTreeMap::new(),
        next_id: 0,
        listeners: Vec::new(),
        zombies: Vec::new(),
        gw4: None,
        task_deadline: 0,
        dhcp: None,
        dhcp_t0: 0,
        cfg: NetConfig::default(),
        static_dns: Vec::new(),
    };
    s.add_addr(IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Addr::LOCALHOST, 8)));
    s.add_addr(IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Addr::LOCALHOST, 128)));
    if s.mux.has_nic() {
        s.add_addr(IpCidr::Ipv6(Ipv6Cidr::new(link_local(&mac), 64)));
    }

    let mut dhcp = false;
    if let Some(ip) = &cmdline.ip {
        s.static_dns = ip.dns.iter().flatten().copied().collect();
        s.cfg.dns = s.static_dns.clone();
        if ip.is_static() {
            if let Some(client) = ip.client {
                let prefix = ip.prefix_len().unwrap_or(24);
                s.add_addr(IpCidr::Ipv4(Ipv4Cidr::new(client, prefix)));
                s.cfg.addr = Some((client, prefix));
            }
            if let Some(gw) = ip.gateway
                && s.iface.routes_mut().add_default_ipv4_route(gw).is_ok()
            {
                s.gw4 = Some(gw);
                s.cfg.gateway = Some(gw);
            }
        } else if ip.autoconf.wants_dhcp()
            || (ip.client.is_none() && ip.autoconf == patina_cmdline::Autoconf::Unspecified)
        {
            dhcp = true;
        }
    }

    // The boot line: hardware address, addresses, gateway.
    let mut addrs = String::new();
    for c in s.iface.ip_addrs() {
        if !addrs.is_empty() {
            addrs.push(' ');
        }
        addrs.push_str(&format!("{}", c));
    }
    match nic_name {
        Some(name) => crate::kprintln!(
            "[patina] net: {} hwaddr {}, addrs {}, gateway {}{}",
            name,
            mac_str(&mac),
            addrs,
            s.gw4.map_or(String::from("none"), |g| format!("{}", g)),
            if dhcp { ", dhcp requested" } else { "" }
        ),
        None => crate::kprintln!(
            "[patina] net: no device, loopback only (hwaddr {}, addrs {})",
            mac_str(&mac),
            addrs
        ),
    }

    let ev = s.mux.nic_event();
    let has_nic = s.mux.has_nic();
    if let Some(ev) = ev {
        let _ = EVENT.set(ev);
    }
    if dhcp && has_nic {
        let mut sock = dhcpv4::Socket::new();
        let mut rc = dhcpv4::RetryConfig::default();
        // Retransmit quickly while booting (smoltcp waits 10 s / 5 s).
        rc.discover_timeout = Duration::from_secs(1);
        rc.initial_request_timeout = Duration::from_secs(1);
        sock.set_retry_config(rc);
        sock.set_parameter_request_list(&DHCP_PARAMS);
        // The last received packet, for the host/domain name options.
        sock.set_receive_packet_buffer(alloc::vec![0u8; 1500].leak());
        s.dhcp = Some(s.sockets.add(sock));
        s.dhcp_t0 = crate::arch::timer::now_ns();
    }
    DHCP_REQUESTED.store(dhcp, Ordering::Relaxed);
    if STACK.set(SpinLock::new(s)).is_err() {
        return;
    }
    crate::drivers::spawn(task);
}

/// `ip=dhcp` (or `ip=` without a client address): wait, at boot and before
/// user mode, until the DHCP client has a lease or [`DHCP_WAIT_NS`] passed.
/// Called from `kernel_main` after `ctl::init`, so a timeout is also
/// reported as a control-channel log event. Interrupts are not needed: the
/// NIC's `recv` polls its used ring. The client socket stays in the set;
/// whoever polls afterwards (the net task, socket syscalls) applies
/// renewals and losses ([`Stack::poll_all`]).
pub fn wait_dhcp() {
    if !DHCP_REQUESTED.load(Ordering::Relaxed) {
        return;
    }
    let Some(lock) = get() else {
        return;
    };
    if lock.lock().dhcp.is_none() {
        crate::kprintln!("[patina] net: ip=dhcp: no network device, continuing without an address");
        crate::ctl::log_warn(format_args!(
            "net: ip=dhcp: no network device, continuing without an address"
        ));
        return;
    }
    let t0 = crate::arch::timer::now_ns();
    loop {
        let (leased, next) = {
            let mut s = lock.lock();
            s.poll_all();
            (s.cfg.leased, s.next_deadline())
        };
        if leased {
            return;
        }
        let now = crate::arch::timer::now_ns();
        if now.saturating_sub(t0) >= DHCP_WAIT_NS {
            break;
        }
        // Poll at least every millisecond (replies are found by polling).
        let until = next.min(now + 1_000_000).min(t0 + DHCP_WAIT_NS);
        while crate::arch::timer::now_ns() < until {
            core::hint::spin_loop();
        }
    }
    crate::kprintln!("[patina] net: dhcp: no lease after 10 s, continuing without an address");
    crate::ctl::log_warn(format_args!(
        "net: dhcp: no lease after 10 s, continuing without an address"
    ));
}

/// The net task: poll, notify, sleep until the next deadline or an event.
fn task() {
    let lock = get().expect("net: no stack");
    let ev = event();
    let mut spins = 0u32;
    loop {
        let (notify, mut deadline) = {
            let mut s = lock.lock();
            s.poll_all();
            let n = s.refresh_all();
            let d = s.next_deadline();
            s.task_deadline = d;
            (n, d)
        };
        notify.fire();
        let now = crate::arch::timer::now_ns();
        if deadline <= now {
            // Something wants polling right away; do not spin forever.
            spins += 1;
            if spins < 8 {
                continue;
            }
            deadline = now + 1_000_000;
        }
        spins = 0;
        ev.wait(Some(deadline));
    }
}
