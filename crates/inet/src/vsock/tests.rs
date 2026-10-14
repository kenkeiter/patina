//! Host tests of the vsock core: the header codec against the exact
//! `virtio_vsock_hdr` layout, ports, credit math, the state machine
//! driven by a scripted device, guest↔host runs over two cores whose
//! device packets travel as encoded bytes, and the loopback transport.

use alloc::vec;
use alloc::vec::Vec;

use super::*;
use patina_abi::net::{VMADDR_CID_HOST, VMADDR_CID_LOCAL};

const GUEST: u32 = 3;
const HOST: u32 = VMADDR_CID_HOST;

fn e(n: i32) -> Errno {
    Errno(n)
}

/// Feed loopback packets back until quiet; return the device packets.
fn pump(c: &mut Core) -> Vec<Packet> {
    let mut dev = Vec::new();
    for _ in 0..10_000 {
        let out = c.take_out();
        if out.is_empty() {
            return dev;
        }
        for p in out {
            assert_eq!(p.hdr.len as usize, p.payload.len());
            match p.via {
                Via::Loopback => c.input(p),
                Via::Device => dev.push(p),
            }
        }
    }
    panic!("loopback never settled");
}

/// Serialise a packet as the device would see it, and parse it back.
fn wire(p: &Packet) -> Packet {
    let mut b = Vec::from(p.hdr.encode());
    b.extend_from_slice(&p.payload);
    let hdr = Hdr::decode(&b).unwrap();
    assert_eq!(hdr.len as usize, b.len() - HDR_LEN);
    Packet {
        via: Via::Device,
        hdr,
        payload: b[HDR_LEN..].to_vec(),
    }
}

/// A guest core and a host core joined by a "device": device packets are
/// encoded and decoded on the way. The host core is the same state
/// machine (the protocol is symmetric); its "guest CID" is 2.
struct Pair {
    g: Core,
    h: Core,
    /// Split host→guest RW packets to at most this many bytes (the device
    /// fitting them into the guest's rx buffers).
    split: usize,
    /// Packets sent guest→host / host→guest so far (for assertions).
    g2h: Vec<Packet>,
    h2g: Vec<Packet>,
}

impl Pair {
    fn new() -> Pair {
        let mut g = Core::new(1);
        g.set_device(Some(GUEST));
        g.reserve(4242);
        let mut h = Core::new(2);
        h.set_device(Some(HOST));
        h.set_dev_payload(MAX_PKT_BUF_SIZE);
        Pair {
            g,
            h,
            split: usize::MAX,
            g2h: Vec::new(),
            h2g: Vec::new(),
        }
    }

    /// Exchange device packets until both sides are quiet.
    fn run(&mut self) {
        for _ in 0..100_000 {
            let a = pump(&mut self.g);
            let b = pump(&mut self.h);
            if a.is_empty() && b.is_empty() {
                return;
            }
            for p in a {
                let w = wire(&p);
                self.g2h.push(w.clone());
                self.h.input(w);
            }
            for p in b {
                let mut w = wire(&p);
                if w.hdr.op == OP_RW && w.payload.len() > self.split {
                    for ch in p.payload.chunks(self.split) {
                        w.hdr.len = ch.len() as u32;
                        w.payload = ch.to_vec();
                        self.h2g.push(w.clone());
                        self.g.input(w.clone());
                    }
                } else {
                    self.h2g.push(w.clone());
                    self.g.input(w);
                }
            }
        }
        panic!("pair never settled");
    }
}

/// A connected loopback pair on `core`: (client, accepted, listener).
fn loop_pair(c: &mut Core, port: u32, cid: u32) -> (SockId, SockId, SockId) {
    let l = c.create();
    c.bind(l, Addr::new(VMADDR_CID_ANY, port)).unwrap();
    c.listen(l, 16).unwrap();
    let a = c.create();
    assert_eq!(c.connect(a, Addr::new(cid, port), 0), Err(e(EINPROGRESS)));
    assert!(pump(c).is_empty());
    assert_eq!(c.connect_result(a), Some(Ok(())));
    let (b, peer) = c.accept(l).unwrap();
    assert_eq!(peer.port, c.local_addr(a).unwrap().port);
    (a, b, l)
}

fn hdr(op: u16, src: Addr, dst: Addr) -> Hdr {
    Hdr {
        src_cid: src.cid as u64,
        dst_cid: dst.cid as u64,
        src_port: src.port,
        dst_port: dst.port,
        len: 0,
        ty: TYPE_STREAM,
        op,
        flags: 0,
        buf_alloc: 1000,
        fwd_cnt: 0,
    }
}

fn dev(h: Hdr, payload: &[u8]) -> Packet {
    let mut h = h;
    h.len = payload.len() as u32;
    Packet {
        via: Via::Device,
        hdr: h,
        payload: payload.to_vec(),
    }
}

fn ops(ps: &[Packet]) -> Vec<u16> {
    ps.iter().map(|p| p.hdr.op).collect()
}

fn read_all(c: &mut Core, id: SockId) -> Vec<u8> {
    let mut v = Vec::new();
    let mut buf = [0u8; 7919];
    loop {
        match c.recv(id, &mut buf, false) {
            Ok(0) | Err(_) => return v,
            Ok(n) => v.extend_from_slice(&buf[..n]),
        }
    }
}

fn pattern(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

// ---------------------------------------------------------------------
// Header codec.

#[test]
fn header_layout() {
    let h = Hdr {
        src_cid: 0x0102030405060708,
        dst_cid: 0x1112131415161718,
        src_port: 0x21222324,
        dst_port: 0x31323334,
        len: 0x41424344,
        ty: 0x5152,
        op: 0x6162,
        flags: 0x71727374,
        buf_alloc: 0x81828384,
        fwd_cnt: 0x91929394,
    };
    let b = h.encode();
    assert_eq!(b.len(), 44);
    #[rustfmt::skip]
    let want: [u8; 44] = [
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // src_cid le64
        0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11, // dst_cid le64
        0x24, 0x23, 0x22, 0x21, // src_port
        0x34, 0x33, 0x32, 0x31, // dst_port
        0x44, 0x43, 0x42, 0x41, // len
        0x52, 0x51,             // type le16
        0x62, 0x61,             // op le16
        0x74, 0x73, 0x72, 0x71, // flags
        0x84, 0x83, 0x82, 0x81, // buf_alloc
        0x94, 0x93, 0x92, 0x91, // fwd_cnt
    ];
    assert_eq!(b, want);
    assert_eq!(Hdr::decode(&b), Some(h));
    let mut longer = Vec::from(b);
    longer.extend_from_slice(b"payload");
    assert_eq!(Hdr::decode(&longer), Some(h));
    assert_eq!(Hdr::decode(&b[..43]), None);
    let r = h.reply(OP_RST);
    assert_eq!((r.src_cid, r.dst_cid), (h.dst_cid, h.src_cid));
    assert_eq!((r.src_port, r.dst_port), (h.dst_port, h.src_port));
    assert_eq!((r.op, r.len, r.buf_alloc, r.fwd_cnt), (OP_RST, 0, 0, 0));
    assert_eq!(op_name(OP_CREDIT_REQUEST), "CREDIT_REQUEST");
    assert_eq!(op_name(99), "INVALID");
}

// ---------------------------------------------------------------------
// Ports.

#[test]
fn ephemeral_ports() {
    let mut c = Core::new(0);
    c.reserve(4242);
    let a = c.create();
    c.bind(a, Addr::ANY).unwrap();
    let p = c.local_addr(a).unwrap();
    assert_eq!(p.cid, VMADDR_CID_ANY);
    assert!(p.port > LAST_RESERVED_PORT && p.port != VMADDR_PORT_ANY);
    // Consecutive, skipping taken ports.
    let b = c.create();
    c.bind(b, Addr::new(VMADDR_CID_ANY, p.port + 2)).unwrap();
    let x = c.create();
    c.bind(x, Addr::ANY).unwrap();
    assert_eq!(c.local_addr(x).unwrap().port, p.port + 1);
    let y = c.create();
    c.bind(y, Addr::ANY).unwrap();
    assert_eq!(c.local_addr(y).unwrap().port, p.port + 3);
    // Already bound.
    assert_eq!(c.bind(y, Addr::ANY), Err(e(EINVAL)));
}

#[test]
fn ephemeral_wraps_and_exhausts() {
    // Seed so the cursor starts near the top of the range.
    let span = (VMADDR_PORT_ANY - 1 - LAST_RESERVED_PORT) as u64;
    let mut c = Core::new(span - 2);
    let a = c.create();
    c.bind(a, Addr::ANY).unwrap();
    assert_eq!(c.local_addr(a).unwrap().port, VMADDR_PORT_ANY - 2);
    let b = c.create();
    c.bind(b, Addr::ANY).unwrap();
    assert_eq!(c.local_addr(b).unwrap().port, VMADDR_PORT_ANY - 1);
    // Never VMADDR_PORT_ANY, never a reserved port: wraps to 1024.
    let d = c.create();
    c.bind(d, Addr::ANY).unwrap();
    assert_eq!(c.local_addr(d).unwrap().port, LAST_RESERVED_PORT + 1);
    // 24 taken ports in a row: EADDRNOTAVAIL.
    let mut c = Core::new(0);
    for i in 0..MAX_PORT_RETRIES {
        let s = c.create();
        c.bind(s, Addr::new(VMADDR_CID_ANY, 1024 + i)).unwrap();
    }
    let s = c.create();
    assert_eq!(c.bind(s, Addr::ANY), Err(e(EADDRNOTAVAIL)));
    assert_eq!(c.bind(s, Addr::ANY), Ok(()), "the cursor moved on");
}

#[test]
fn bind_rules() {
    let mut c = Core::new(7);
    c.reserve(4242);
    let s = c.create();
    // Foreign CIDs: EADDRNOTAVAIL (no device: only VMADDR_CID_LOCAL).
    assert_eq!(c.bind(s, Addr::new(GUEST, 5000)), Err(e(EADDRNOTAVAIL)));
    assert_eq!(c.bind(s, Addr::new(HOST, 5000)), Err(e(EADDRNOTAVAIL)));
    // The reserved control port.
    assert_eq!(
        c.bind(s, Addr::new(VMADDR_CID_ANY, 4242)),
        Err(e(EADDRINUSE))
    );
    assert_eq!(
        c.bind(s, Addr::new(VMADDR_CID_LOCAL, 4242)),
        Err(e(EADDRINUSE))
    );
    let k = c.create_kernel();
    c.bind(k, Addr::new(VMADDR_CID_ANY, 4242)).unwrap();
    c.bind(s, Addr::new(VMADDR_CID_LOCAL, 5000)).unwrap();
    // CID overlap: ANY covers every CID, distinct CIDs do not clash.
    c.set_device(Some(GUEST));
    let t = c.create();
    assert_eq!(
        c.bind(t, Addr::new(VMADDR_CID_ANY, 5000)),
        Err(e(EADDRINUSE))
    );
    assert_eq!(
        c.bind(t, Addr::new(VMADDR_CID_LOCAL, 5000)),
        Err(e(EADDRINUSE))
    );
    c.bind(t, Addr::new(GUEST, 5000)).unwrap();
    // Releasing frees the port.
    c.release(s, 0);
    c.release(t, 0);
    let u = c.create();
    c.bind(u, Addr::new(VMADDR_CID_ANY, 5000)).unwrap();
    // listen needs a bound socket.
    let v = c.create();
    assert_eq!(c.listen(v, 1), Err(e(EINVAL)));
}

// ---------------------------------------------------------------------
// Credit math.

#[test]
fn credit_wraparound() {
    let mut s = Sock::new();
    s.peer_buf_alloc = 100;
    s.tx_cnt = u32::MAX - 10;
    s.peer_fwd_cnt = u32::MAX - 20;
    assert_eq!(s.has_space(), 90);
    assert_eq!(s.get_credit(50), 50);
    assert_eq!(s.tx_cnt, 39, "tx_cnt wrapped");
    assert_eq!(s.has_space(), 40);
    assert_eq!(s.get_credit(1000), 40);
    assert_eq!(s.has_space(), 0);
    assert_eq!(s.get_credit(1), 0);
    // The peer consumed across the wrap.
    s.peer_fwd_cnt = 30;
    assert_eq!(s.has_space(), 100 - 79u32.wrapping_sub(30));
    // A peer shrinking its buffer below what is in flight: no space.
    s.peer_buf_alloc = 5;
    assert_eq!(s.has_space(), 0);
}

#[test]
fn credit_update_threshold_and_fwd_wrap() {
    let mut c = Core::new(1);
    let (a, b, _) = loop_pair(&mut c, 9000, VMADDR_CID_LOCAL);
    // Start the receiver's counters just below the wrap.
    {
        let sb = c.socks.get_mut(&b).unwrap();
        sb.fwd_cnt = u32::MAX - 100;
        sb.last_fwd_cnt = u32::MAX - 100;
        let sa = c.socks.get_mut(&a).unwrap();
        sa.tx_cnt = u32::MAX - 100;
        sa.peer_fwd_cnt = u32::MAX - 100;
    }
    let data = pattern(200_000, 1);
    assert_eq!(c.send(a, &data), Ok(200_000));
    assert!(pump(&mut c).is_empty());
    // Partial reads with plenty of space left: no CREDIT_UPDATE.
    let mut buf = vec![0u8; 1000];
    assert_eq!(c.recv(b, &mut buf, false), Ok(1000));
    assert!(
        c.take_out().is_empty(),
        "no update while space is plentiful"
    );
    // Reading until free space < 64 KiB (buf 256 KiB, consumed >192 KiB)
    // or the buffer is empty sends one.
    let mut got = buf.clone();
    let mut updates = 0;
    while got.len() < 200_000 {
        let n = c.recv(b, &mut buf, false).unwrap();
        got.extend_from_slice(&buf[..n]);
        let out = c.take_out();
        updates += out.iter().filter(|p| p.hdr.op == OP_CREDIT_UPDATE).count();
        for p in out {
            c.input(p);
        }
    }
    assert_eq!(got, data);
    assert!(updates >= 1);
    let ib = c.info(b).unwrap();
    assert_eq!(ib.fwd_cnt, (u32::MAX - 100).wrapping_add(200_000));
    let ia = c.info(a).unwrap();
    assert_eq!(ia.tx_cnt, ib.fwd_cnt);
    assert_eq!(ia.peer_fwd_cnt, ib.fwd_cnt);
    assert_eq!(ia.credit, ib.buf_alloc);
}

// ---------------------------------------------------------------------
// Loopback.

#[test]
fn loopback_handshake_and_addresses() {
    let mut c = Core::new(5);
    let (a, b, l) = loop_pair(&mut c, 7000, VMADDR_CID_LOCAL);
    let ia = c.info(a).unwrap();
    assert_eq!(
        (ia.sk, ia.ss, ia.via),
        (Sk::Established, Ss::Connected, Some(Via::Loopback))
    );
    // The client keeps VMADDR_CID_ANY (autobind), the peer is what it
    // connected to; the accepted socket sees VMADDR_CID_LOCAL as peer.
    let la = c.local_addr(a).unwrap();
    assert_eq!(la.cid, VMADDR_CID_ANY);
    assert_eq!(c.peer_addr(a).unwrap(), Addr::new(VMADDR_CID_LOCAL, 7000));
    assert_eq!(c.local_addr(b).unwrap(), Addr::new(VMADDR_CID_LOCAL, 7000));
    assert_eq!(
        c.peer_addr(b).unwrap(),
        Addr::new(VMADDR_CID_LOCAL, la.port)
    );
    assert_eq!(c.peer_addr(l), Err(e(ENOTCONN)));
    assert_eq!(c.accept(l), Err(e(EAGAIN)));
    assert!(c.poll(a).contains(EpollFlags::OUT));
    assert!(!c.poll(a).contains(EpollFlags::IN));
    assert_eq!(c.connect(a, Addr::new(1, 7000), 0), Err(e(EISCONN)));
    assert_eq!(c.listen(a, 1), Err(e(EINVAL)));
    assert_eq!(c.accept(a), Err(e(EINVAL)));
}

#[test]
fn loopback_via_guest_cid() {
    let mut c = Core::new(5);
    c.set_device(Some(GUEST));
    let (a, b, _) = loop_pair(&mut c, 7001, GUEST);
    assert_eq!(c.info(a).unwrap().via, Some(Via::Loopback));
    assert_eq!(c.peer_addr(a).unwrap(), Addr::new(GUEST, 7001));
    assert_eq!(c.local_addr(b).unwrap(), Addr::new(GUEST, 7001));
    // Data both ways although the packets carry VMADDR_CID_LOCAL.
    assert_eq!(c.send(a, b"ping"), Ok(4));
    assert_eq!(c.send(b, b"pong"), Ok(4));
    assert!(pump(&mut c).is_empty());
    let mut buf = [0u8; 8];
    assert_eq!(c.recv(b, &mut buf, false), Ok(4));
    assert_eq!(&buf[..4], b"ping");
    assert_eq!(c.recv(a, &mut buf, false), Ok(4));
    assert_eq!(&buf[..4], b"pong");
    // A listener bound to the guest CID does not take VMADDR_CID_LOCAL
    // (Linux: the bound lookup needs the same CID or ANY).
    let l = c.create();
    c.bind(l, Addr::new(GUEST, 7002)).unwrap();
    c.listen(l, 4).unwrap();
    let x = c.create();
    assert_eq!(
        c.connect(x, Addr::new(VMADDR_CID_LOCAL, 7002), 0),
        Err(e(EINPROGRESS))
    );
    pump(&mut c);
    assert_eq!(c.connect_result(x), Some(Err(e(ECONNRESET))));
}

#[test]
fn transport_selection() {
    let mut c = Core::new(5);
    let s = c.create();
    // No device: only VMADDR_CID_LOCAL is reachable.
    for cid in [VMADDR_CID_HOST, GUEST, 0, VMADDR_CID_ANY] {
        assert_eq!(c.connect(s, Addr::new(cid, 80), 0), Err(e(ENODEV)));
    }
    assert_eq!(c.local_cid(), VMADDR_CID_ANY);
    c.set_device(Some(GUEST));
    assert_eq!(c.local_cid(), GUEST);
    assert_eq!(c.connect(s, Addr::new(HOST, 80), 0), Err(e(EINPROGRESS)));
    let out = c.take_out();
    assert_eq!(out.len(), 1);
    let p = &out[0];
    assert_eq!(p.via, Via::Device);
    assert_eq!(
        (p.hdr.op, p.hdr.ty, p.hdr.len),
        (OP_REQUEST, TYPE_STREAM, 0)
    );
    assert_eq!(
        (p.hdr.src_cid, p.hdr.dst_cid, p.hdr.dst_port),
        (GUEST as u64, HOST as u64, 80)
    );
    assert_eq!(p.hdr.buf_alloc, DEFAULT_BUFFER_SIZE as u32);
    assert_eq!(c.connect(s, Addr::new(HOST, 80), 0), Err(e(EALREADY)));
    assert_eq!(c.connect_result(s), None);
    assert!(c.poll(s).is_empty(), "connecting: neither IN nor OUT");
}

#[test]
fn refused_and_reserved() {
    let mut c = Core::new(5);
    c.reserve(4242);
    let s = c.create();
    assert_eq!(
        c.connect(s, Addr::new(VMADDR_CID_LOCAL, 5555), 0),
        Err(e(EINPROGRESS))
    );
    pump(&mut c);
    // Linux: a reset while connecting is ECONNRESET.
    assert_eq!(c.connect_result(s), Some(Err(e(ECONNRESET))));
    let p = c.poll(s);
    assert!(p.contains(EpollFlags::ERR | EpollFlags::OUT));
    assert_eq!(c.take_error(s), ECONNRESET);
    assert_eq!(c.take_error(s), 0);
    assert_eq!(c.recv(s, &mut [0; 4], false), Err(e(ENOTCONN)));
    assert_eq!(c.send(s, b"x"), Err(e(ENOTCONN)));
    // It may connect again (to a listener this time).
    let l = c.create();
    c.bind(l, Addr::new(VMADDR_CID_ANY, 5555)).unwrap();
    c.listen(l, 1).unwrap();
    assert_eq!(
        c.connect(s, Addr::new(VMADDR_CID_LOCAL, 5555), 0),
        Err(e(EINPROGRESS))
    );
    pump(&mut c);
    assert_eq!(c.connect_result(s), Some(Ok(())));
    // The kernel's control listener takes device connections only.
    c.set_device(Some(GUEST));
    let k = c.create_kernel();
    c.bind(k, Addr::new(VMADDR_CID_ANY, 4242)).unwrap();
    c.listen(k, 4).unwrap();
    for cid in [VMADDR_CID_LOCAL, GUEST] {
        let t = c.create();
        assert_eq!(c.connect(t, Addr::new(cid, 4242), 0), Err(e(EINPROGRESS)));
        pump(&mut c);
        assert_eq!(c.connect_result(t), Some(Err(e(ECONNRESET))));
    }
    assert_eq!(c.info(k).unwrap().queued, 0);
    c.input(dev(
        hdr(OP_REQUEST, Addr::new(HOST, 1111), Addr::new(GUEST, 4242)),
        &[],
    ));
    assert_eq!(c.info(k).unwrap().queued, 1);
    assert_eq!(ops(&c.take_out()), vec![OP_RESPONSE]);
}

#[test]
fn loopback_stream_and_backpressure() {
    let mut c = Core::new(9);
    let (a, b, _) = loop_pair(&mut c, 7100, VMADDR_CID_LOCAL);
    let data = pattern(1 << 20, 3);
    let mut sent = 0;
    let mut got = Vec::new();
    let mut blocked = 0;
    let mut buf = vec![0u8; 10_000];
    while got.len() < data.len() {
        // Writer: until the peer's credit runs out.
        loop {
            if sent == data.len() {
                break;
            }
            match c.send(a, &data[sent..]) {
                Ok(n) => sent += n,
                Err(Errno(EAGAIN)) => {
                    blocked += 1;
                    assert!(!c.poll(a).contains(EpollFlags::OUT));
                    break;
                }
                Err(x) => panic!("{x:?}"),
            }
            pump(&mut c);
        }
        pump(&mut c);
        // The receiver never holds more than it advertised.
        let ib = c.info(b).unwrap();
        assert!(ib.rx_bytes <= ib.buf_alloc as usize);
        assert!(sent - got.len() <= DEFAULT_BUFFER_SIZE as usize);
        // Slow reader: a few small reads per round.
        for _ in 0..3 {
            match c.recv(b, &mut buf, false) {
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(Errno(EAGAIN)) => break,
                Err(x) => panic!("{x:?}"),
            }
            pump(&mut c);
        }
    }
    assert!(blocked > 0, "the writer hit the credit limit");
    assert_eq!(got, data);
}

#[test]
fn peek_and_partial() {
    let mut c = Core::new(9);
    let (a, b, _) = loop_pair(&mut c, 7101, VMADDR_CID_LOCAL);
    assert_eq!(c.recv(b, &mut [0; 4], false), Err(e(EAGAIN)));
    c.send(a, b"hello world").unwrap();
    pump(&mut c);
    assert!(c.poll(b).contains(EpollFlags::IN | EpollFlags::RDNORM));
    let mut buf = [0u8; 5];
    assert_eq!(c.recv(b, &mut buf, true), Ok(5));
    assert_eq!(&buf, b"hello");
    assert_eq!(c.rx_bytes(b), 11);
    assert_eq!(c.recv(b, &mut buf, false), Ok(5));
    assert_eq!(c.recv(b, &mut [], false), Ok(0));
    let mut rest = [0u8; 64];
    assert_eq!(c.recv(b, &mut rest, false), Ok(6));
    assert_eq!(&rest[..6], b" world");
}

#[test]
fn half_close_and_close() {
    let mut c = Core::new(9);
    let (a, b, _) = loop_pair(&mut c, 7200, VMADDR_CID_LOCAL);
    c.send(a, b"request").unwrap();
    c.shutdown(a, 1).unwrap(); // SHUT_WR
    pump(&mut c);
    assert_eq!(c.send(a, b"x"), Err(e(EPIPE)));
    assert_eq!(c.peer_addr(a), Err(e(ENOTCONN)), "SS_DISCONNECTING");
    let pb = c.poll(b);
    assert!(pb.contains(EpollFlags::RDHUP | EpollFlags::IN));
    assert!(!pb.contains(EpollFlags::HUP));
    assert_eq!(read_all(&mut c, b), b"request");
    assert_eq!(c.recv(b, &mut [0; 4], false), Ok(0), "EOF");
    // The server answers and closes; the client reads it, then EOF.
    c.send(b, b"response").unwrap();
    c.release(b, 0);
    pump(&mut c);
    assert_eq!(read_all(&mut c, a), b"response");
    assert_eq!(c.recv(a, &mut [0; 4], false), Ok(0));
    assert!(c.poll(a).contains(EpollFlags::HUP));
    // The server's orphan got the client's RST once the client had read
    // everything... only when the client closes (it still had data).
    let before = c.len();
    c.release(a, 0);
    pump(&mut c);
    assert!(c.len() < before);
    assert_eq!(c.len(), 1, "only the listener is left");
}

#[test]
fn close_with_both_idle_resets_at_once() {
    let mut c = Core::new(9);
    let (a, b, l) = loop_pair(&mut c, 7300, VMADDR_CID_LOCAL);
    c.release(a, 0);
    // SHUTDOWN(both) → the peer had nothing unread: RST back, both done.
    let out = c.take_out();
    assert_eq!(ops(&out), vec![OP_SHUTDOWN]);
    assert_eq!(out[0].hdr.flags, SHUTDOWN_RCV | SHUTDOWN_SEND);
    for p in out {
        c.input(p);
    }
    let out = c.take_out();
    assert_eq!(ops(&out), vec![OP_RST]);
    for p in out {
        c.input(p);
    }
    assert!(!c.exists(a), "the orphan is gone after the RST");
    let ib = c.info(b).unwrap();
    assert!(ib.done);
    assert_eq!(ib.sk, Sk::Closing);
    assert_eq!(c.recv(b, &mut [0; 4], false), Ok(0));
    assert_eq!(c.send(b, b"x"), Err(e(EPIPE)));
    let pb = c.poll(b);
    assert!(pb.contains(EpollFlags::IN | EpollFlags::RDHUP | EpollFlags::OUT));
    c.release(b, 0);
    assert!(pump(&mut c).is_empty());
    assert_eq!(c.len(), 1);
    c.release(l, 0);
    assert!(c.is_empty());
}

#[test]
fn orphan_times_out() {
    let mut p = Pair::new();
    let s = p.g.create();
    let hl = p.h.create();
    p.h.bind(hl, Addr::new(VMADDR_CID_ANY, 80)).unwrap();
    p.h.listen(hl, 1).unwrap();
    assert_eq!(p.g.connect(s, Addr::new(HOST, 80), 0), Err(e(EINPROGRESS)));
    p.run();
    let (hs, _) = p.h.accept(hl).unwrap();
    // The host never closes; it has unread data, so the guest's close
    // SHUTDOWN is not answered with RST.
    p.g.send(s, b"unread").unwrap();
    p.run();
    p.g.release(s, 1000);
    p.run();
    assert!(p.g.exists(s));
    assert!(p.g.info(s).unwrap().orphan);
    assert_eq!(p.g.next_deadline(), Some(1000 + CLOSE_TIMEOUT_NS));
    p.g.tick(1000 + CLOSE_TIMEOUT_NS - 1);
    assert!(p.g.exists(s));
    p.g.tick(1000 + CLOSE_TIMEOUT_NS);
    assert!(!p.g.exists(s));
    assert_eq!(ops(&pump(&mut p.g)), vec![OP_RST]);
    assert_eq!(p.g.next_deadline(), None);
    // The host had already seen SHUTDOWN(both): data still readable.
    assert_eq!(read_all(&mut p.h, hs), b"unread");
}

#[test]
fn backlog_bound() {
    let mut c = Core::new(9);
    let l = c.create();
    c.bind(l, Addr::new(VMADDR_CID_ANY, 7400)).unwrap();
    c.listen(l, 0).unwrap();
    // Backlog 0 still queues one (full when longer than the backlog).
    let a = c.create();
    let _ = c.connect(a, Addr::new(1, 7400), 0);
    pump(&mut c);
    assert_eq!(c.connect_result(a), Some(Ok(())));
    let b = c.create();
    let _ = c.connect(b, Addr::new(1, 7400), 0);
    pump(&mut c);
    assert_eq!(c.connect_result(b), Some(Err(e(ECONNRESET))));
    // A negative backlog is SOMAXCONN.
    c.listen(l, -1).unwrap();
    assert_eq!(c.socks[&l].backlog, SOMAXCONN as usize);
    // Closing the listener releases its queue (SHUTDOWN to the client).
    c.release(l, 0);
    pump(&mut c);
    assert_eq!(c.recv(a, &mut [0; 4], false), Ok(0));
}

#[test]
fn shut_rd_and_bad_how() {
    let mut c = Core::new(9);
    let (a, b, _) = loop_pair(&mut c, 7500, VMADDR_CID_LOCAL);
    assert_eq!(c.shutdown(a, 3), Err(e(EINVAL)));
    assert_eq!(c.shutdown(a, -1), Err(e(EINVAL)));
    c.shutdown(a, 0).unwrap(); // SHUT_RD
    pump(&mut c);
    assert_eq!(c.recv(a, &mut [0; 4], false), Ok(0));
    // The peer may not send any more (EPIPE), but we still may.
    assert_eq!(c.send(b, b"x"), Err(e(EPIPE)));
    assert_eq!(c.send(a, b"y"), Ok(1));
    let s = c.create();
    assert_eq!(c.shutdown(s, 2), Err(e(ENOTCONN)));
}

// ---------------------------------------------------------------------
// The scripted device.

fn guest() -> Core {
    let mut c = Core::new(11);
    c.set_device(Some(GUEST));
    c
}

#[test]
fn device_accept_and_data() {
    let mut c = guest();
    let l = c.create();
    c.bind(l, Addr::new(VMADDR_CID_ANY, 1234)).unwrap();
    c.listen(l, 8).unwrap();
    let host = Addr::new(HOST, 40000);
    let me = Addr::new(GUEST, 1234);
    let mut req = hdr(OP_REQUEST, host, me);
    req.buf_alloc = 1000;
    c.input(dev(req, &[]));
    let out = c.take_out();
    assert_eq!(out.len(), 1);
    let r = &out[0].hdr;
    assert_eq!(out[0].via, Via::Device);
    assert_eq!(r.op, OP_RESPONSE);
    assert_eq!(
        (r.src_cid, r.src_port, r.dst_cid, r.dst_port),
        (3, 1234, 2, 40000)
    );
    assert_eq!(
        (r.buf_alloc, r.fwd_cnt, r.len, r.ty),
        (DEFAULT_BUFFER_SIZE as u32, 0, 0, TYPE_STREAM)
    );
    assert!(c.poll(l).contains(EpollFlags::IN));
    let (s, peer) = c.accept(l).unwrap();
    assert_eq!(peer, host);
    assert_eq!(c.local_addr(s).unwrap(), me);
    // Host data arrives; reading it all sends a CREDIT_UPDATE.
    c.input(dev(hdr(OP_RW, host, me), b"abc"));
    c.input(dev(hdr(OP_RW, host, me), b"def"));
    assert!(c.take_out().is_empty());
    assert_eq!(read_all(&mut c, s), b"abcdef");
    let out = c.take_out();
    assert_eq!(ops(&out), vec![OP_CREDIT_UPDATE]);
    assert_eq!(out[0].hdr.fwd_cnt, 6);
    // Credit request → update.
    c.input(dev(hdr(OP_CREDIT_REQUEST, host, me), &[]));
    let out = c.take_out();
    assert_eq!(ops(&out), vec![OP_CREDIT_UPDATE]);
    assert_eq!(out[0].hdr.fwd_cnt, 6);
    // Our sends respect the host's buf_alloc (1000) and are split into
    // device-sized packets.
    c.set_dev_payload(300);
    let data = pattern(5000, 9);
    assert_eq!(c.send(s, &data), Ok(1000));
    let out = c.take_out();
    assert_eq!(
        out.iter().map(|p| p.payload.len()).collect::<Vec<_>>(),
        vec![300, 300, 300, 100]
    );
    assert!(
        out.iter()
            .all(|p| p.hdr.op == OP_RW && p.hdr.dst_port == 40000)
    );
    assert_eq!(c.send(s, &data[1000..]), Err(e(EAGAIN)));
    assert_eq!(
        ops(&c.take_out()),
        vec![OP_CREDIT_REQUEST],
        "one request per stall"
    );
    assert_eq!(c.send(s, &data[1000..]), Err(e(EAGAIN)));
    assert!(c.take_out().is_empty());
    // The host consumed 600: 600 more.
    let mut u = hdr(OP_CREDIT_UPDATE, host, me);
    u.buf_alloc = 1000;
    u.fwd_cnt = 600;
    c.input(dev(u, &[]));
    assert!(c.take_dirty().contains(&s));
    assert!(c.poll(s).contains(EpollFlags::OUT));
    assert_eq!(c.send(s, &data[1000..]), Ok(600));
    // A peer overrunning our buffer loses the packet.
    c.set_buffer(s, 0, 128).unwrap();
    c.input(dev(hdr(OP_RW, host, me), &[0; 129]));
    assert_eq!(c.rx_bytes(s), 0);
    c.input(dev(hdr(OP_RW, host, me), &[0; 128]));
    assert_eq!(c.rx_bytes(s), 128);
}

#[test]
fn device_junk_gets_rst() {
    let mut c = guest();
    let host = Addr::new(HOST, 40000);
    // Nobody on the port.
    c.input(dev(hdr(OP_REQUEST, host, Addr::new(GUEST, 9)), &[]));
    let out = c.take_out();
    assert_eq!(ops(&out), vec![OP_RST]);
    let r = out[0].hdr;
    assert_eq!(
        (r.src_cid, r.src_port, r.dst_cid, r.dst_port),
        (3, 9, 2, 40000)
    );
    // Data for an unknown connection.
    c.input(dev(hdr(OP_RW, host, Addr::new(GUEST, 9)), b"x"));
    assert_eq!(ops(&c.take_out()), vec![OP_RST]);
    // Never answer a RST.
    c.input(dev(hdr(OP_RST, host, Addr::new(GUEST, 9)), &[]));
    assert!(c.take_out().is_empty());
    // Unknown type, seqpacket (we have only streams).
    let l = c.create();
    c.bind(l, Addr::new(VMADDR_CID_ANY, 10)).unwrap();
    c.listen(l, 8).unwrap();
    for ty in [0, TYPE_SEQPACKET, 9] {
        let mut h = hdr(OP_REQUEST, host, Addr::new(GUEST, 10));
        h.ty = ty;
        c.input(dev(h, &[]));
        let out = c.take_out();
        assert_eq!(ops(&out), vec![OP_RST]);
        assert_eq!(out[0].hdr.ty, ty);
    }
    // A listener gets something other than REQUEST.
    c.input(dev(hdr(OP_RW, host, Addr::new(GUEST, 10)), b"x"));
    assert_eq!(ops(&c.take_out()), vec![OP_RST]);
    assert_eq!(c.info(l).unwrap().queued, 0);
    // A RESPONSE for a socket that is not connecting.
    let s = c.create();
    c.bind(s, Addr::new(VMADDR_CID_ANY, 11)).unwrap();
    c.input(dev(hdr(OP_RESPONSE, host, Addr::new(GUEST, 11)), &[]));
    assert_eq!(ops(&c.take_out()), vec![OP_RST]);
}

#[test]
fn device_connect_paths() {
    let host = Addr::new(HOST, 5000);
    // RESPONSE completes; the host's credit comes with it.
    let mut c = guest();
    let s = c.create();
    let _ = c.connect(s, host, 0);
    let req = c.take_out().remove(0).hdr;
    let me = Addr::new(GUEST, req.src_port);
    let mut r = hdr(OP_RESPONSE, host, me);
    r.buf_alloc = 4096;
    c.input(dev(r, &[]));
    assert_eq!(c.connect_result(s), Some(Ok(())));
    assert_eq!(c.info(s).unwrap().credit, 4096);
    assert!(c.poll(s).contains(EpollFlags::OUT));
    // A RESPONSE from someone else is reset and changes nothing.
    let s2 = c.create();
    let _ = c.connect(s2, host, 0);
    let me2 = Addr::new(GUEST, c.take_out().remove(0).hdr.src_port);
    c.input(dev(hdr(OP_RESPONSE, Addr::new(HOST, 5001), me2), &[]));
    assert_eq!(ops(&c.take_out()), vec![OP_RST]);
    assert_eq!(c.connect_result(s2), None);
    // Anything but RESPONSE/RST/INVALID while connecting: EPROTO + RST.
    c.input(dev(hdr(OP_INVALID, host, me2), &[]));
    assert_eq!(c.connect_result(s2), None);
    c.input(dev(hdr(OP_RW, host, me2), b"early"));
    assert_eq!(ops(&c.take_out()), vec![OP_RST]);
    assert_eq!(c.connect_result(s2), Some(Err(e(EPROTO))));
    // RST: ECONNRESET, no answer.
    let s3 = c.create();
    let _ = c.connect(s3, host, 0);
    let me3 = Addr::new(GUEST, c.take_out().remove(0).hdr.src_port);
    c.input(dev(hdr(OP_RST, host, me3), &[]));
    assert!(c.take_out().is_empty());
    assert_eq!(c.connect_result(s3), Some(Err(e(ECONNRESET))));
    // No answer at all: ETIMEDOUT after the connect timeout.
    let s4 = c.create();
    c.set_connect_timeout(s4, 500).unwrap();
    assert_eq!(c.connect_timeout(s4), Ok(500));
    let _ = c.connect(s4, host, 1000);
    c.take_out();
    assert_eq!(c.next_deadline(), Some(1500));
    c.tick(1499);
    assert_eq!(c.connect_result(s4), None);
    c.tick(1500);
    assert_eq!(c.connect_result(s4), Some(Err(e(ETIMEDOUT))));
    assert!(c.poll(s4).contains(EpollFlags::ERR | EpollFlags::OUT));
    assert_eq!(c.next_deadline(), None);
    c.set_connect_timeout(s4, 0).unwrap();
    assert_eq!(c.connect_timeout(s4), Ok(DEFAULT_CONNECT_TIMEOUT_NS));
}

#[test]
fn device_shutdown_and_reset() {
    let host = Addr::new(HOST, 6000);
    let me = Addr::new(GUEST, 77);
    let setup = || {
        let mut c = guest();
        let l = c.create();
        c.bind(l, Addr::new(VMADDR_CID_ANY, 77)).unwrap();
        c.listen(l, 8).unwrap();
        c.input(dev(hdr(OP_REQUEST, host, me), &[]));
        c.take_out();
        let (s, _) = c.accept(l).unwrap();
        (c, s)
    };
    // SHUTDOWN(SEND): EOF after the data, we may still write.
    let (mut c, s) = setup();
    c.input(dev(hdr(OP_RW, host, me), b"tail"));
    let mut sh = hdr(OP_SHUTDOWN, host, me);
    sh.flags = SHUTDOWN_SEND;
    c.input(dev(sh, &[]));
    assert!(c.poll(s).contains(EpollFlags::RDHUP));
    assert_eq!(read_all(&mut c, s), b"tail");
    assert_eq!(c.recv(s, &mut [0; 1], false), Ok(0));
    assert_eq!(c.send(s, b"ok"), Ok(2));
    // SHUTDOWN(both) with nothing unread: RST, done.
    sh.flags = SHUTDOWN_RCV | SHUTDOWN_SEND;
    c.take_out();
    c.input(dev(sh, &[]));
    assert_eq!(ops(&c.take_out()), vec![OP_RST]);
    assert!(c.info(s).unwrap().done);
    assert_eq!(c.send(s, b"x"), Err(e(EPIPE)));
    // Later packets for it: RST (done / out of the table).
    c.input(dev(hdr(OP_RW, host, me), b"late"));
    assert_eq!(ops(&c.take_out()), vec![OP_RST]);
    // Our close now: Linux resets once more (peer_shutdown is complete).
    c.release(s, 0);
    assert_eq!(ops(&c.take_out()), vec![OP_RST]);
    assert!(!c.exists(s));

    // SHUTDOWN(both) with unread data: no RST until we close.
    let (mut c, s) = setup();
    c.input(dev(hdr(OP_RW, host, me), b"unread"));
    c.input(dev(sh, &[]));
    assert!(c.take_out().is_empty());
    assert!(!c.info(s).unwrap().done);
    c.release(s, 0);
    assert_eq!(ops(&c.take_out()), vec![OP_RST]);
    assert!(!c.exists(s));

    // RST: done; data still readable, then EOF; EPIPE on write.
    let (mut c, s) = setup();
    c.input(dev(hdr(OP_RW, host, me), b"last"));
    c.input(dev(hdr(OP_RST, host, me), &[]));
    assert!(c.take_out().is_empty());
    assert_eq!(read_all(&mut c, s), b"last");
    assert_eq!(c.recv(s, &mut [0; 1], false), Ok(0));
    assert_eq!(c.send(s, b"x"), Err(e(EPIPE)));

    // Our close: SHUTDOWN(both), orphaned until the host's RST.
    let (mut c, s) = setup();
    c.release(s, 0);
    let out = c.take_out();
    assert_eq!(ops(&out), vec![OP_SHUTDOWN]);
    assert_eq!(out[0].hdr.flags, 3);
    assert!(c.info(s).unwrap().orphan);
    c.input(dev(hdr(OP_RW, host, me), b"ignored"));
    c.input(dev(hdr(OP_RST, host, me), &[]));
    assert!(!c.exists(s));
    assert_eq!(c.next_deadline(), None);
}

#[test]
fn device_transport_reset() {
    let mut c = guest();
    let host = Addr::new(HOST, 6000);
    let l = c.create();
    c.bind(l, Addr::new(VMADDR_CID_ANY, 77)).unwrap();
    c.listen(l, 8).unwrap();
    c.input(dev(hdr(OP_REQUEST, host, Addr::new(GUEST, 77)), &[]));
    c.take_out();
    let (s, _) = c.accept(l).unwrap();
    let (la, lb, _) = loop_pair(&mut c, 78, VMADDR_CID_LOCAL);
    c.take_dirty();
    c.transport_reset(Some(9));
    assert_eq!(c.guest_cid(), Some(9));
    assert_eq!(c.local_cid(), 9);
    assert!(c.take_dirty().contains(&s));
    let i = c.info(s).unwrap();
    assert_eq!((i.sk, i.err), (Sk::Close, ECONNRESET));
    assert!(c.poll(s).contains(EpollFlags::ERR));
    assert_eq!(c.recv(s, &mut [0; 1], false), Err(e(ENOTCONN)));
    assert_eq!(c.send(s, b"x"), Err(e(ENOTCONN)));
    assert_eq!(c.take_error(s), ECONNRESET);
    // Listeners and loopback connections are untouched.
    assert!(c.is_listening(l));
    assert_eq!(c.info(la).unwrap().sk, Sk::Established);
    assert_eq!(c.send(la, b"still"), Ok(5));
    pump(&mut c);
    assert_eq!(read_all(&mut c, lb), b"still");
    pump(&mut c);
    // The new CID is used from now on (a new host port: the reset socket
    // stays in the connected table until it is closed, as on Linux).
    c.input(dev(
        hdr(OP_REQUEST, Addr::new(HOST, 6001), Addr::new(9, 77)),
        &[],
    ));
    let out = c.take_out();
    assert_eq!(out[0].hdr.op, OP_RESPONSE);
    assert_eq!(out[0].hdr.src_cid, 9);
    c.release(s, 0);
    assert!(
        c.take_out().is_empty(),
        "a reset connection closes silently"
    );
}

#[test]
fn buffer_options() {
    let mut c = Core::new(1);
    let s = c.create();
    assert_eq!(c.buffer(s, 0), Ok(DEFAULT_BUFFER_SIZE));
    assert_eq!(c.buffer(s, 1), Ok(DEFAULT_BUFFER_MIN_SIZE));
    assert_eq!(c.buffer(s, 2), Ok(DEFAULT_BUFFER_MAX_SIZE));
    c.set_buffer(s, 0, 1 << 30).unwrap();
    assert_eq!(
        c.buffer(s, 0),
        Ok(DEFAULT_BUFFER_MAX_SIZE),
        "clamped to max"
    );
    c.set_buffer(s, 0, 1).unwrap();
    assert_eq!(
        c.buffer(s, 0),
        Ok(DEFAULT_BUFFER_MIN_SIZE),
        "clamped to min"
    );
    c.set_buffer(s, 2, 1 << 20).unwrap();
    c.set_buffer(s, 0, 1 << 20).unwrap();
    assert_eq!(c.buffer(s, 0), Ok(1 << 20));
    // Lowering the max pulls the size down.
    c.set_buffer(s, 2, 4096).unwrap();
    assert_eq!(c.buffer(s, 0), Ok(4096));
    assert_eq!(c.set_buffer(s, 7, 1), Err(e(ENOPROTOOPT)));
    // The advertised buf_alloc follows (and is inherited on accept).
    c.bind(s, Addr::new(VMADDR_CID_ANY, 99)).unwrap();
    c.listen(s, 4).unwrap();
    let a = c.create();
    let _ = c.connect(a, Addr::new(1, 99), 0);
    let req = c.take_out();
    for p in req {
        c.input(p);
    }
    let out = c.take_out();
    assert_eq!(out[0].hdr.op, OP_RESPONSE);
    assert_eq!(out[0].hdr.buf_alloc, 4096);
}

#[test]
fn abort_resets() {
    let mut c = Core::new(1);
    let (a, b, _) = loop_pair(&mut c, 99, VMADDR_CID_LOCAL);
    c.abort(a);
    assert!(!c.exists(a));
    pump(&mut c);
    assert!(c.info(b).unwrap().done);
    assert_eq!(c.recv(b, &mut [0; 1], false), Ok(0));
}

// ---------------------------------------------------------------------
// Guest ↔ host over the device.

#[test]
fn host_to_guest_and_back_with_split_packets() {
    let mut p = Pair::new();
    p.split = 1000;
    let gl = p.g.create();
    p.g.bind(gl, Addr::new(VMADDR_CID_ANY, 1024)).unwrap();
    p.g.listen(gl, 4).unwrap();
    let hc = p.h.create();
    assert_eq!(
        p.h.connect(hc, Addr::new(GUEST, 1024), 0),
        Err(e(EINPROGRESS))
    );
    p.run();
    assert_eq!(p.h.connect_result(hc), Some(Ok(())));
    let (gs, peer) = p.g.accept(gl).unwrap();
    assert_eq!(peer.cid, HOST);
    assert_eq!(p.g.peer_addr(gs).unwrap(), peer);
    // The host writes 64 KiB packets; the "device" splits them into
    // 1000-byte ones for the guest's buffers.
    let data = pattern(300_000, 5);
    let mut sent = 0;
    let mut got = Vec::new();
    let mut buf = vec![0u8; 4096];
    while got.len() < data.len() {
        if sent < data.len()
            && let Ok(n) = p.h.send(hc, &data[sent..])
        {
            sent += n;
        }
        p.run();
        let gi = p.g.info(gs).unwrap();
        assert!(gi.rx_bytes <= gi.buf_alloc as usize);
        if let Ok(n) = p.g.recv(gs, &mut buf, false) {
            got.extend_from_slice(&buf[..n]);
        }
        p.run();
    }
    assert_eq!(got, data);
    assert!(
        p.h2g
            .iter()
            .filter(|x| x.hdr.op == OP_RW)
            .all(|x| x.payload.len() <= 1000)
    );
    // Guest → host: device packets of at most 4052 bytes.
    let back = pattern(100_000, 6);
    let mut sent = 0;
    let mut got = Vec::new();
    while got.len() < back.len() {
        if sent < back.len()
            && let Ok(n) = p.g.send(gs, &back[sent..])
        {
            sent += n;
        }
        p.run();
        got.extend(read_all(&mut p.h, hc));
        p.run();
    }
    assert_eq!(got, back);
    assert!(
        p.g2h
            .iter()
            .filter(|x| x.hdr.op == OP_RW)
            .all(|x| x.payload.len() <= DEFAULT_DEV_PAYLOAD)
    );
    // Close from the host: the guest sees EOF, then closes; both gone.
    p.h.release(hc, 0);
    p.run();
    assert_eq!(p.g.recv(gs, &mut buf, false), Ok(0));
    p.g.release(gs, 0);
    p.run();
    assert_eq!(p.g.len(), 1);
    assert!(p.h.is_empty());
}

#[test]
fn slow_host_credit() {
    let mut p = Pair::new();
    let hl = p.h.create();
    p.h.bind(hl, Addr::new(VMADDR_CID_ANY, 2000)).unwrap();
    p.h.listen(hl, 4).unwrap();
    // The host advertises a small buffer.
    p.h.set_buffer(hl, 1, 128).unwrap();
    p.h.set_buffer(hl, 0, 8192).unwrap();
    let gs = p.g.create();
    let _ = p.g.connect(gs, Addr::new(HOST, 2000), 0);
    p.run();
    let (hs, _) = p.h.accept(hl).unwrap();
    assert_eq!(p.g.info(gs).unwrap().credit, 8192);
    let data = pattern(50_000, 8);
    let n = p.g.send(gs, &data).unwrap();
    assert_eq!(n, 8192);
    p.run();
    assert_eq!(p.g.send(gs, &data[n..]), Err(e(EAGAIN)));
    p.run();
    // The host's answer to our CREDIT_REQUEST changes nothing yet.
    assert_eq!(p.g.info(gs).unwrap().credit, 0);
    assert!(p.g2h.iter().any(|x| x.hdr.op == OP_CREDIT_REQUEST));
    let mut sent = n;
    let mut got = Vec::new();
    let mut buf = [0u8; 1500];
    while got.len() < data.len() {
        // The host reads a little at a time.
        if let Ok(k) = p.h.recv(hs, &mut buf, false) {
            got.extend_from_slice(&buf[..k]);
        }
        p.run();
        if sent < data.len()
            && let Ok(k) = p.g.send(gs, &data[sent..])
        {
            sent += k;
        }
        p.run();
        let hi = p.h.info(hs).unwrap();
        assert!(hi.rx_bytes <= 8192);
    }
    assert_eq!(got, data);
}

/// Random model run: many connections both ways over the device and the
/// loopback, random sizes, slow readers, half-closes; every byte must
/// arrive in order and no receiver may hold more than it advertised.
#[test]
fn model_random_streams() {
    struct Conn {
        tx_g: bool,
        tx: SockId,
        rx_g: bool,
        rx: SockId,
        data: Vec<u8>,
        sent: usize,
        got: Vec<u8>,
        closed: bool,
    }
    fn core(p: &mut Pair, g: bool) -> &mut Core {
        if g { &mut p.g } else { &mut p.h }
    }
    let mut rng = 0x9e3779b97f4a7c15u64;
    let mut next = move |n: u64| {
        rng ^= rng << 7;
        rng ^= rng >> 9;
        rng % n
    };
    let mut p = Pair::new();
    p.split = 2048;
    let gl = p.g.create();
    p.g.bind(gl, Addr::new(VMADDR_CID_ANY, 3000)).unwrap();
    p.g.listen(gl, 64).unwrap();
    let hl = p.h.create();
    p.h.bind(hl, Addr::new(VMADDR_CID_ANY, 3000)).unwrap();
    p.h.listen(hl, 64).unwrap();
    let mut conns = Vec::new();
    for i in 0..12u32 {
        let size = 1 + next(200_000) as usize;
        let data = pattern(size, i + 100);
        // 0: host connects to the guest, 1: guest to host, 2: guest
        // loopback (VMADDR_CID_LOCAL or the guest CID).
        let kind = i % 3;
        let (client_g, listener_g, l, addr) = match kind {
            0 => (false, true, gl, Addr::new(GUEST, 3000)),
            1 => (true, false, hl, Addr::new(HOST, 3000)),
            _ => (
                true,
                true,
                gl,
                Addr::new(if i % 2 == 0 { VMADDR_CID_LOCAL } else { GUEST }, 3000),
            ),
        };
        let c = core(&mut p, client_g).create();
        let _ = core(&mut p, client_g).connect(c, addr, 0);
        p.run();
        assert_eq!(core(&mut p, client_g).connect_result(c), Some(Ok(())));
        let (a, _) = core(&mut p, listener_g).accept(l).unwrap();
        // Alternate the direction of data.
        let (tx_g, tx, rx_g, rx) = if i % 2 == 0 {
            (client_g, c, listener_g, a)
        } else {
            (listener_g, a, client_g, c)
        };
        conns.push(Conn {
            tx_g,
            tx,
            rx_g,
            rx,
            data,
            sent: 0,
            got: Vec::new(),
            closed: false,
        });
    }
    let mut rounds = 0;
    let mut buf = vec![0u8; 70_000];
    while conns
        .iter()
        .any(|c| c.got.len() < c.data.len() || !c.closed)
    {
        rounds += 1;
        assert!(rounds < 200_000, "stalled");
        let k = next(conns.len() as u64) as usize;
        let c = &mut conns[k];
        match next(3) {
            0 if c.sent < c.data.len() => {
                let n = 1 + next(80_000) as usize;
                let end = (c.sent + n).min(c.data.len());
                match core(&mut p, c.tx_g).send(c.tx, &c.data[c.sent..end]) {
                    Ok(w) => c.sent += w,
                    Err(Errno(EAGAIN)) => {}
                    Err(x) => panic!("send {x:?}"),
                }
            }
            0 if !c.closed => {
                core(&mut p, c.tx_g).shutdown(c.tx, 1).unwrap();
                c.closed = true;
            }
            _ => {
                let n = 1 + next(buf.len() as u64) as usize;
                let rc = core(&mut p, c.rx_g);
                match rc.recv(c.rx, &mut buf[..n], false) {
                    Ok(0) => assert!(c.closed && c.got.len() == c.data.len(), "early EOF"),
                    Ok(r) => c.got.extend_from_slice(&buf[..r]),
                    Err(Errno(EAGAIN)) => {}
                    Err(x) => panic!("recv {x:?}"),
                }
                let i = rc.info(c.rx).unwrap();
                assert!(i.rx_bytes <= i.buf_alloc as usize);
            }
        }
        p.run();
    }
    for c in &conns {
        assert_eq!(c.got, c.data);
        let rc = core(&mut p, c.rx_g);
        assert_eq!(rc.recv(c.rx, &mut buf, false), Ok(0));
        assert!(rc.poll(c.rx).contains(EpollFlags::RDHUP));
        // Counters agree: every byte sent was consumed.
        let ti = core(&mut p, c.tx_g).info(c.tx).unwrap();
        let ri = core(&mut p, c.rx_g).info(c.rx).unwrap();
        assert_eq!(ti.tx_cnt, ri.fwd_cnt);
        assert_eq!(ti.tx_cnt as usize, c.data.len());
    }
    // Close everything; after the exchanges only the listeners remain.
    for c in &conns {
        core(&mut p, c.tx_g).release(c.tx, 0);
        p.run();
        core(&mut p, c.rx_g).release(c.rx, 0);
        p.run();
    }
    assert_eq!(p.g.len(), 1, "guest: only the listener");
    assert_eq!(p.h.len(), 1, "host: only the listener");
    assert_eq!(p.g.next_deadline(), None);
}

#[test]
fn peer_beyond_credit_is_dropped_and_counted() {
    let mut c = guest();
    let l = c.create();
    c.bind(l, Addr::new(VMADDR_CID_ANY, 1234)).unwrap();
    c.listen(l, 8).unwrap();
    let host = Addr::new(HOST, 40000);
    let me = Addr::new(GUEST, 1234);
    c.input(dev(hdr(OP_REQUEST, host, me), &[]));
    let (s, _) = c.accept(l).unwrap();
    c.take_out();
    // A peer ignoring the advertised buffer loses the packet that does not
    // fit (Linux `virtio_transport_inc_rx_pkt`); the kernel logs the count.
    let full = vec![7u8; DEFAULT_BUFFER_SIZE as usize];
    c.input(dev(hdr(OP_RW, host, me), &full));
    assert_eq!(c.rx_drops(), 0);
    c.input(dev(hdr(OP_RW, host, me), b"x"));
    assert_eq!(c.rx_drops(), 1);
    assert_eq!(c.rx_bytes(s), DEFAULT_BUFFER_SIZE as usize);
}
