#![cfg(feature = "data")]
use link_sdk::data::action_in;
use serde_json::json;
use std::time::{Duration, Instant};
fn directory() -> std::path::PathBuf {
    let mut id = [0; 12];
    getrandom::fill(&mut id).unwrap();
    let path = std::env::temp_dir().join(format!("linksdk-contention-{:x?}", id));
    std::fs::create_dir_all(&path).unwrap();
    path
}
#[test]
fn initialized_reads_do_not_take_a_writer_lock() {
    let dir = directory();
    action_in(
        &dir,
        42,
        json!({"op":"put","app":"test","dataset":"rules","key":"one","value":7}),
    )
    .unwrap();
    let holder = rusqlite::Connection::open(dir.join("shared-data.sqlite")).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    let result = action_in(
        &dir,
        42,
        json!({"op":"get","app":"test","dataset":"rules","key":"one"}),
    );
    holder.execute_batch("ROLLBACK").unwrap();
    assert_eq!(result.unwrap()["one"]["value"], 7);
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(holder);
    std::fs::remove_dir_all(dir).unwrap();
}

use link_sdk::data::{DataErrorKind, DataStore, DatasetKey, Operation, SnapshotEvent};
use std::{
    io::{Read, Write},
    process::{Child, Stdio},
};
fn op() -> Operation {
    Operation::new(Duration::from_secs(1))
}
fn request(operation: &str, value: i32) -> serde_json::Value {
    json!({"op":operation,"app":"test","dataset":"rules","key":"one","value":value})
}
struct Fixture {
    dir: std::path::PathBuf,
    store: DataStore,
}
impl Fixture {
    fn new() -> Self {
        let dir = directory();
        let store = DataStore::open_in(&dir, 42, &op()).unwrap();
        store.action(request("put", 7), &op()).unwrap();
        Self { dir, store }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
struct Held {
    child: Child,
}
impl Held {
    fn start(dir: &std::path::Path, kind: &str) -> Self {
        let ready = dir.join(format!("ready-{kind}"));
        let _ = std::fs::remove_file(&ready);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "child_storage_process",
                "--nocapture",
            ])
            .env("LINKSDK_CHILD_DIRECTORY", dir)
            .env("LINKSDK_CHILD_KIND", kind)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let held = Self { child };
        let start = Instant::now();
        while !ready.exists() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "child did not become ready"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        held
    }
    fn release(&mut self) {
        if let Some(mut stdin) = self.child.stdin.take() {
            let _ = stdin.write_all(b"x");
        }
        assert!(self.child.wait().unwrap().success());
    }
}
impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[test]
#[ignore = "subprocess fixture invoked by parent contention tests"]
fn child_storage_process() {
    let dir = std::path::PathBuf::from(std::env::var_os("LINKSDK_CHILD_DIRECTORY").unwrap());
    let kind = std::env::var("LINKSDK_CHILD_KIND").unwrap();
    if kind == "write" {
        action_in(&dir, 42, request("put", 19)).unwrap();
        std::fs::write(dir.join("ready-write"), b"ready").unwrap();
        return;
    }
    let db = rusqlite::Connection::open(dir.join("shared-data.sqlite")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE; INSERT INTO shared_meta VALUES('uncommitted-test','value')")
        .unwrap();
    std::fs::write(dir.join("ready-lock"), b"ready").unwrap();
    let mut byte = [0];
    std::io::stdin().read_exact(&mut byte).unwrap();
    db.execute_batch("ROLLBACK").unwrap();
}
#[test]
fn another_process_cannot_block_committed_reads_or_produce_a_false_write_ack() {
    let f = Fixture::new();
    let mut held = Held::start(&f.dir, "lock");
    for operation in ["get", "list", "heads", "status"] {
        assert!(
            f.store.action(request(operation, 0), &op()).is_ok(),
            "{operation}"
        );
    }
    let e = f.store.action(request("put", 8), &op()).unwrap_err();
    assert_eq!(e.kind, DataErrorKind::Busy);
    assert_eq!(e.sqlite_code, Some(5));
    assert!(e.is_retryable());
    assert_eq!(
        f.store.action(request("get", 0), &op()).unwrap()["one"]["value"],
        7
    );
    held.release();
    assert_eq!(
        f.store.action(request("get", 0), &op()).unwrap()["one"]["value"],
        7,
        "no hidden pending write"
    );
    assert_eq!(
        f.store.action(request("put", 8), &op()).unwrap()["saved"],
        "local"
    );
}
#[test]
fn lock_release_allows_bounded_retry_and_preserves_actor_and_schema() {
    let f = Fixture::new();
    let db = rusqlite::Connection::open(f.dir.join("shared-data.sqlite")).unwrap();
    let actor: String = db
        .query_row("select value from shared_meta where key='actor'", [], |r| {
            r.get(0)
        })
        .unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let worker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        db.execute_batch("ROLLBACK").unwrap();
    });
    f.store
        .action(request("put", 9), &op().with_max_attempts(20))
        .unwrap();
    worker.join().unwrap();
    let db = rusqlite::Connection::open(f.dir.join("shared-data.sqlite")).unwrap();
    assert_eq!(
        actor,
        db.query_row("select value from shared_meta where key='actor'", [], |r| r
            .get::<_, String>(0))
            .unwrap()
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn deadlines_and_cancellation_bound_contention_and_never_queue_writes() {
    let f = Fixture::new();
    let db = rusqlite::Connection::open(f.dir.join("shared-data.sqlite")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    let e = f
        .store
        .action(
            request("put", 99),
            &Operation::new(Duration::from_millis(20)).with_max_attempts(128),
        )
        .unwrap_err();
    assert_eq!(e.kind, DataErrorKind::DeadlineExceeded);
    assert!(start.elapsed() < Duration::from_secs(1));
    let control = op().with_max_attempts(128);
    let cancel = control.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        cancel.cancel();
    });
    assert_eq!(
        f.store
            .action(request("put", 99), &control)
            .unwrap_err()
            .kind,
        DataErrorKind::Cancelled
    );
    t.join().unwrap();
    db.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        f.store.action(request("get", 0), &op()).unwrap()["one"]["value"],
        7
    );
}
#[test]
fn stale_versions_are_not_automatically_replayed_and_capacity_is_distinct() {
    let f = Fixture::new();
    let old = f.store.action(request("get", 0), &op()).unwrap();
    let versions = old["one"]["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["version"].clone())
        .collect::<Vec<_>>();
    f.store.action(request("put", 8), &op()).unwrap();
    let mut stale = request("put", 9);
    stale["expectedVersions"] = json!(versions);
    let e = f
        .store
        .action(stale, &op().with_max_attempts(128))
        .unwrap_err();
    assert_eq!(e.kind, DataErrorKind::StaleVersion);
    assert!(!e.is_retryable());
    f.store
        .action(json!({"op":"limits","maxBytes":1024}), &op())
        .unwrap();
    let mut huge = request("put", 9);
    let mut bytes = vec![0u8; 8192];
    getrandom::fill(&mut bytes).unwrap();
    huge["value"] = json!(bytes);
    assert_eq!(
        f.store.action(huge, &op()).unwrap_err().kind,
        DataErrorKind::CapacityExceeded
    );
    assert_eq!(
        f.store.action(request("get", 0), &op()).unwrap()["one"]["value"],
        8
    );
}
#[test]
fn snapshots_have_stable_versions_and_explicit_stale_state_on_failure() {
    let f = Fixture::new();
    let keys = vec![DatasetKey::new("test", "rules")];
    let mut reader = f.store.reader(keys.clone()).unwrap();
    let first = f.store.snapshot(&keys, &op()).unwrap();
    let second = f.store.snapshot(&keys, &op()).unwrap();
    assert_eq!(first.version, second.version);
    assert!(matches!(
        reader.refresh(&op()),
        SnapshotEvent::Fresh { changed: true, .. }
    ));
    assert!(matches!(
        reader.refresh(&op()),
        SnapshotEvent::Fresh { changed: false, .. }
    ));
    let cancelled = op();
    cancelled.cancel();
    match reader.refresh(&cancelled) {
        SnapshotEvent::Unavailable {
            last_good: Some(last),
            error,
        } => {
            assert_eq!(last.version, first.version);
            assert_eq!(error.kind, DataErrorKind::Cancelled);
        }
        e => panic!("{e:?}"),
    }
    f.store.action(request("put", 8), &op()).unwrap();
    match reader.refresh(&op()) {
        SnapshotEvent::Fresh {
            snapshot,
            changed: true,
        } => {
            assert_ne!(snapshot.version, first.version);
            assert_eq!(snapshot.datasets[0].entries["one"]["value"], 8);
        }
        e => panic!("{e:?}"),
    }
    let cold = DataStore::open_in(&f.dir, 42, &op()).unwrap();
    let mut cold_reader = cold.reader(keys).unwrap();
    assert!(matches!(
        cold_reader.refresh(&cancelled),
        SnapshotEvent::Unavailable {
            last_good: None,
            ..
        }
    ));
}
#[test]
fn snapshot_selection_is_atomic_across_datasets_during_external_commits() {
    let f = Fixture::new();
    let keys = vec![
        DatasetKey::new("test", "rules"),
        DatasetKey::new("test", "exits"),
    ];
    let mut req = request("put", 7);
    req["dataset"] = json!("exits");
    f.store.action(req, &op()).unwrap();
    let db = rusqlite::Connection::open(f.dir.join("shared-data.sqlite")).unwrap();
    let seven: Vec<u8> = db
        .query_row(
            "select doc from shared_docs where dataset='rules'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    f.store.action(request("put", 8), &op()).unwrap();
    let eight: Vec<u8> = db
        .query_row(
            "select doc from shared_docs where dataset='rules'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    db.execute("update shared_docs set doc=?1", [&seven])
        .unwrap();
    let dir = f.dir.clone();
    let worker = std::thread::spawn(move || {
        let mut db = rusqlite::Connection::open(dir.join("shared-data.sqlite")).unwrap();
        db.busy_timeout(Duration::from_secs(1)).unwrap();
        for i in 0..50 {
            let tx = db.transaction().unwrap();
            tx.execute(
                "update shared_docs set doc=?1",
                [if i % 2 == 0 { &eight } else { &seven }],
            )
            .unwrap();
            tx.commit().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    for _ in 0..50 {
        let s = f.store.snapshot(&keys, &op()).unwrap();
        assert_eq!(
            s.datasets[0].entries["one"]["value"],
            s.datasets[1].entries["one"]["value"]
        );
    }
    worker.join().unwrap();
}
#[test]
fn bounded_subscription_recovers_local_and_other_process_changes() {
    let f = Fixture::new();
    let events = f
        .store
        .subscribe(
            vec![DatasetKey::new("test", "rules")],
            Duration::from_millis(20),
        )
        .unwrap();
    assert!(matches!(
        events.recv_timeout(Duration::from_secs(2)).unwrap(),
        SnapshotEvent::Fresh { .. }
    ));
    for value in 8..15 {
        f.store.action(request("put", value), &op()).unwrap();
    }
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < end);
        if let SnapshotEvent::Fresh { snapshot, .. } =
            events.recv_timeout(Duration::from_secs(1)).unwrap()
        {
            if snapshot.datasets[0].entries["one"]["value"] == 14 {
                break;
            }
        }
    }
    let mut child = Held::start(&f.dir, "write");
    child.release();
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < end);
        if let SnapshotEvent::Fresh { snapshot, .. } =
            events.recv_timeout(Duration::from_secs(1)).unwrap()
        {
            if snapshot.datasets[0].entries["one"]["value"] == 19 {
                break;
            }
        }
    }
    let start = Instant::now();
    drop(events);
    assert!(start.elapsed() < Duration::from_secs(1));
}
#[test]
fn future_format_and_wrong_space_are_typed_and_never_repaired() {
    let f = Fixture::new();
    assert_eq!(
        DataStore::open_in(&f.dir, 43, &op()).err().unwrap().kind,
        DataErrorKind::SpaceMismatch
    );
    let db = rusqlite::Connection::open(f.dir.join("shared-data.sqlite")).unwrap();
    db.pragma_update(None, "user_version", 999).unwrap();
    assert_eq!(
        f.store.action(request("get", 0), &op()).unwrap_err().kind,
        DataErrorKind::UnsupportedFormat
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        999
    );
}
#[test]
fn corrupt_store_and_missing_actor_do_not_create_a_new_replica() {
    let f = Fixture::new();
    let db = rusqlite::Connection::open(f.dir.join("shared-data.sqlite")).unwrap();
    db.execute("delete from shared_meta where key='actor'", [])
        .unwrap();
    assert_eq!(
        DataStore::open_in(&f.dir, 42, &op()).err().unwrap().kind,
        DataErrorKind::CorruptStorage
    );
    assert_eq!(
        db.query_row(
            "select count(*) from shared_meta where key='actor'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    drop(db);
    let dir = directory();
    std::fs::write(dir.join("shared-data.sqlite"), b"not SQLite").unwrap();
    assert_eq!(
        DataStore::open_in(&dir, 42, &op()).err().unwrap().kind,
        DataErrorKind::CorruptStorage
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn subscriptions_report_failure_then_recovery_without_discarding_last_good() {
    let f = Fixture::new();
    let events = f
        .store
        .subscribe(
            vec![DatasetKey::new("test", "rules")],
            Duration::from_millis(10),
        )
        .unwrap();
    let first = match events.recv_timeout(Duration::from_secs(2)).unwrap() {
        SnapshotEvent::Fresh { snapshot, .. } => snapshot,
        e => panic!("{e:?}"),
    };
    let db = rusqlite::Connection::open(f.dir.join("shared-data.sqlite")).unwrap();
    db.pragma_update(None, "user_version", 999).unwrap();
    match events.recv_timeout(Duration::from_secs(2)).unwrap() {
        SnapshotEvent::Unavailable {
            last_good: Some(s),
            error,
        } => {
            assert_eq!(s.version, first.version);
            // Worker may have successfully re-read the same version after the
            // first event was queued. A valid newer observation is retained.
            assert!(s.observed_at_ms >= first.observed_at_ms);
            assert_eq!(error.kind, DataErrorKind::UnsupportedFormat);
        }
        e => panic!("{e:?}"),
    }
    db.pragma_update(None, "user_version", 1).unwrap();
    match events.recv_timeout(Duration::from_secs(2)).unwrap() {
        SnapshotEvent::Fresh { snapshot, .. } => assert_eq!(snapshot.version, first.version),
        e => panic!("{e:?}"),
    }
}
#[test]
fn simultaneous_apps_serialize_writes_but_readers_remain_independent() {
    let f = Fixture::new();
    let mut workers = Vec::new();
    for app in 0..6 {
        let store = f.store.clone();
        workers.push(std::thread::spawn(move||{for key in 0..10 {store.action(json!({"op":"put","app":format!("app{app}"),"dataset":"rules","key":key.to_string(),"value":key}),&Operation::new(Duration::from_secs(2)).with_max_attempts(128)).unwrap();}}));
    }
    for _ in 0..10 {
        assert_eq!(
            f.store.action(request("get", 0), &op()).unwrap()["one"]["value"],
            7
        );
    }
    for worker in workers {
        worker.join().unwrap();
    }
    let keys = (0..6)
        .map(|app| DatasetKey::new(format!("app{app}"), "rules"))
        .collect::<Vec<_>>();
    let snapshot = f.store.snapshot(&keys, &op()).unwrap();
    for d in &snapshot.datasets {
        assert_eq!(d.entries.as_object().unwrap().len(), 10);
    }
}

#[test]
fn an_exclusive_reader_lock_reports_busy_with_the_last_verified_snapshot() {
    let f = Fixture::new();
    let mut reader = f
        .store
        .reader(vec![DatasetKey::new("test", "rules")])
        .unwrap();
    let before = match reader.refresh(&op()) {
        SnapshotEvent::Fresh { snapshot, .. } => snapshot,
        e => panic!("{e:?}"),
    };
    // A valid rollback-journal store makes an exclusive read lock reproducible.
    // Normal WAL BEGIN IMMEDIATE contention is separately proven not to block reads.
    let db = rusqlite::Connection::open(f.dir.join("shared-data.sqlite")).unwrap();
    db.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE")
        .unwrap();
    match reader.refresh(&op()) {
        SnapshotEvent::Unavailable {
            last_good: Some(s),
            error,
        } => {
            assert_eq!(error.kind, DataErrorKind::Busy);
            assert_eq!(s.version, before.version);
            assert_eq!(s.observed_at_ms, before.observed_at_ms);
        }
        e => panic!("{e:?}"),
    }
    db.execute_batch("ROLLBACK; PRAGMA journal_mode=WAL")
        .unwrap();
    assert!(matches!(
        reader.refresh(&op()),
        SnapshotEvent::Fresh { changed: false, .. }
    ));
}

#[test]
fn invalid_requests_are_not_misclassified_as_readonly_storage() {
    let f = Fixture::new();
    for request in [json!({}), json!({"op":"unknown"})] {
        let e = f.store.action(request, &op()).unwrap_err();
        assert_eq!(e.kind, DataErrorKind::InvalidInput);
        assert!(!e.is_retryable());
    }
}

#[test]
#[cfg(unix)]
fn an_existing_readonly_database_can_be_opened_for_reads() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let path = f.dir.join("shared-data.sqlite");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    let result = DataStore::open_in(&f.dir, 42, &op())
        .and_then(|store| store.action(request("get", 0), &op()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(result.unwrap()["one"]["value"], 7);
}

#[test]
fn legacy_initialization_rejects_wrong_space_before_committing_metadata() {
    let f = Fixture::new();
    let db = rusqlite::Connection::open(f.dir.join("shared-data.sqlite")).unwrap();
    let actor: String = db
        .query_row("select value from shared_meta where key='actor'", [], |r| {
            r.get(0)
        })
        .unwrap();
    db.pragma_update(None, "user_version", 0).unwrap();
    assert_eq!(
        DataStore::open_in(&f.dir, 43, &op()).err().unwrap().kind,
        DataErrorKind::SpaceMismatch
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let reopened = DataStore::open_in(&f.dir, 42, &op()).unwrap();
    assert_eq!(
        reopened.action(request("get", 0), &op()).unwrap()["one"]["value"],
        7
    );
    assert_eq!(
        actor,
        db.query_row("select value from shared_meta where key='actor'", [], |r| r
            .get::<_, String>(0))
            .unwrap()
    );
    db.execute_batch("DELETE FROM shared_meta WHERE key='actor'; PRAGMA user_version=0;")
        .unwrap();
    assert_eq!(
        DataStore::open_in(&f.dir, 42, &op()).err().unwrap().kind,
        DataErrorKind::CorruptStorage
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
