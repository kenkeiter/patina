//! virtio-console (virtio 1.2 §5.3) with multiport support (decision D61).
//!
//! Queues: port 0 rx/tx = 0/1, control rx/tx = 2/3, port n >= 1 rx/tx =
//! 2n+2 / 2n+3. Ports 0..[`MAX_PORTS`] are set up (QEMU numbers a
//! `virtconsole` as port 0 and `virtserialport`s from 1). Handshake (boot,
//! polled): DEVICE_READY → the device sends DEVICE_ADD per port → we
//! answer PORT_READY → it sends PORT_NAME / CONSOLE_PORT / PORT_OPEN (the
//! host side of the port is connected: for a socket chardev, a client is
//! attached). We send PORT_OPEN for console ports and named ports.
//! Without `VIRTIO_CONSOLE_F_MULTIPORT` there is just port 0, a console.
//!
//! Users:
//! * the control channel (`ctl`) finds the port named `patina.ctl`
//!   ([`find_port`]), reads with [`service`] from its kernel thread (woken
//!   by the device interrupt through [`EVENT`]) and writes with [`write`];
//! * `hvc0`: with `console=hvc0` the first console port becomes the kernel
//!   console ([`Hvc`]), replacing the early UART/ring
//!   (`console::switch_to`).
//!
//! Transmit is synchronous and bounded: copy into the port's DMA buffer,
//! notify, spin until the device returns it (QEMU completes during the
//! notify for a ready backend). If the device does not return it in time
//! the buffer stays in flight and later writes are dropped until it comes
//! back, so a host that stops reading slows nothing down. Writes work with
//! IRQs masked, from any CPU and (with force-unlocking) from the panic /
//! exit path.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::queue::{Buf, DmaBuf, VirtQueue};
use super::{DEVICE_CONSOLE, Transport, attach_irq, driver_ok, negotiate, poll_until};
use crate::drivers::Event;
use crate::sync::{OnceCell, SpinLock, SpinLockGuard};

const F_MULTIPORT: u64 = 1 << 1;

pub const MAX_PORTS: usize = 4;
const RX_BUFS: usize = 8;
const RX_BUF_SIZE: usize = 512;
const TX_BUF_SIZE: usize = 4096;

const VIRTIO_CONSOLE_DEVICE_READY: u16 = 0;
const VIRTIO_CONSOLE_DEVICE_ADD: u16 = 1;
const VIRTIO_CONSOLE_DEVICE_REMOVE: u16 = 2;
const VIRTIO_CONSOLE_PORT_READY: u16 = 3;
const VIRTIO_CONSOLE_CONSOLE_PORT: u16 = 4;
const VIRTIO_CONSOLE_PORT_OPEN: u16 = 6;
const VIRTIO_CONSOLE_PORT_NAME: u16 = 7;

/// A receive queue with `RX_BUFS` posted buffers.
struct RxQ {
    q: VirtQueue,
    buf: DmaBuf,
    /// Buffer slot of each descriptor head.
    slot_of: [u8; 256],
}

impl RxQ {
    fn new(t: &dyn Transport, index: u16) -> Option<RxQ> {
        let mut r = RxQ {
            q: VirtQueue::new(t, index, RX_BUFS as u16)?,
            buf: DmaBuf::new(RX_BUFS * RX_BUF_SIZE)?,
            slot_of: [0; 256],
        };
        let n = (r.q.size() as usize).min(RX_BUFS);
        for s in 0..n {
            r.post(s);
        }
        Some(r)
    }

    fn post(&mut self, slot: usize) {
        let b = Buf {
            addr: self.buf.pa(slot * RX_BUF_SIZE),
            len: RX_BUF_SIZE as u32,
            write: true,
        };
        if let Some(h) = self.q.add(&[b]) {
            self.slot_of[h as usize] = slot as u8;
        }
    }

    /// Pop every received buffer, pass its bytes to `f`, repost it.
    /// Returns whether anything was popped (the caller kicks).
    fn drain(&mut self, f: &mut dyn FnMut(&[u8])) -> bool {
        let mut any = false;
        let mut tmp = [0u8; RX_BUF_SIZE];
        while let Some((h, len)) = self.q.pop_used() {
            let slot = self.slot_of[h as usize] as usize;
            let n = (len as usize).min(RX_BUF_SIZE);
            self.buf.read(slot * RX_BUF_SIZE, &mut tmp[..n]);
            self.post(slot);
            f(&tmp[..n]);
            any = true;
        }
        any
    }
}

/// A transmit queue with one DMA buffer; `pending` bytes are staged in it
/// until [`TxQ::flush`].
struct TxQ {
    q: VirtQueue,
    buf: DmaBuf,
    pending: usize,
    inflight: bool,
    /// Bytes dropped because the device did not take them.
    dropped: u64,
}

impl TxQ {
    fn new(t: &dyn Transport, index: u16) -> Option<TxQ> {
        let mut q = VirtQueue::new(t, index, 4)?;
        q.set_no_interrupt(true);
        Some(TxQ {
            q,
            buf: DmaBuf::new(TX_BUF_SIZE)?,
            pending: 0,
            inflight: false,
            dropped: 0,
        })
    }

    /// Collect a completed transmission. True if the buffer is free.
    fn reap(&mut self) -> bool {
        if self.inflight && self.q.pop_used().is_some() {
            self.inflight = false;
        }
        !self.inflight
    }

    /// Send the staged bytes and wait up to `timeout_ns` for the device to
    /// return the buffer. False if they were dropped / are still in flight.
    fn flush(&mut self, t: &dyn Transport, timeout_ns: u64) -> bool {
        if self.pending == 0 {
            return true;
        }
        if !self.reap() {
            self.dropped += self.pending as u64;
            self.pending = 0;
            return false;
        }
        let b = Buf {
            addr: self.buf.pa(0),
            len: self.pending as u32,
            write: false,
        };
        if self.q.add(&[b]).is_none() {
            self.dropped += self.pending as u64;
            self.pending = 0;
            return false;
        }
        self.pending = 0;
        self.inflight = true;
        self.q.kick(t);
        poll_until(timeout_ns, || self.reap())
    }

    /// Stage `data`, flushing whenever the buffer fills.
    fn write(&mut self, t: &dyn Transport, mut data: &[u8], timeout_ns: u64) -> bool {
        let mut ok = true;
        while !data.is_empty() {
            if self.pending == TX_BUF_SIZE {
                ok &= self.flush(t, timeout_ns);
            }
            if !self.reap() {
                // The device still owns the buffer: drop.
                self.dropped += data.len() as u64;
                return false;
            }
            let n = (TX_BUF_SIZE - self.pending).min(data.len());
            self.buf.write(self.pending, &data[..n]);
            self.pending += n;
            data = &data[n..];
        }
        ok
    }
}

struct Port {
    rx: SpinLock<Option<RxQ>>,
    tx: SpinLock<Option<TxQ>>,
    name: SpinLock<([u8; 32], usize)>,
    present: AtomicBool,
    console: AtomicBool,
    host_open: AtomicBool,
    guest_opened: AtomicBool,
}

impl Port {
    const fn new() -> Port {
        Port {
            rx: SpinLock::new(None),
            tx: SpinLock::new(None),
            name: SpinLock::new(([0; 32], 0)),
            present: AtomicBool::new(false),
            console: AtomicBool::new(false),
            host_open: AtomicBool::new(false),
            guest_opened: AtomicBool::new(false),
        }
    }
}

struct Console {
    t: Box<dyn Transport>,
    multiport: bool,
    ctrl_rx: SpinLock<Option<RxQ>>,
    ctrl_tx: SpinLock<Option<TxQ>>,
    ports: [Port; MAX_PORTS],
}

static DEV: OnceCell<Console> = OnceCell::new();
/// Signalled by the device interrupt; the control channel thread waits on it.
pub static EVENT: Event = Event::new();
/// Bumped whenever a port's host-open state changes.
pub static OPEN_GEN: AtomicU32 = AtomicU32::new(0);

fn rx_queue(port: usize) -> u16 {
    if port == 0 { 0 } else { (2 * port + 2) as u16 }
}

fn tx_queue(port: usize) -> u16 {
    rx_queue(port) + 1
}

/// Lock for the transmit path: normally a plain lock; once the VM is going
/// down (exit/panic owner, other CPUs stopped) a holder may never release
/// it, so wait a bounded time and then break the lock.
fn lock_tx<T>(l: &SpinLock<T>) -> SpinLockGuard<'_, T> {
    if !crate::task::sched::exiting() {
        return l.lock();
    }
    for _ in 0..1_000_000 {
        if let Some(g) = l.try_lock() {
            return g;
        }
        core::hint::spin_loop();
    }
    // SAFETY: only on the no-return exit/panic path, as console.rs does.
    unsafe { l.force_unlock() };
    l.lock()
}

fn irq() {
    if let Some(d) = DEV.get() {
        d.t.ack_interrupt();
    }
    EVENT.signal();
}

/// Claim and start the first virtio-console device, if any.
pub fn init(cmdline: &patina_cmdline::Cmdline) {
    let Some(dev) = super::take(DEVICE_CONSOLE) else {
        return;
    };
    let t = dev.transport;
    let feats = match negotiate(&*t, F_MULTIPORT) {
        Ok(f) => f,
        Err(e) => {
            crate::kprintln!("[patina] virtio-console: {}", e);
            return;
        }
    };
    let multiport = feats & F_MULTIPORT != 0;
    let max_ports = if multiport {
        (t.config_read32(4) as usize).clamp(1, MAX_PORTS)
    } else {
        1
    };
    let c = Console {
        t,
        multiport,
        ctrl_rx: SpinLock::new(None),
        ctrl_tx: SpinLock::new(None),
        ports: [const { Port::new() }; MAX_PORTS],
    };
    for p in 0..max_ports {
        *c.ports[p].rx.lock() = RxQ::new(&*c.t, rx_queue(p));
        *c.ports[p].tx.lock() = TxQ::new(&*c.t, tx_queue(p));
    }
    if multiport {
        *c.ctrl_rx.lock() = RxQ::new(&*c.t, 2);
        *c.ctrl_tx.lock() = TxQ::new(&*c.t, 3);
    } else {
        c.ports[0].present.store(true, Ordering::Relaxed);
        c.ports[0].console.store(true, Ordering::Relaxed);
    }
    driver_ok(&*c.t);
    for p in 0..max_ports {
        if let Some(r) = c.ports[p].rx.lock().as_ref() {
            r.q.kick(&*c.t);
        }
    }
    if let Some(r) = c.ctrl_rx.lock().as_ref() {
        r.q.kick(&*c.t);
    }
    let Ok(c) = DEV.set(c) else {
        return; // a second console device: the first one wins
    };
    if multiport {
        control_send(c, u32::MAX, VIRTIO_CONSOLE_DEVICE_READY, 1);
        // The device answers during the notify (QEMU) or shortly after;
        // stop once 20 ms pass without a new control message.
        let t0 = crate::arch::timer::now_ns();
        let mut last = t0;
        loop {
            if service_control(c) {
                last = crate::arch::timer::now_ns();
            }
            let now = crate::arch::timer::now_ns();
            if now - last > 20_000_000 || now - t0 > 200_000_000 {
                break;
            }
            core::hint::spin_loop();
        }
    }
    if let Some(line) = dev.irq {
        attach_irq(line, irq);
    }
    let mut names = alloc::string::String::new();
    for (i, p) in c.ports.iter().enumerate() {
        if p.present.load(Ordering::Relaxed) {
            let n = p.name.lock();
            names += &alloc::format!(
                " {}{}{}{}",
                i,
                if n.1 > 0 { "=" } else { "" },
                core::str::from_utf8(&n.0[..n.1]).unwrap_or("?"),
                if p.console.load(Ordering::Relaxed) {
                    "(console)"
                } else {
                    ""
                }
            );
        }
    }
    crate::kprintln!(
        "[patina] virtio-console: {}, ports:{}",
        if multiport {
            "multiport"
        } else {
            "single port"
        },
        if names.is_empty() { " none" } else { &names }
    );
    if cmdline.primary_console().is_some_and(|(n, _)| n == "hvc0") {
        match c
            .ports
            .iter()
            .position(|p| p.present.load(Ordering::Relaxed) && p.console.load(Ordering::Relaxed))
        {
            Some(p) => {
                HVC_PORT.store(p as u32, Ordering::Relaxed);
                crate::console::switch_to(&HVC);
                crate::kprintln!("[patina] console: hvc0 (virtio-console port {})", p);
            }
            None => crate::kprintln!("[patina] console: console=hvc0 but no virtio console port"),
        }
    }
}

fn control_send(c: &Console, id: u32, event: u16, value: u16) {
    let mut m = [0u8; 8];
    m[..4].copy_from_slice(&id.to_le_bytes());
    m[4..6].copy_from_slice(&event.to_le_bytes());
    m[6..].copy_from_slice(&value.to_le_bytes());
    let mut g = lock_tx(&c.ctrl_tx);
    if let Some(tx) = g.as_mut() {
        tx.write(&*c.t, &m, 50_000_000);
        tx.flush(&*c.t, 50_000_000);
    }
}

fn port_open(c: &Console, id: usize) {
    if !c.ports[id].guest_opened.swap(true, Ordering::Relaxed) {
        control_send(c, id as u32, VIRTIO_CONSOLE_PORT_OPEN, 1);
    }
}

/// Process pending control messages. True if there were any.
fn service_control(c: &Console) -> bool {
    // Collect messages first: replies take other locks.
    let mut msgs: [([u8; 8 + 32], usize); RX_BUFS] = [([0; 40], 0); RX_BUFS];
    let mut n = 0;
    {
        let mut g = c.ctrl_rx.lock();
        let Some(rx) = g.as_mut() else {
            return false;
        };
        let popped = rx.drain(&mut |b: &[u8]| {
            if n < RX_BUFS {
                let k = b.len().min(40);
                msgs[n].0[..k].copy_from_slice(&b[..k]);
                msgs[n].1 = k;
                n += 1;
            }
        });
        if popped {
            rx.q.kick(&*c.t);
        }
    }
    for (m, len) in &msgs[..n] {
        if *len < 8 {
            continue;
        }
        let id = u32::from_le_bytes([m[0], m[1], m[2], m[3]]) as usize;
        let event = u16::from_le_bytes([m[4], m[5]]);
        let value = u16::from_le_bytes([m[6], m[7]]);
        let Some(p) = c.ports.get(id) else {
            if event == VIRTIO_CONSOLE_DEVICE_ADD {
                control_send(c, id as u32, VIRTIO_CONSOLE_PORT_READY, 0);
            }
            continue;
        };
        match event {
            VIRTIO_CONSOLE_DEVICE_ADD => {
                let ok = p.tx.lock().is_some();
                p.present.store(ok, Ordering::Relaxed);
                control_send(c, id as u32, VIRTIO_CONSOLE_PORT_READY, ok as u16);
            }
            VIRTIO_CONSOLE_DEVICE_REMOVE => {
                p.present.store(false, Ordering::Relaxed);
                p.host_open.store(false, Ordering::Relaxed);
                OPEN_GEN.fetch_add(1, Ordering::Relaxed);
            }
            VIRTIO_CONSOLE_CONSOLE_PORT => {
                p.console.store(true, Ordering::Relaxed);
                port_open(c, id);
            }
            VIRTIO_CONSOLE_PORT_NAME => {
                let name = &m[8..*len];
                let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
                let mut g = p.name.lock();
                let k = name.len().min(32);
                g.0[..k].copy_from_slice(&name[..k]);
                g.1 = k;
                drop(g);
                port_open(c, id);
            }
            VIRTIO_CONSOLE_PORT_OPEN => {
                p.host_open.store(value != 0, Ordering::Relaxed);
                OPEN_GEN.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
    n > 0
}

/// Whether a virtio-console device is driven.
pub fn present() -> bool {
    DEV.get().is_some()
}

/// Port number of the port named `name`.
pub fn find_port(name: &str) -> Option<usize> {
    let c = DEV.get()?;
    c.ports.iter().position(|p| {
        let n = p.name.lock();
        p.present.load(Ordering::Relaxed) && &n.0[..n.1] == name.as_bytes()
    })
}

/// Whether the host side of `port` is connected (PORT_OPEN 1 seen).
pub fn host_open(port: usize) -> bool {
    DEV.get()
        .and_then(|c| c.ports.get(port))
        .is_some_and(|p| p.host_open.load(Ordering::Relaxed))
}

/// Process control messages and received data (thread context or
/// polling); `on_data(port, bytes)` gets the data of every port.
pub fn service(on_data: &mut dyn FnMut(usize, &[u8])) {
    let Some(c) = DEV.get() else {
        return;
    };
    if c.multiport {
        service_control(c);
    }
    for (i, p) in c.ports.iter().enumerate() {
        let mut g = p.rx.lock();
        if let Some(rx) = g.as_mut()
            && rx.drain(&mut |b: &[u8]| on_data(i, b))
        {
            rx.q.kick(&*c.t);
        }
    }
}

/// Write `data` to `port` and wait up to `timeout_ns` for the device to
/// take it. False if (part of) it was dropped.
pub fn write(port: usize, data: &[u8], timeout_ns: u64) -> bool {
    let Some(c) = DEV.get() else {
        return false;
    };
    let Some(p) = c.ports.get(port) else {
        return false;
    };
    let mut g = lock_tx(&p.tx);
    match g.as_mut() {
        Some(tx) => tx.write(&*c.t, data, timeout_ns) & tx.flush(&*c.t, timeout_ns),
        None => false,
    }
}

static HVC_PORT: AtomicU32 = AtomicU32::new(0);

/// The `hvc0` kernel console: virtio-console port [`HVC_PORT`].
pub struct Hvc;
pub static HVC: Hvc = Hvc;

/// Per-write wait for the device (QEMU takes console output synchronously).
const HVC_TIMEOUT_NS: u64 = 100_000_000;

impl crate::console::Console for Hvc {
    fn write_bytes(&self, bytes: &[u8]) {
        let Some(c) = DEV.get() else {
            return;
        };
        let p = &c.ports[HVC_PORT.load(Ordering::Relaxed) as usize];
        let mut g = lock_tx(&p.tx);
        if let Some(tx) = g.as_mut() {
            tx.write(&*c.t, bytes, HVC_TIMEOUT_NS);
        }
    }

    fn flush(&self) {
        let Some(c) = DEV.get() else {
            return;
        };
        let p = &c.ports[HVC_PORT.load(Ordering::Relaxed) as usize];
        let mut g = lock_tx(&p.tx);
        if let Some(tx) = g.as_mut() {
            tx.flush(&*c.t, HVC_TIMEOUT_NS);
        }
    }
}
