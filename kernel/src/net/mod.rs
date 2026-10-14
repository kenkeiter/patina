//! Networking (design §9):
//!
//! * [`device`]: the driver/stack seam (`NetDevice`, `register`/`take`);
//! * [`mux`]: the smoltcp `Device` (NIC + loopback queue);
//! * [`stack`]: the interface, the socket set, the net task (D67);
//! * [`socket`]: the `SocketOps` seam the socket syscalls call;
//! * [`inet`]: AF_INET/AF_INET6 TCP and UDP sockets (D68);
//! * [`unix`]: AF_UNIX stream, datagram and seqpacket sockets (D71).
//! * [`netlink`]: read-only AF_NETLINK/NETLINK_ROUTE sockets (D73).

pub mod device;
pub mod inet;
pub mod mux;
pub mod netlink;
pub mod socket;
pub mod stack;
pub mod unix;

/// Start the stack (after `drivers::init`, which registers the NIC).
pub fn init(cmdline: &patina_cmdline::Cmdline) {
    stack::init(cmdline);
}

pub use stack::effective_config;

/// `ip=dhcp`: wait (bounded) for a lease before user mode. After
/// `ctl::init`, so a timeout also reaches the control channel.
pub fn wait_dhcp() {
    stack::wait_dhcp();
}
