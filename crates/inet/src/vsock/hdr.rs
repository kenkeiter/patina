//! The virtio-vsock packet header (`struct virtio_vsock_hdr`, virtio 1.2
//! §5.10.6, 44 bytes, little-endian, packed) and its constants.

/// Size of `struct virtio_vsock_hdr`.
pub const HDR_LEN: usize = 44;

/// `virtio_vsock_type`.
pub const TYPE_STREAM: u16 = 1;
pub const TYPE_SEQPACKET: u16 = 2;

/// `virtio_vsock_op`.
pub const OP_INVALID: u16 = 0;
pub const OP_REQUEST: u16 = 1;
pub const OP_RESPONSE: u16 = 2;
pub const OP_RST: u16 = 3;
pub const OP_SHUTDOWN: u16 = 4;
pub const OP_RW: u16 = 5;
pub const OP_CREDIT_UPDATE: u16 = 6;
pub const OP_CREDIT_REQUEST: u16 = 7;

/// `OP_SHUTDOWN` flags (`virtio_vsock_shutdown`); the same bit values as
/// the kernel's `RCV_SHUTDOWN` / `SEND_SHUTDOWN`.
pub const SHUTDOWN_RCV: u32 = 1;
pub const SHUTDOWN_SEND: u32 = 2;

/// Event queue: `VIRTIO_VSOCK_EVENT_TRANSPORT_RESET`.
pub const EVENT_TRANSPORT_RESET: u32 = 0;

/// Feature bit `VIRTIO_VSOCK_F_STREAM` (virtio 1.2; a device offering no
/// type feature supports streams only).
pub const F_STREAM: u64 = 1 << 0;

/// One packet header. CIDs are 64-bit on the wire; only the low 32 bits
/// are used (Linux `vsock_addr_init` takes a `u32`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hdr {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    pub ty: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

impl Hdr {
    /// Decode the first [`HDR_LEN`] bytes of `b` (`None` if shorter).
    pub fn decode(b: &[u8]) -> Option<Hdr> {
        let b = b.get(..HDR_LEN)?;
        let u64_ = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let u32_ = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u16_ = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
        Some(Hdr {
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
        })
    }

    pub fn encode(&self) -> [u8; HDR_LEN] {
        let mut b = [0u8; HDR_LEN];
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

    /// The header of a reply to `self` (addresses swapped), like Linux
    /// `virtio_transport_reset_no_sock`.
    pub fn reply(&self, op: u16) -> Hdr {
        Hdr {
            src_cid: self.dst_cid,
            dst_cid: self.src_cid,
            src_port: self.dst_port,
            dst_port: self.src_port,
            ty: self.ty,
            op,
            ..Hdr::default()
        }
    }
}

/// Name of an op for logs.
pub fn op_name(op: u16) -> &'static str {
    match op {
        OP_REQUEST => "REQUEST",
        OP_RESPONSE => "RESPONSE",
        OP_RST => "RST",
        OP_SHUTDOWN => "SHUTDOWN",
        OP_RW => "RW",
        OP_CREDIT_UPDATE => "CREDIT_UPDATE",
        OP_CREDIT_REQUEST => "CREDIT_REQUEST",
        _ => "INVALID",
    }
}
