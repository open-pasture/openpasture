//! A real OpenCollar on USB serial, talking to an openpasture server: the
//! bench bridge, capture and replay, and the factory test station. A dev,
//! bench and factory tool outside the server, like collar-sim; it only ever
//! talks to real hardware.

pub mod bridge;
pub mod capture;
pub mod collar;
pub mod console;
pub mod factory;
pub mod link;
pub mod replay;
pub mod serial;
pub mod session;
pub mod uplink;
