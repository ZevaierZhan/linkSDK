//! Atomic JSON values: local durable writes, eventual replication, inspectable conflicts.
#[cfg(feature = "network")]
pub mod replication;
#[cfg_attr(not(feature = "network"), allow(dead_code))]
mod store;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

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
        store::action(self.home.clone(), request)
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
