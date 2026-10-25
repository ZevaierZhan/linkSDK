//! One libzt node and active space per process. No official Planet fallback.
pub(crate) mod address;
pub mod frame;
mod node;
mod socket;
pub use node::{Bootstrap, Node, Peer};
pub use socket::{io_diagnostic, SocketIo, ZtSocket as Stream};
