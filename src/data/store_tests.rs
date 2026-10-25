use super::*;

const N: u64 = 42;
fn random_value() -> Value {
    let mut bytes = vec![0u8; 3000];
    getrandom::fill(&mut bytes).unwrap();
    json!(B64.encode(bytes))
}
fn store(name: &str) -> Store {
    let mut db = Connection::open_in_memory().unwrap();
    Store::initialize(&mut db).unwrap();
    Store {
        db,
        actor: ActorId::from(name.as_bytes()),
    }
}
fn call(s: &mut Store, op: &str, app: &str, key: &str, value: Value) -> Result<Value, String> {
    local_action(
        s,
        N,
        json!({"op":op,"app":app,"dataset":"notes","key":key,"value":value}),
    )
}
fn put(s: &mut Store, key: &str, value: Value) {
    assert_eq!(
        call(s, "put", "test.v1", key, value).unwrap()["saved"],
        "local"
    );
}
fn entries(s: &mut Store) -> Value {
    call(s, "list", "test.v1", "", Value::Null).unwrap()
}
fn heads(s: &mut Store) -> Value {
    call(s, "heads", "test.v1", "", Value::Null).unwrap()
}
fn step(
    s: &mut Store,
    peer: &str,
    states: &mut States,
    input: Vec<SyncReplica>,
) -> Vec<SyncReplica> {
    s.sync_step(N, peer, states, input, &mut Metrics::default())
        .unwrap()
}
fn copy_packet(p: &[SyncReplica]) -> Vec<SyncReplica> {
    serde_json::from_value(serde_json::to_value(p).unwrap()).unwrap()
}
fn repair(a: &mut Store, b: &mut Store) {
    let (mut sa, mut sb) = (States::new(), States::new());
    let mut packet = step(a, "b", &mut sa, vec![]);
    for _ in 0..32 {
        let reply = step(b, "a", &mut sb, packet);
        packet = step(a, "b", &mut sa, reply);
        if packet.iter().all(|r| r.message.is_none()) && heads(a) == heads(b) {
            return;
        }
    }
    panic!("repair did not converge");
}

#[test]
fn capacity_rejects_atomically_and_recovers_without_history_loss() {
    let mut s = store("a");
    local_action(
        &mut s,
        N,
        json!({"op":"limits","maxBytes":1024,"maxDatasets":1}),
    )
    .unwrap();
    put(&mut s, "old", json!("retained"));
    let before = heads(&mut s);
    assert!(call(&mut s, "put", "test.v1", "large", random_value()).is_err());
    assert_eq!(heads(&mut s), before);
    assert!(call(&mut s, "put", "other.v1", "key", json!(1)).is_err());
    assert!(local_action(&mut s, N, json!({"op":"limits","maxBytes":1})).is_err());
    assert_eq!(Store::limits(&s.db).unwrap(), (1024, 1));
    local_action(
        &mut s,
        N,
        json!({"op":"limits","maxBytes":16384,"maxDatasets":2}),
    )
    .unwrap();
    put(&mut s, "large", random_value());
    call(&mut s, "put", "other.v1", "key", json!(1)).unwrap();
    assert_eq!(entries(&mut s)["old"]["value"], "retained");
    assert!(call(
        &mut s,
        "put",
        "test.v1",
        "too-large",
        json!("x".repeat(65536))
    )
    .is_err());
}

#[test]
fn history_larger_than_default_eight_mib_repairs_after_expansion() {
    let (mut a, mut b) = (store("a"), store("b"));
    let mut doc = AutoCommit::new();
    doc.set_actor(a.actor.clone());
    for i in 0..190 {
        let mut bytes = vec![0u8; 48000];
        getrandom::fill(&mut bytes).unwrap();
        // 64,000 bytes plus JSON quotes is within the 64 KiB atomic value limit.
        doc.put(
            ROOT,
            format!("key-{i}"),
            serde_json::to_string(&B64.encode(bytes)).unwrap(),
        )
        .unwrap();
    }
    assert!(doc.save().len() > MAX);
    assert!(Store::save(&a.db, N, "test.v1", "notes", &mut doc).is_err());
    local_action(&mut a, N, json!({"op":"limits","maxBytes":16*1024*1024})).unwrap();
    Store::save(&a.db, N, "test.v1", "notes", &mut doc).unwrap();
    local_action(&mut b, N, json!({"op":"limits","maxBytes":16*1024*1024})).unwrap();
    repair(&mut a, &mut b);
    assert_eq!(heads(&mut a), heads(&mut b));
    assert_eq!(entries(&mut b).as_object().unwrap().len(), 190);
    // New empty replica can bootstrap from an ordinary holder, with saved peer state.
    let mut fresh = store("new-incarnation");
    local_action(
        &mut fresh,
        N,
        json!({"op":"limits","maxBytes":16*1024*1024}),
    )
    .unwrap();
    repair(&mut b, &mut fresh);
    assert_eq!(heads(&mut b), heads(&mut fresh));
}

#[test]
fn rejected_merge_keeps_document_safe_state_and_metrics_then_retries() {
    let (mut a, mut b) = (store("a"), store("b"));
    put(&mut a, "baseline", json!(0));
    repair(&mut a, &mut b);
    local_action(&mut b, N, json!({"op":"limits","maxBytes":1024})).unwrap();
    put(&mut a, "new", random_value());
    let (mut sa, mut sb) = (States::new(), States::new());
    let mut p = step(&mut a, "b", &mut sa, vec![]);
    let mut failed = false;
    for _ in 0..16 {
        let doc_before = heads(&mut b);
        let state_before: Vec<_> = sb.iter().map(|(k, s)| (k.clone(), s.encode())).collect();
        let persisted: Vec<Vec<u8>> =
            b.db.prepare("SELECT state FROM shared_sync ORDER BY peer,app,dataset")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
        let mut m = Metrics::default();
        match b.sync_step(N, "a", &mut sb, p, &mut m) {
            Ok(reply) => p = step(&mut a, "b", &mut sa, reply),
            Err(e) => {
                assert!(e.contains("容量"), "{e}");
                assert_eq!(heads(&mut b), doc_before);
                assert_eq!(
                    sb.iter()
                        .map(|(k, s)| (k.clone(), s.encode()))
                        .collect::<Vec<_>>(),
                    state_before
                );
                let after: Vec<Vec<u8>> =
                    b.db.prepare("SELECT state FROM shared_sync ORDER BY peer,app,dataset")
                        .unwrap()
                        .query_map([], |r| r.get(0))
                        .unwrap()
                        .map(Result::unwrap)
                        .collect();
                assert_eq!(after, persisted);
                assert_eq!(m.change_received, 0);
                failed = true;
                break;
            }
        }
    }
    assert!(failed);
    local_action(&mut b, N, json!({"op":"limits","maxBytes":16384})).unwrap();
    repair(&mut a, &mut b);
    assert_eq!(entries(&mut a), entries(&mut b));
}

#[test]
fn dataset_quota_rejects_whole_exchange_and_recovers_after_expansion() {
    let (mut a, mut b) = (store("a"), store("b"));
    put(&mut a, "first", json!(1));
    call(&mut a, "put", "other.v1", "second", json!(2)).unwrap();
    local_action(&mut b, N, json!({"op":"limits","maxDatasets":1})).unwrap();
    let mut sa = States::new();
    let mut sb = States::new();
    let p = step(&mut a, "b", &mut sa, vec![]);
    assert!(b
        .sync_step(N, "a", &mut sb, p, &mut Metrics::default())
        .err()
        .unwrap()
        .contains("容量"));
    assert!(sb.is_empty());
    assert!(b.replicas(N).unwrap().is_empty());
    local_action(&mut b, N, json!({"op":"limits","maxDatasets":2})).unwrap();
    repair(&mut a, &mut b);
    assert_eq!(
        call(&mut b, "get", "other.v1", "second", Value::Null).unwrap()["second"]["value"],
        2
    );
}

#[test]
fn commit_failure_rolls_back_local_ack_and_sync_progress() {
    let (mut a, mut b) = (store("a"), store("b"));
    put(&mut a, "baseline", json!(0));
    repair(&mut a, &mut b);
    // Deferred FK causes an actual SQLite COMMIT failure, after document/state SQL ran.
    b.db.execute_batch("PRAGMA foreign_keys=ON;
      CREATE TABLE parent(id INTEGER PRIMARY KEY);
      CREATE TABLE fault(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);
      CREATE TRIGGER fail_commit AFTER INSERT ON shared_docs BEGIN INSERT INTO fault VALUES(1); END;
      CREATE TRIGGER fail_update AFTER UPDATE ON shared_docs BEGIN INSERT INTO fault VALUES(1); END;").unwrap();
    let before = heads(&mut b);
    assert!(call(&mut b, "put", "test.v1", "local-failed", json!(1)).is_err());
    assert_eq!(heads(&mut b), before);
    b.db.execute_batch("DROP TRIGGER fail_commit; DROP TRIGGER fail_update;")
        .unwrap();
    put(&mut a, "remote", json!(2));
    let (mut sa, mut sb) = (States::new(), States::new());
    let opening = step(&mut a, "b", &mut sa, vec![]);
    let response = step(&mut b, "a", &mut sb, opening);
    let p = step(&mut a, "b", &mut sa, response);
    assert!(
        p.iter().any(|r| r.change_bytes > 0),
        "fault must interrupt actual incoming changes"
    );
    let states_before: Vec<_> = sb.iter().map(|(k, s)| (k.clone(), s.encode())).collect();
    let persisted_before: Vec<Vec<u8>> =
        b.db.prepare("SELECT state FROM shared_sync ORDER BY peer,app,dataset")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
    b.db.execute_batch("CREATE TRIGGER fail_commit AFTER INSERT ON shared_docs BEGIN INSERT INTO fault VALUES(1); END;
      CREATE TRIGGER fail_update AFTER UPDATE ON shared_docs BEGIN INSERT INTO fault VALUES(1); END;").unwrap();
    let mut m = Metrics::default();
    assert!(b.sync_step(N, "a", &mut sb, p, &mut m).is_err());
    assert_eq!(
        sb.iter()
            .map(|(k, s)| (k.clone(), s.encode()))
            .collect::<Vec<_>>(),
        states_before
    );
    let persisted_after: Vec<Vec<u8>> =
        b.db.prepare("SELECT state FROM shared_sync ORDER BY peer,app,dataset")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
    assert_eq!(persisted_after, persisted_before);
    assert_eq!(m.change_received, 0);
    assert_eq!(heads(&mut b), before);
    b.db.execute_batch("DROP TRIGGER fail_commit; DROP TRIGGER fail_update;")
        .unwrap();
    put(&mut b, "after-failure", json!(3));
    repair(&mut a, &mut b);
    assert_eq!(entries(&mut a), entries(&mut b));
}

#[test]
fn sqlite_full_and_readonly_do_not_acknowledge_writes() {
    let mut s = store("disk");
    put(&mut s, "kept", json!(true));
    let before = heads(&mut s);
    s.db.execute_batch("PRAGMA query_only=ON;").unwrap();
    assert!(call(&mut s, "put", "test.v1", "readonly", json!(1)).is_err());
    s.db.execute_batch("PRAGMA query_only=OFF;").unwrap();
    assert_eq!(heads(&mut s), before);
    let pages: i64 =
        s.db.pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap();
    s.db.pragma_update(None, "max_page_count", pages).unwrap();
    let mut bytes = vec![0u8; 45000];
    getrandom::fill(&mut bytes).unwrap();
    let e = call(
        &mut s,
        "put",
        "test.v1",
        "disk-full",
        json!(B64.encode(bytes)),
    )
    .unwrap_err();
    assert!(e.contains("full"), "{e}");
    assert_eq!(heads(&mut s), before);
    s.db.pragma_update(None, "max_page_count", 10000).unwrap();
    put(&mut s, "recovered", json!(true));
    assert_eq!(entries(&mut s)["kept"]["value"], true);
}

#[test]
fn deletion_replay_concurrency_and_conflict_resolution() {
    let (mut a, mut b, mut old) = (store("a"), store("b"), store("old"));
    put(&mut a, "key", json!("original"));
    repair(&mut a, &mut b);
    repair(&mut a, &mut old);
    call(&mut a, "delete", "test.v1", "key", Value::Null).unwrap();
    repair(&mut a, &mut old);
    assert!(entries(&mut a).get("key").is_none());
    put(&mut b, "key", json!("unobserved concurrent write"));
    repair(&mut a, &mut b);
    assert_eq!(
        entries(&mut a)["key"]["value"],
        "unobserved concurrent write"
    );
    put(&mut a, "choice", json!("A"));
    put(&mut b, "choice", json!("B"));
    repair(&mut a, &mut b);
    assert_eq!(
        entries(&mut a)["choice"]["versions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    put(&mut a, "choice", json!("resolved"));
    repair(&mut a, &mut b);
    repair(&mut a, &mut old);
    assert_eq!(entries(&mut old)["choice"]["conflict"], false);
    call(&mut a, "delete", "test.v1", "key", Value::Null).unwrap();
    repair(&mut a, &mut b);
    repair(&mut b, &mut old);
    assert!(entries(&mut old).get("key").is_none());
}

#[test]
fn lost_response_duplicate_session_and_out_of_order_history_converge() {
    let (mut a, mut b) = (store("a"), store("b"));
    put(&mut a, "first", json!(1));
    let (mut sa, mut sb) = (States::new(), States::new());
    let first = step(&mut a, "b", &mut sa, vec![]);
    let response = step(&mut b, "a", &mut sb, copy_packet(&first));
    // Drop the response and reconnect, then replay the old opening message.
    drop(response);
    repair(&mut a, &mut b);
    let mut fresh = States::new();
    step(&mut b, "a", &mut fresh, first);
    repair(&mut a, &mut b);
    assert_eq!(entries(&mut a), entries(&mut b));
    let mut doc = Store::load(&a.db, N, "test.v1", "notes").unwrap();
    doc.set_actor(a.actor.clone());
    doc.put(ROOT, "second", "2").unwrap();
    let changes = doc.get_changes(&[]);
    let mut reordered = AutoCommit::new();
    for c in changes.iter().rev() {
        reordered.apply_changes([c.clone()]).unwrap();
    }
    for c in &changes {
        reordered.apply_changes([c.clone()]).unwrap();
    }
    assert_eq!(reordered.get_heads(), doc.get_heads());
}

#[test]
fn storage_schema_upgrade_preserves_history_and_future_schema_is_untouched() {
    let mut s = store("legacy");
    put(&mut s, "kept", json!({"oldField":1}));
    let before = heads(&mut s);
    s.db.pragma_update(None, "user_version", 0).unwrap();
    Store::initialize(&mut s.db).unwrap();
    Store::initialize(&mut s.db).unwrap();
    assert_eq!(heads(&mut s), before);
    s.db.pragma_update(None, "user_version", 99).unwrap();
    assert!(Store::initialize(&mut s.db).unwrap_err().contains("升级"));
    assert_eq!(
        s.db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        99
    );
    assert_eq!(heads(&mut s), before);
}

#[test]
fn business_schema_namespaces_preserve_opaque_values_without_cross_writes() {
    let (mut a, mut b) = (store("a"), store("b"));
    call(&mut a, "put", "test.v1", "same", json!({"title":"old"})).unwrap();
    call(
        &mut b,
        "put",
        "test.v2",
        "same",
        json!({"title":{"text":"new"},"unknownFutureField":[1,2]}),
    )
    .unwrap();
    repair(&mut a, &mut b);
    for s in [&mut a, &mut b] {
        assert_eq!(
            call(s, "get", "test.v1", "same", Value::Null).unwrap()["same"]["value"],
            json!({"title":"old"})
        );
        assert_eq!(
            call(s, "get", "test.v2", "same", Value::Null).unwrap()["same"]["value"]
                ["unknownFutureField"],
            json!([1, 2])
        );
    }
    call(
        &mut a,
        "put",
        "test.v1",
        "same",
        json!({"title":"old client edit"}),
    )
    .unwrap();
    repair(&mut a, &mut b);
    assert_eq!(
        call(&mut b, "get", "test.v2", "same", Value::Null).unwrap()["same"]["value"]["title"],
        json!({"text":"new"})
    );
}

#[test]
fn invalid_envelope_and_malformed_updates_do_not_change_replica() {
    let mut s = store("local");
    put(&mut s, "kept", json!(1));
    let before = heads(&mut s);
    for (version, network, source) in [
        (1, N, "remote"),
        (3, N, "remote"),
        (2, N + 1, "remote"),
        (2, N, "local"),
    ] {
        assert!(validate_packet(
            &Packet {
                version,
                network,
                source: source.into(),
                replicas: vec![]
            },
            N,
            "local",
            Some("remote"),
            ""
        )
        .is_err());
    }
    let mut states = States::new();
    let mut m = Metrics::default();
    let bad = SyncReplica {
        app: "test.v1".into(),
        dataset: "notes".into(),
        message: Some("not-base64".into()),
        change_bytes: 0,
    };
    assert!(s
        .sync_step(N, "remote", &mut states, vec![bad], &mut m)
        .is_err());
    assert!(states.is_empty());
    assert_eq!(m.change_received, 0);
    assert_eq!(heads(&mut s), before);
}
