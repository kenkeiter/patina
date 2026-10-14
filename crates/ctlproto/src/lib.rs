//! patina control-channel protocol (design §10.2): newline-delimited JSON
//! over virtio-vsock port 4242 or the `patina.ctl` console port.
//!
//! - [`framer::LineFramer`] turns arbitrary transport chunks into lines.
//! - [`msg::decode_request`] turns a line into a [`msg::Request`]
//!   (`ping`/`signal`/`stop`/`time`/`dump`) or a [`msg::DecodeError`] whose
//!   [`reply`](msg::DecodeError::reply) is the error event to send back.
//! - [`msg::Event::encode`] writes guest → host events
//!   (`pong`/`ready`/`exit`/`panic`/`log`/`error`).
//! - [`json`] is the minimal allocation-free JSON reader underneath.
//!
//! No allocation, no `unsafe`, and no panics on any input.
//!
//! ```
//! use patina_ctlproto::{framer::{Frame, LineFramer}, msg::{decode_request, Event, Op}};
//!
//! let mut framer = LineFramer::<1024>::new();
//! let mut out = [0u8; 256];
//! framer.feed(b"{\"op\":\"ping\",\"id\":1}\n", |frame| {
//!     if let Frame::Line(line) = frame {
//!         let reply = match decode_request(line) {
//!             Ok(req) if req.op == Op::Ping => Event::Pong { id: req.id, uptime_ms: 42 },
//!             Ok(_) => return,
//!             Err(e) => e.reply(),
//!         };
//!         let n = reply.encode_to_slice(&mut out).unwrap();
//!         assert_eq!(&out[..n], b"{\"ev\":\"pong\",\"id\":1,\"uptime_ms\":42}\n");
//!     }
//! });
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod framer;
pub mod json;
pub mod msg;

pub use framer::{Frame, LineFramer};
pub use msg::{decode_request, DecodeError, Event, Op, Request, SignalSpec};

/// vsock port the guest listens on.
pub const VSOCK_PORT: u32 = 4242;
/// Name of the virtio-console port used as a fallback transport.
pub const CONSOLE_PORT_NAME: &str = "patina.ctl";
/// Recommended maximum request line length.
pub const MAX_LINE: usize = 4096;

#[cfg(test)]
mod fuzz;
