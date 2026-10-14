//! Networking (design §9): the driver/stack seam ([`device`]); the smoltcp
//! interface task, sockets and `AF_UNIX` land here in Phase 4.

pub mod device;
