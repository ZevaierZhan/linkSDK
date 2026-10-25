//! One libzt node and active space per process. No official Planet fallback.
pub mod address;
pub mod host;
/// Escape hatch for host-owned UDP/protocol adapters; uses the same native instance.
pub use libzt as raw;
pub mod frame;
mod node;
mod socket;
pub use node::{Bootstrap, Node, Peer};
pub use socket::{
    checked_io_count, io_diagnostic, retryable, socket_errno, SocketIo, ZtSocket as Stream,
};
