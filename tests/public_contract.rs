#![cfg(feature = "data")]
use link_sdk::{runtime::Sdk, SpaceConfig};
use serde_json::json;
fn home(label: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::path::PathBuf::from("artifacts/contracts")
        .join(format!("{label}-{}-{nonce}", std::process::id()))
}
#[test]
fn offline_public_contract_persists_and_rejects_another_space() {
    let home = home("offline");
    {
        let sdk = Sdk::open_replica(&home, SpaceConfig { network_id: 42 }).unwrap();
        let notes = sdk.dataset("notes.v1", "items");
        assert_eq!(
            notes.put("key", json!({"body":"offline"})).unwrap(),
            json!({"saved":"local","replicated":false})
        );
        assert_eq!(notes.get("key").unwrap()["key"]["conflict"], false);
    }
    assert!(Sdk::open_replica(&home, SpaceConfig { network_id: 43 }).is_err());
    let sdk = Sdk::open(&home).unwrap();
    assert_eq!(
        sdk.dataset("notes.v1", "items").get("key").unwrap()["key"]["value"],
        json!({"body":"offline"})
    );
}
#[test]
fn dataset_handle_keeps_profile_exclusive_after_sdk_handle_drops() {
    let home = home("lock");
    let sdk = Sdk::open_replica(&home, SpaceConfig { network_id: 42 }).unwrap();
    let dataset = sdk.dataset("notes.v1", "items");
    drop(sdk);
    assert!(Sdk::open(&home).is_err());
    dataset.put("retained", json!(1)).unwrap();
    drop(dataset);
    assert!(Sdk::open(&home).is_ok());
}
#[test]
fn unknown_storage_version_is_not_rewritten() {
    let home = home("future");
    let sdk = Sdk::open_replica(&home, SpaceConfig { network_id: 42 }).unwrap();
    let path = home.join("space/shared-data.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.pragma_update(None, "user_version", 999).unwrap();
    drop(db);
    assert!(sdk
        .dataset("notes.v1", "items")
        .put("lost", json!(true))
        .is_err());
    let db = rusqlite::Connection::open(path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        999
    );
}
