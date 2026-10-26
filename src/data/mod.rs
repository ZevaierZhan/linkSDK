//! Atomic JSON values: local durable writes, eventual replication, inspectable conflicts.
mod error;
mod operation;
#[cfg(feature = "network")]
pub mod replication;
#[cfg_attr(not(feature = "network"), allow(dead_code))]
mod store;
mod view;
pub use error::{DataError, DataErrorKind, DataResult};
pub use operation::Operation;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};
pub use view::{
    DataStore, DatasetKey, DatasetSnapshot, Snapshot, SnapshotEvent, SnapshotReader, Subscription,
};

#[derive(Clone)]
pub struct Dataset {
    home: PathBuf,
    app: String,
    dataset: String,
    _owner: Arc<crate::platform::ProfileLock>,
}
impl Dataset {
    pub(crate) fn new(owner: Arc<crate::platform::ProfileLock>, app: &str, dataset: &str) -> Self {
        Self {
            home: owner.home.clone(),
            app: app.into(),
            dataset: dataset.into(),
            _owner: owner,
        }
    }
    fn call(&self, op: &str, key: Option<&str>, value: Option<Value>) -> Result<Value, String> {
        let mut request = json!({"op":op,"app":self.app,"dataset":self.dataset});
        if let Some(key) = key {
            request["key"] = json!(key);
        }
        if let Some(value) = value {
            request["value"] = value;
        }
        action_at(&self.home, request)
    }
    pub fn put(&self, key: &str, value: Value) -> Result<Value, String> {
        self.call("put", Some(key), Some(value))
    }
    pub fn get(&self, key: &str) -> Result<Value, String> {
        self.call("get", Some(key), None)
    }
    pub fn list(&self) -> Result<Value, String> {
        self.call("list", None, None)
    }
    pub fn delete(&self, key: &str) -> Result<Value, String> {
        self.call("delete", Some(key), None)
    }
    pub fn heads(&self) -> Result<Value, String> {
        self.call("heads", None, None)
    }
    pub fn status(&self) -> Result<Value, String> {
        self.call("status", None, None)
    }
}

/// Adapter for a host that already owns profile lifecycle and permits serialized CLI writers.
/// Keeps the original space/shared-data.sqlite layout; does not take a second profile lock.
pub fn action_at(home: impl AsRef<std::path::Path>, request: Value) -> Result<Value, String> {
    try_action_at(home, request, &Operation::default()).map_err(Into::into)
}

/// Storage-only adapter for a host-owned verified public space context.
/// The caller owns directory selection/permissions; no identity or network is created.
pub fn action_in(
    directory: impl AsRef<std::path::Path>,
    network_id: u64,
    request: Value,
) -> Result<Value, String> {
    try_action_in(directory, network_id, request, &Operation::default()).map_err(Into::into)
}

/// Typed adapters retain transient storage errors for host policy. Legacy adapters format at the edge.
pub fn try_action_at(
    home: impl AsRef<std::path::Path>,
    request: Value,
    op: &Operation,
) -> DataResult<Value> {
    DataStore::open_at(home, op)?.action(request, op)
}
pub fn try_action_in(
    directory: impl AsRef<std::path::Path>,
    network_id: u64,
    request: Value,
    op: &Operation,
) -> DataResult<Value> {
    DataStore::open_in(directory, network_id, op)?.action(request, op)
}
