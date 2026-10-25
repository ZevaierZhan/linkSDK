//! Embedded, UI-independent P2P networking and durable shared JSON registers.
#[cfg(feature = "data")]
pub mod data;
#[cfg(feature = "network")]
pub mod network;
pub mod platform;
pub mod runtime;

use serde::{Deserialize, Serialize};

/// Kept separate from SDK version and wire/storage format versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpaceConfig {
    pub network_id: u64,
}

#[cfg_attr(not(feature = "network"), allow(dead_code))]
pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
