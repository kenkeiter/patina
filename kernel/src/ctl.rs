//! Control channel (design §10, decision D63): NDJSON between the host and
//! the kernel over virtio-vsock port 4242 or the virtio-console port
//! `patina.ctl`, whichever is present (vsock preferred).
//!
//! * Requests are handled by one kernel thread ([`ready`] starts it just
//!   before thread 0 enters user mode). It sleeps on the transport's
//!   interrupt event (plus a 250 ms poll as a safety net), feeds received
//!   bytes through `patina_ctlproto::LineFramer` and answers: `ping` →
//!   `pong`; `signal` → `send_to_process` (`SI_USER` from pid 1) for the
//!   host-injectable set; `stop` → SIGTERM plus a grace timer whose expiry
//!   forces the exit (`code 137, forced`); `time` → steps the wall clock;
//!   `dump` → SIGQUIT. Bad lines get the `DecodeError::reply()` event.
//! * Events (`ready`, `exit`, `panic`, `log`, replies) are encoded into a
//!   stack buffer and appended to a 4 KiB pending buffer, which is written
//!   to the transport whenever a host is connected (the console port's
//!   PORT_OPEN, an accepted vsock connection) — so `ready` reaches a host
//!   that connects after boot. Events that do not fit are dropped.
//! * `exit` (from `crate::exit_with`) is written synchronously and waited
//!   for (bounded ~100 ms) before the sentinel and poweroff; `panic` is
//!   emitted from the panic handler. Both may run on a CPU that stopped
//!   the others, so every lock on that path is taken with a bounded wait
//!   and broken if a stopped CPU holds it (`sched::exiting`).
//!
//! A forced stop races a normal exit through the `EXIT_OWNER` protocol
//! (`sched::stop_other_cpus`): whichever comes first reports its exit, the
//! other CPU halts.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use patina_abi::signal::{SI_USER, SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2, SigInfo};
use patina_ctlproto::{Event, Frame, LineFramer, MAX_LINE, Op, SignalSpec, decode_request};

use crate::drivers::virtio::{console, vsock};
use crate::sync::{SpinLock, SpinLockGuard};

const T_NONE: u8 = 0;
const T_CONSOLE: u8 = 1;
const T_VSOCK: u8 = 2;

static TRANSPORT: AtomicU8 = AtomicU8::new(T_NONE);
static PORT: AtomicU64 = AtomicU64::new(0);
static MEM_MB: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);

const PENDING_CAP: usize = 4096;
static PENDING: SpinLock<([u8; PENDING_CAP], usize)> = SpinLock::new(([0; PENDING_CAP], 0));

/// Signals the host may inject (design §6).
const INJECTABLE: [i32; 6] = [SIGTERM, SIGINT, SIGHUP, SIGUSR1, SIGUSR2, SIGQUIT];

/// Pick the transport (after `drivers::init`). `mem_mb` is reported in
/// the `ready` event.
pub fn init(mem_mb: u64) {
    MEM_MB.store(mem_mb, Ordering::Relaxed);
    if vsock::present() {
        vsock::listen(patina_ctlproto::VSOCK_PORT);
        TRANSPORT.store(T_VSOCK, Ordering::Relaxed);
        crate::kprintln!(
            "[patina] ctl: virtio-vsock port {}",
            patina_ctlproto::VSOCK_PORT
        );
    } else if let Some(p) = console::find_port(patina_ctlproto::CONSOLE_PORT_NAME) {
        PORT.store(p as u64, Ordering::Relaxed);
        TRANSPORT.store(T_CONSOLE, Ordering::Relaxed);
        crate::kprintln!(
            "[patina] ctl: virtio-console port {} \"{}\"{}",
            p,
            patina_ctlproto::CONSOLE_PORT_NAME,
            if console::host_open(p) {
                " (host connected)"
            } else {
                ""
            }
        );
    } else {
        crate::kprintln!("[patina] ctl: no transport");
    }
}

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
    // SAFETY: no-return exit/panic path (see the module docs).
    unsafe { l.force_unlock() };
    l.lock()
}

fn connected() -> bool {
    match TRANSPORT.load(Ordering::Relaxed) {
        T_CONSOLE => console::host_open(PORT.load(Ordering::Relaxed) as usize),
        T_VSOCK => vsock::connected(),
        _ => false,
    }
}

fn transport_write(b: &[u8], timeout_ns: u64) -> bool {
    match TRANSPORT.load(Ordering::Relaxed) {
        T_CONSOLE => console::write(PORT.load(Ordering::Relaxed) as usize, b, timeout_ns),
        T_VSOCK => vsock::write(b, timeout_ns),
        _ => false,
    }
}

/// Write out the pending buffer if a host is connected.
fn flush_pending(timeout_ns: u64) {
    if TRANSPORT.load(Ordering::Relaxed) == T_NONE || !connected() {
        return;
    }
    let mut p = lock(&PENDING);
    if p.1 > 0 {
        let n = p.1;
        // Even if the write was dropped, the bytes are gone: never resend
        // half-written lines.
        transport_write(&p.0[..n], timeout_ns);
        p.1 = 0;
    }
}

fn emit_with(ev: &Event<'_>, timeout_ns: u64) {
    if TRANSPORT.load(Ordering::Relaxed) == T_NONE {
        return;
    }
    let mut buf = [0u8; 1024];
    let Some(n) = ev.encode_to_slice(&mut buf) else {
        return;
    };
    {
        let mut p = lock(&PENDING);
        let at = p.1;
        if at + n <= PENDING_CAP {
            p.0[at..at + n].copy_from_slice(&buf[..n]);
            p.1 = at + n;
        }
    }
    flush_pending(timeout_ns);
}

/// Emit an event (any context; bounded wait).
pub fn emit(ev: &Event<'_>) {
    emit_with(ev, 50_000_000);
}

/// `ready`: called by the loader just before thread 0 enters user mode
/// (`go` from `.go.buildinfo`, "" if absent). Starts the control thread.
pub fn ready(go: &str) {
    emit(&Event::Ready {
        go,
        cpus: crate::arch::smp::cpu_count() as u32,
        mem_mb: MEM_MB.load(Ordering::Relaxed),
    });
    if TRANSPORT.load(Ordering::Relaxed) != T_NONE && !STARTED.swap(true, Ordering::Relaxed) {
        crate::task::spawn_kernel_thread(thread);
    }
}

/// A truncating `fmt::Write` into a fixed buffer.
struct Trunc<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl Write for Trunc<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            let mut tmp = [0u8; 4];
            let e = c.encode_utf8(&mut tmp);
            if self.len + e.len() > self.buf.len() {
                return Ok(());
            }
            self.buf[self.len..self.len + e.len()].copy_from_slice(e.as_bytes());
            self.len += e.len();
        }
        Ok(())
    }
}

/// Format `args` into `buf` (truncated at a character boundary).
fn format_into<'a>(buf: &'a mut [u8], args: fmt::Arguments<'_>) -> &'a str {
    let mut w = Trunc { buf, len: 0 };
    let _ = w.write_fmt(args);
    let n = w.len;
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

/// `{"ev":"log","level":"warn","msg":...}` (e.g. the ENOSYS-once log).
pub fn log_warn(args: fmt::Arguments<'_>) {
    log("warn", args);
}

/// `{"ev":"log","level":"info","msg":...}` (e.g. the DHCP lease).
pub fn log_info(args: fmt::Arguments<'_>) {
    log("info", args);
}

fn log(level: &str, args: fmt::Arguments<'_>) {
    if TRANSPORT.load(Ordering::Relaxed) == T_NONE {
        return;
    }
    let mut b = [0u8; 256];
    let msg = format_into(&mut b, args);
    emit_with(&Event::Log { level, msg }, 10_000_000);
}

/// `{"ev":"panic","msg":...}` from the panic handler (other CPUs stopped).
pub fn emit_panic(args: fmt::Arguments<'_>) {
    if TRANSPORT.load(Ordering::Relaxed) == T_NONE {
        return;
    }
    let mut b = [0u8; 768];
    let msg = format_into(&mut b, args);
    emit_with(&Event::Panic { msg }, 100_000_000);
}

/// The `exit` event, written and waited for before poweroff (bounded).
pub fn emit_exit(code: i32, signal: Option<&str>, forced: bool) {
    emit_with(
        &Event::Exit {
            code,
            signal,
            forced,
        },
        100_000_000,
    );
    if TRANSPORT.load(Ordering::Relaxed) == T_VSOCK {
        vsock::flush(100_000_000);
    }
}

// ---------------------------------------------------------------------------
// The control thread.

fn wait_event(deadline: u64) {
    match TRANSPORT.load(Ordering::Relaxed) {
        T_CONSOLE => console::EVENT.wait(Some(deadline)),
        T_VSOCK => vsock::EVENT.wait(Some(deadline)),
        _ => false,
    };
}

fn signal_number(spec: SignalSpec) -> Option<i32> {
    let n = match spec {
        SignalSpec::Name(s) => patina_abi::signal::from_name(s.as_str())?,
        SignalSpec::Number(n) => i32::try_from(n).ok()?,
    };
    INJECTABLE.contains(&n).then_some(n)
}

fn send_signal(sig: i32) {
    crate::task::signal::send_to_process(SigInfo::kill(sig, SI_USER, 1, 0));
}

fn force_stop() -> ! {
    crate::kprintln!("[patina] ctl: grace period over, forcing exit");
    crate::exit_with(137, None, true)
}

struct State {
    grace_deadline: Option<u64>,
}

fn handle_line(st: &mut State, line: &[u8]) {
    let req = match decode_request(line) {
        Ok(r) => r,
        Err(e) => {
            emit(&e.reply());
            return;
        }
    };
    match req.op {
        Op::Ping => emit(&Event::Pong {
            id: req.id,
            uptime_ms: crate::arch::timer::now_ns() / 1_000_000,
        }),
        Op::Signal(spec) => match signal_number(spec) {
            Some(sig) => send_signal(sig),
            None => emit(&Event::Error {
                id: req.id,
                msg: "signal not injectable",
                field: Some("sig"),
            }),
        },
        Op::Dump => send_signal(SIGQUIT),
        Op::Time { epoch_ns } => crate::wallclock::set_now(epoch_ns),
        Op::Stop { grace_ms } => {
            let grace = grace_ms.unwrap_or(patina_ctlproto::msg::DEFAULT_GRACE_MS);
            if grace == 0 {
                force_stop();
            }
            send_signal(SIGTERM);
            let d = crate::arch::timer::now_ns().saturating_add(grace.saturating_mul(1_000_000));
            st.grace_deadline = Some(st.grace_deadline.map_or(d, |old| old.min(d)));
        }
    }
}

fn thread() {
    let mut framer = LineFramer::<MAX_LINE>::new();
    let mut st = State {
        grace_deadline: None,
    };
    let mut was_connected = false;
    let mut conn_gen = vsock::CONN_GEN.load(Ordering::SeqCst);
    let port = PORT.load(Ordering::Relaxed) as usize;
    loop {
        let mut lines: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
        let mut too_long = false;
        let mut on_bytes = |b: &[u8]| {
            // A new vsock connection starts with a clean line buffer; lines
            // of the replaced connection are dropped.
            let g = vsock::CONN_GEN.load(Ordering::SeqCst);
            if g != conn_gen {
                conn_gen = g;
                framer.reset();
                lines.clear();
            }
            framer.feed(b, |f| match f {
                Frame::Line(l) => lines.push(l.to_vec()),
                Frame::TooLong => too_long = true,
            })
        };
        match TRANSPORT.load(Ordering::Relaxed) {
            T_CONSOLE => console::service(&mut |p, b| {
                if p == port {
                    on_bytes(b);
                }
            }),
            T_VSOCK => vsock::service(&mut on_bytes),
            _ => return,
        }
        let c = connected();
        if c && !was_connected {
            flush_pending(50_000_000);
        }
        was_connected = c;
        if too_long {
            emit(&Event::Error {
                id: None,
                msg: "line too long",
                field: None,
            });
        }
        for l in &lines {
            handle_line(&mut st, l);
        }
        let now = crate::arch::timer::now_ns();
        if let Some(d) = st.grace_deadline
            && now >= d
        {
            force_stop();
        }
        let mut deadline = now + 250_000_000;
        if let Some(d) = st.grace_deadline {
            deadline = deadline.min(d);
        }
        wait_event(deadline);
    }
}
