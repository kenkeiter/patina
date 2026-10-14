//! Networking (design §9):
//!
//! * [`device`]: the driver/stack seam (`NetDevice`, `register`/`take`);
//! * [`mux`]: the smoltcp `Device` (NIC + loopback queue);
//! * [`stack`]: the interface, the socket set, the net task (D67);
//! * [`socket`]: the `SocketOps` seam the socket syscalls call;
//! * [`inet`]: AF_INET/AF_INET6 TCP and UDP sockets (D68);
//! * [`unix`]: AF_UNIX stream, datagram and seqpacket sockets (D71).

pub mod device;
pub mod inet;
pub mod mux;
pub mod socket;
pub mod stack;
pub mod unix;

/// Start the stack (after `drivers::init`, which registers the NIC).
pub fn init(cmdline: &patina_cmdline::Cmdline) {
    stack::init(cmdline);
}
