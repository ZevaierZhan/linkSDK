//! Storage-only repro: two real SQLite connections, no libzt/helper/TUN or user profiles.
use link_sdk::data::{DataErrorKind, DataStore, DatasetKey, Operation};
use serde_json::json;
use std::time::Duration;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).map_err(|e| std::io::Error::other(e.to_string()))?;
    let directory = std::env::temp_dir().join(format!("linksdk-resilience-{:x?}", id));
    std::fs::create_dir(&directory)?;
    let op = || Operation::new(Duration::from_millis(100));
    let store = DataStore::open_in(&directory, 42, &op())?;
    let put =
        |value| json!({"op":"put","app":"demo","dataset":"rules","key":"route","value":value});
    store.action(put(1), &op())?;
    let keys = vec![DatasetKey::new("demo", "rules")];
    let before = store.snapshot(&keys, &op())?;
    let locker = rusqlite::Connection::open(directory.join("shared-data.sqlite"))?;
    locker.execute_batch("BEGIN IMMEDIATE")?;
    let blocked = store.action(put(2), &op()).unwrap_err();
    assert_eq!(blocked.kind, DataErrorKind::Busy);
    let during = store.snapshot(&keys, &op())?;
    assert_eq!(before.version, during.version);
    assert_eq!(during.datasets[0].entries["route"]["value"], 1);
    locker.execute_batch("ROLLBACK")?;
    store.action(put(2), &op())?;
    let after = store.snapshot(&keys, &op())?;
    assert_ne!(after.version, before.version);
    drop(locker);
    std::fs::remove_dir_all(&directory)?;
    println!(
        "{}",
        json!({"result":"passed","readWhileWriteLocked":true,"blockedWriteKind":blocked.kind,"noFalseWriteAcknowledgement":true,"recoversAfterUnlock":true,"wireVersion":2,"storageVersion":1})
    );
    Ok(())
}
