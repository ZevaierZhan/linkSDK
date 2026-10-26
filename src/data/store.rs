//! AP shared JSON registers. Automerge owns causality and merge; SQLite owns durability.
//! Values are atomic JSON (nested fields are not independently merged). No history GC.
use super::{DataError, DataErrorKind};
use crate::SpaceConfig;
use automerge::{
    sync::{self, SyncDoc},
    transaction::Transactable,
    ActorId, AutoCommit, ReadDoc, ROOT,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::{path::Path, time::Duration};
const MAX: usize = 8 * 1024 * 1024;
const MAX_DOCS: usize = 128;
// Local quotas may grow without discarding history; wire bounds remain finite.
pub(super) const HARD_MAX: usize = 64 * 1024 * 1024;
const HARD_DOCS: usize = 1024;
const STORAGE_VERSION: i64 = 1;
pub(super) const PROTOCOL_VERSION: u8 = 2;
const CAPACITY_ERROR: &str = "共享历史容量不足，本次内容未保存。扩容后重试；删除内容不会释放历史。";
#[derive(Serialize, Deserialize)]
struct Replica {
    app: String,
    dataset: String,
    bytes: Vec<u8>,
}
#[derive(Serialize, Deserialize)]
pub(super) struct SyncReplica {
    pub(super) app: String,
    pub(super) dataset: String,
    pub(super) message: Option<String>,
    #[serde(skip)]
    pub(super) change_bytes: u64,
}
#[derive(Serialize, Deserialize)]
pub(super) struct Packet {
    pub(super) version: u8,
    pub(super) network: u64,
    pub(super) source: String,
    pub(super) replicas: Vec<SyncReplica>,
}
impl Packet {
    pub(super) fn quiet(&self) -> bool {
        self.replicas.iter().all(|r| r.message.is_none())
    }
}
pub(super) type States = HashMap<(String, String), sync::State>;
#[derive(Default)]
pub(super) struct Metrics {
    pub(super) peer: String,
    pub(super) rounds: u64,
    pub(super) sent: u64,
    pub(super) received: u64,
    pub(super) change_sent: u64,
    pub(super) change_received: u64,
}

pub(super) struct Store {
    pub(super) db: Connection,
    actor: ActorId,
}
pub(super) fn valid(s: &str) -> Result<(), DataError> {
    if s.is_empty() || s.len() > 128 || s.chars().any(char::is_control) {
        Err("共享数据标识需为1–128字节且不包含控制字符".into())
    } else {
        Ok(())
    }
}
fn err(e: impl std::error::Error + Send + Sync + 'static) -> DataError {
    let source = &e as &dyn std::error::Error;
    if let Some(sql) = source.downcast_ref::<rusqlite::Error>() {
        DataError::sqlite(sql)
    } else if source.is::<std::io::Error>() {
        DataError::new(DataErrorKind::Io, e.to_string())
    } else {
        DataError::new(DataErrorKind::InvalidInput, e.to_string())
    }
}
impl Store {
    pub(super) fn initialized(dir: &Path) -> Result<bool, DataError> {
        let path = dir.join("shared-data.sqlite");
        if !path.try_exists().map_err(err)? {
            return Ok(false);
        }
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(err)?;
        db.busy_timeout(Duration::ZERO).map_err(err)?;
        let version: i64 = db
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(err)?;
        Ok(version != 0)
    }
    fn schema(db: &Connection) -> Result<(), DataError> {
        db.execute_batch("CREATE TABLE IF NOT EXISTS shared_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS shared_docs(network TEXT NOT NULL,app TEXT NOT NULL,dataset TEXT NOT NULL,doc BLOB NOT NULL,PRIMARY KEY(network,app,dataset));
          CREATE TABLE IF NOT EXISTS shared_sync(network TEXT NOT NULL,peer TEXT NOT NULL,app TEXT NOT NULL,dataset TEXT NOT NULL,state BLOB NOT NULL,PRIMARY KEY(network,peer,app,dataset));
          CREATE TABLE IF NOT EXISTS shared_sync_metrics(peer TEXT PRIMARY KEY,success_ms INTEGER NOT NULL DEFAULT 0,error TEXT,sessions INTEGER NOT NULL DEFAULT 0,sent INTEGER NOT NULL DEFAULT 0,received INTEGER NOT NULL DEFAULT 0,change_sent INTEGER NOT NULL DEFAULT 0,change_received INTEGER NOT NULL DEFAULT 0,last_rounds INTEGER NOT NULL DEFAULT 0,last_change_sent INTEGER NOT NULL DEFAULT 0,last_change_received INTEGER NOT NULL DEFAULT 0);
          PRAGMA user_version=1;").map_err(err)
    }
    fn initialize(db: &mut Connection) -> Result<(), DataError> {
        let version: i64 = db
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(err)?;
        if version > STORAGE_VERSION {
            return Err(DataError::new(
                DataErrorKind::UnsupportedFormat,
                format!("共享存储格式{version}高于当前SDK支持版本；请升级"),
            ));
        }
        if version == STORAGE_VERSION {
            return Ok(());
        }
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .map_err(err)?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(err)?;
        Self::schema(&tx)?;
        tx.commit().map_err(err)
    }
    fn metadata(db: &Connection, key: &str) -> Result<String, DataError> {
        db.query_row("SELECT value FROM shared_meta WHERE key=?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .map_err(err)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| DataError::corrupt(format!("replica {key} metadata missing")))
    }
    fn limits(db: &Connection) -> Result<(usize, usize), DataError> {
        let read = |key: &str, default: usize| -> Result<usize, DataError> {
            db.query_row("SELECT value FROM shared_meta WHERE key=?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(err)?
            .map(|v| v.parse().map_err(DataError::corrupt))
            .unwrap_or(Ok(default))
        };
        let bytes = read("max_bytes", MAX)?;
        let docs = read("max_datasets", MAX_DOCS)?;
        if !(1024..=HARD_MAX).contains(&bytes) || !(1..=HARD_DOCS).contains(&docs) {
            return Err(DataError::corrupt("共享配额配置无效"));
        }
        Ok((bytes, docs))
    }
    pub(super) fn open(home: &Path) -> Result<(Self, u64), DataError> {
        let dir = home.join("space");
        let config: SpaceConfig =
            serde_json::from_slice(&std::fs::read(dir.join("space.json")).map_err(err)?)
                .map_err(err)?;
        Self::open_at(&dir, config.network_id)
    }
    pub(super) fn open_at(dir: &Path, network_id: u64) -> Result<(Self, u64), DataError> {
        if network_id == 0 {
            return Err("Invalid network context".into());
        }
        // Never invent a new replica when identity/configuration is unavailable.
        let mut db = Connection::open(dir.join("shared-data.sqlite")).map_err(err)?;
        db.busy_timeout(Duration::ZERO).map_err(err)?;
        // Connection-local, never rely on a build's default WAL durability setting.
        db.pragma_update(None, "synchronous", "FULL").map_err(err)?;
        let version: i64 = db
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(err)?;
        if version == 0 {
            db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
                .map_err(err)?;
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(err)?;
            Self::schema(&tx)?;
            let previous_network: Option<String> = tx
                .query_row(
                    "SELECT value FROM shared_meta WHERE key='network'",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(err)?;
            if previous_network
                .as_ref()
                .is_some_and(|saved| saved != &format!("{network_id:016x}"))
            {
                return Err(DataError::new(
                    DataErrorKind::SpaceMismatch,
                    "共享副本所属空间不匹配",
                ));
            }
            let has_actor: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM shared_meta WHERE key='actor')",
                    [],
                    |r| r.get(0),
                )
                .map_err(err)?;
            let documents: i64 = tx
                .query_row("SELECT count(*) FROM shared_docs", [], |r| r.get(0))
                .map_err(err)?;
            if !has_actor && documents != 0 {
                return Err(DataError::corrupt("replica actor metadata missing"));
            }
            tx.execute(
                "INSERT OR IGNORE INTO shared_meta VALUES('actor',lower(hex(randomblob(16))))",
                [],
            )
            .map_err(err)?;
            tx.execute(
                "INSERT OR IGNORE INTO shared_meta VALUES('network',?1)",
                [format!("{network_id:016x}")],
            )
            .map_err(err)?;
            tx.commit().map_err(err)?;
        }
        Self::initialize(&mut db)?;
        let actor = Self::metadata(&db, "actor")?;
        let saved = Self::metadata(&db, "network")?;
        if saved != format!("{:016x}", network_id) {
            return Err(DataError::new(
                DataErrorKind::SpaceMismatch,
                "共享副本所属空间不匹配",
            ));
        }
        Ok((
            Self {
                db,
                actor: ActorId::from(actor.as_bytes()),
            },
            network_id,
        ))
    }
    pub(super) fn open_read_at(
        dir: &Path,
        network_id: u64,
        readonly: bool,
    ) -> Result<(Self, u64), DataError> {
        let flags = if readonly {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let db = Connection::open_with_flags(dir.join("shared-data.sqlite"), flags).map_err(err)?;
        db.busy_timeout(Duration::ZERO).map_err(err)?;
        if !readonly {
            db.pragma_update(None, "synchronous", "FULL").map_err(err)?;
        }
        let version: i64 = db
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(err)?;
        if version > STORAGE_VERSION {
            return Err(DataError::new(
                DataErrorKind::UnsupportedFormat,
                "共享存储格式高于当前SDK支持版本；请升级",
            ));
        }
        if version != STORAGE_VERSION {
            return Err(DataError::corrupt("storage not initialized"));
        }
        let saved = Self::metadata(&db, "network")?;
        if network_id == 0 || saved != format!("{network_id:016x}") {
            return Err(DataError::new(
                DataErrorKind::SpaceMismatch,
                "共享副本所属空间不匹配",
            ));
        }
        let actor = Self::metadata(&db, "actor")?;
        Ok((
            Self {
                db,
                actor: ActorId::from(actor.as_bytes()),
            },
            network_id,
        ))
    }
    pub(super) fn load(db: &Connection, n: u64, a: &str, d: &str) -> Result<AutoCommit, DataError> {
        let bytes: Option<Vec<u8>> = db
            .query_row(
                "SELECT doc FROM shared_docs WHERE network=?1 AND app=?2 AND dataset=?3",
                params![format!("{n:016x}"), a, d],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        match bytes {
            Some(b) => AutoCommit::load(&b).map_err(DataError::corrupt),
            None => Ok(AutoCommit::new()),
        }
    }
    fn save(
        db: &Connection,
        n: u64,
        a: &str,
        d: &str,
        doc: &mut AutoCommit,
    ) -> Result<(), DataError> {
        let b = doc.save();
        let (size, count): (i64, i64) = db
            .query_row(
                "SELECT coalesce(sum(length(doc)),0),count(*) FROM shared_docs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(err)?;
        let old: Option<i64> = db
            .query_row(
                "SELECT length(doc) FROM shared_docs WHERE network=?1 AND app=?2 AND dataset=?3",
                params![format!("{n:016x}"), a, d],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        let (max_bytes, max_docs) = Self::limits(db)?;
        if size - old.unwrap_or(0) + b.len() as i64 > max_bytes as i64
            || (old.is_none() && count >= max_docs as i64)
        {
            return Err(DataError::new(
                DataErrorKind::CapacityExceeded,
                CAPACITY_ERROR,
            ));
        }
        db.execute("INSERT INTO shared_docs VALUES(?1,?2,?3,?4) ON CONFLICT(network,app,dataset) DO UPDATE SET doc=excluded.doc",params![format!("{n:016x}"),a,d,b]).map_err(err)?;
        Ok(())
    }
    fn replicas(&self, n: u64) -> Result<Vec<Replica>, DataError> {
        let mut q = self
            .db
            .prepare(
                "SELECT app,dataset,doc FROM shared_docs WHERE network=?1 ORDER BY app,dataset",
            )
            .map_err(err)?;
        let rows = q
            .query_map([format!("{n:016x}")], |r| {
                Ok(Replica {
                    app: r.get(0)?,
                    dataset: r.get(1)?,
                    bytes: r.get(2)?,
                })
            })
            .map_err(err)?;
        rows.map(|r| r.map_err(err)).collect()
    }
    // Document and resumable peer state commit together. Transient in-flight flags
    // stay on this ordered TCP connection; State::encode persists only shared_heads.
    pub(super) fn sync_step(
        &mut self,
        n: u64,
        peer: &str,
        states: &mut States,
        incoming: Vec<SyncReplica>,
        metrics: &mut Metrics,
    ) -> Result<Vec<SyncReplica>, DataError> {
        if incoming.len() > HARD_DOCS {
            return Err("过多数据集".into());
        }
        let gate = super::view::writer(&self.db);
        let _permit = gate.as_ref().map(|g| g.try_write()).transpose()?;
        let mut updates = HashMap::new();
        for r in incoming {
            valid(&r.app)?;
            valid(&r.dataset)?;
            if updates.insert((r.app, r.dataset), r.message).is_some() {
                return Err("重复数据集".into());
            }
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(err)?;
        let mut keys: BTreeSet<(String, String)> = updates.keys().cloned().collect();
        {
            let mut q = tx
                .prepare("SELECT app,dataset FROM shared_docs WHERE network=?1")
                .map_err(err)?;
            let rows = q
                .query_map([format!("{n:016x}")], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(err)?;
            for r in rows {
                keys.insert(r.map_err(err)?);
            }
        }
        if keys.len() > Self::limits(&tx)?.1 {
            return Err(DataError::new(
                DataErrorKind::CapacityExceeded,
                CAPACITY_ERROR,
            ));
        }
        let mut next = states.clone();
        let mut output = Vec::new();
        let mut change_received = 0;
        for (app, dataset) in keys {
            let key = (app.clone(), dataset.clone());
            if !next.contains_key(&key) {
                let saved:Option<Vec<u8>>=tx.query_row("SELECT state FROM shared_sync WHERE network=?1 AND peer=?2 AND app=?3 AND dataset=?4",params![format!("{n:016x}"),peer,app,dataset],|r|r.get(0)).optional().map_err(err)?;
                next.insert(
                    key.clone(),
                    saved
                        .map(|b| sync::State::decode(&b).map_err(err))
                        .transpose()?
                        .unwrap_or_default(),
                );
            }
            let state = next.get_mut(&key).unwrap();
            let mut doc = Self::load(&tx, n, &app, &dataset)?;
            if let Some(Some(encoded)) = updates.remove(&key) {
                let bytes = B64.decode(encoded).map_err(err)?;
                if bytes.len() > HARD_MAX * 2 {
                    return Err("同步消息过大".into());
                }
                let message = sync::Message::decode(&bytes).map_err(err)?;
                change_received += message.changes.iter().map(|b| b.len() as u64).sum::<u64>();
                doc.sync()
                    .receive_sync_message(state, message)
                    .map_err(err)?;
                Self::save(&tx, n, &app, &dataset, &mut doc)?;
            }
            let mut change_bytes = 0;
            let message = doc.sync().generate_sync_message(state).map(|message| {
                change_bytes = message.changes.iter().map(|b| b.len() as u64).sum();
                B64.encode(message.encode())
            });
            tx.execute("INSERT INTO shared_sync VALUES(?1,?2,?3,?4,?5) ON CONFLICT(network,peer,app,dataset) DO UPDATE SET state=excluded.state",params![format!("{n:016x}"),peer,app,dataset,state.encode()]).map_err(err)?;
            output.push(SyncReplica {
                app,
                dataset,
                message,
                change_bytes,
            });
        }
        tx.commit().map_err(err)?;
        super::view::committed(&self.db);
        *states = next;
        metrics.change_received += change_received;
        Ok(output)
    }
    pub(super) fn record(&self, m: &Metrics, result: &Result<(), String>) -> Result<(), DataError> {
        if m.peer.is_empty() {
            return Ok(());
        }
        let gate = super::view::writer(&self.db);
        let _permit = gate.as_ref().map(|g| g.try_write()).transpose()?;
        self.db.execute("INSERT INTO shared_sync_metrics(peer,success_ms,error,sessions,sent,received,change_sent,change_received,last_rounds,last_change_sent,last_change_received) VALUES(?1,?2,?3,1,?4,?5,?6,?7,?8,?6,?7) ON CONFLICT(peer) DO UPDATE SET success_ms=CASE WHEN excluded.error IS NULL THEN excluded.success_ms ELSE success_ms END,error=excluded.error,sessions=sessions+1,sent=sent+excluded.sent,received=received+excluded.received,change_sent=change_sent+excluded.change_sent,change_received=change_received+excluded.change_received,last_rounds=excluded.last_rounds,last_change_sent=excluded.last_change_sent,last_change_received=excluded.last_change_received",params![m.peer,if result.is_ok(){crate::now_millis() as i64}else{0},result.as_ref().err(),m.sent as i64,m.received as i64,m.change_sent as i64,m.change_received as i64,m.rounds as i64]).map_err(err)?;
        Ok(())
    }
    fn sync_metrics(&self) -> Result<Vec<Value>, DataError> {
        let mut q=self.db.prepare("SELECT peer,success_ms,error,sessions,sent,received,change_sent,change_received,last_rounds,last_change_sent,last_change_received FROM shared_sync_metrics ORDER BY peer").map_err(err)?;
        let rows=q.query_map([],|r|Ok(json!({"peer":r.get::<_,String>(0)?,"successMs":r.get::<_,i64>(1)?,"error":r.get::<_,Option<String>>(2)?,"sessions":r.get::<_,i64>(3)?,"sentBytes":r.get::<_,i64>(4)?,"receivedBytes":r.get::<_,i64>(5)?,"sentChangeBytes":r.get::<_,i64>(6)?,"receivedChangeBytes":r.get::<_,i64>(7)?,"lastRounds":r.get::<_,i64>(8)?,"lastSentChangeBytes":r.get::<_,i64>(9)?,"lastReceivedChangeBytes":r.get::<_,i64>(10)?}))).map_err(err)?;
        rows.map(|r| r.map_err(err)).collect()
    }
}
/// Local reads and writes work without libzt/organizer availability. All calls reload
/// the durable document inside serialized SQLite transactions, including CLI writers.
#[cfg(test)]
pub(super) fn local_action(s: &mut Store, n: u64, request: Value) -> Result<Value, DataError> {
    local_action_with(s, n, request, None)
}
pub(super) fn local_action_with(
    s: &mut Store,
    n: u64,
    request: Value,
    op_control: Option<&super::Operation>,
) -> Result<Value, DataError> {
    let check = || op_control.map_or(Ok(()), super::Operation::check);
    check()?;
    let op = request["op"].as_str().ok_or("缺少op")?;
    let gate = if matches!(op, "put" | "delete" | "limits") {
        super::view::writer(&s.db)
    } else {
        None
    };
    let _permit = gate.as_ref().map(|g| g.try_write()).transpose()?;
    if op == "limits" {
        let tx =
            s.db.transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(err)?;
        let (old_bytes, old_docs) = Store::limits(&tx)?;
        let number =
            |name: &str, fallback: usize, max: usize, min: usize| -> Result<usize, DataError> {
                if request.get(name).is_none_or(Value::is_null) {
                    return Ok(fallback);
                }
                let n = request[name].as_u64().ok_or("配额必须是正整数")?;
                if n < min as u64 || n > max as u64 {
                    return Err(format!("{name}范围为{min}–{max}").into());
                }
                Ok(n as usize)
            };
        let bytes = number("maxBytes", old_bytes, HARD_MAX, 1024)?;
        let docs = number("maxDatasets", old_docs, HARD_DOCS, 1)?;
        let (used, count): (i64, i64) = tx
            .query_row(
                "SELECT coalesce(sum(length(doc)),0),count(*) FROM shared_docs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(err)?;
        if (bytes as i64) < used || (docs as i64) < count {
            return Err("配额不能低于已保存历史；原配置未改变".into());
        }
        for (key, value) in [("max_bytes", bytes), ("max_datasets", docs)] {
            tx.execute("INSERT INTO shared_meta VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value.to_string()]).map_err(err)?;
        }
        check()?;
        tx.commit().map_err(err)?;
        super::view::committed(&s.db);
        return Ok(json!({"maxBytes":bytes,"maxDatasets":docs,"scope":"local","historyGc":false}));
    }
    if op == "status" {
        let reps = s.replicas(n)?;
        let (max_bytes, max_docs) = Store::limits(&s.db)?;
        return Ok(
            json!({"engine":"automerge-0.12.0","semantics":"local-durable/eventual","storageVersion":STORAGE_VERSION,"protocolVersion":PROTOCOL_VERSION,"applicationSchema":"versioned-namespace; opaque JSON; no automatic migration","datasets":reps.len(),"bytes":reps.iter().map(|r|r.bytes.len()).sum::<usize>(),"maxBytes":max_bytes,"maxDatasets":max_docs,"hardMaxBytes":HARD_MAX,"hardMaxDatasets":HARD_DOCS,"maxValueBytes":65536,"historyGc":false,"lastExchangeMs":s.db.query_row("SELECT value FROM shared_meta WHERE key='last_exchange_ms'",[],|r|r.get::<_,String>(0)).optional().map_err(err)?,"replicationAcknowledgement":"not-tracked","transport":"libzt-tcp:19997","repair":"automerge-sync/5s-after-cycle","syncPeers":s.sync_metrics()?}),
        );
    }
    let app = request["app"].as_str().ok_or("缺少app")?;
    let dataset = request["dataset"].as_str().ok_or("缺少dataset")?;
    valid(app)?;
    valid(dataset)?;
    let behavior = if matches!(op, "get" | "list" | "heads") {
        TransactionBehavior::Deferred
    } else {
        TransactionBehavior::Immediate
    };
    let tx = s.db.transaction_with_behavior(behavior).map_err(err)?;
    let mut doc = Store::load(&tx, n, app, dataset)?;
    doc.set_actor(s.actor.clone());
    match op {
        "put" | "delete" => {
            let key = request["key"].as_str().ok_or("缺少key")?;
            valid(key)?;
            // Optional optimistic concurrency is checked inside this same
            // IMMEDIATE transaction, including changes received by sync since
            // the caller read its form. Existing unconditional callers keep
            // their established Automerge conflict-resolution semantics.
            if let Some(expected) = request.get("expectedVersions") {
                let mut expected = expected
                    .as_array()
                    .ok_or("expectedVersions必须是版本编号列表")?
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned).ok_or("版本编号必须是字符串"))
                    .collect::<Result<Vec<_>, _>>()?;
                expected.sort();
                if expected.windows(2).any(|v| v[0] == v[1]) {
                    return Err("expectedVersions不能包含重复编号".into());
                }
                let mut actual = doc
                    .get_all(ROOT, key)
                    .map_err(err)?
                    .into_iter()
                    .map(|(_, id)| id.to_string())
                    .collect::<Vec<_>>();
                actual.sort();
                if actual != expected {
                    return Err(DataError::new(
                        DataErrorKind::StaleVersion,
                        "共享配置已有更新，请重新载入后再保存；未覆盖新版本。",
                    ));
                }
            }
            if op == "delete" {
                doc.delete(ROOT, key).map_err(err)?;
            } else {
                let value = request.get("value").ok_or("缺少value")?;
                let encoded = serde_json::to_string(value).map_err(err)?;
                if encoded.len() > 65536 {
                    return Err("单值最多64KiB".into());
                }
                doc.put(ROOT, key, encoded).map_err(err)?;
            }
            Store::save(&tx, n, app, dataset, &mut doc)?;
            check()?;
            tx.commit().map_err(err)?;
            super::view::committed(&s.db);
            Ok(json!({"saved":"local","replicated":false}))
        }
        "heads" => {
            let mut heads: Vec<String> = doc.get_heads().iter().map(ToString::to_string).collect();
            heads.sort();
            check()?;
            tx.commit().map_err(err)?;
            Ok(json!({"heads":heads}))
        }
        "get" | "list" => {
            let key = if op == "get" {
                Some(request["key"].as_str().ok_or("缺少key")?)
            } else {
                None
            };
            let value = entries(&doc, key)?;
            check()?;
            tx.commit().map_err(err)?;
            Ok(value)
        }
        _ => Err("支持status/limits/get/list/put/delete/heads".into()),
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
pub(super) fn validate_packet(
    p: &Packet,
    network: u64,
    local: &str,
    target: Option<&str>,
    peer: &str,
) -> Result<(), DataError> {
    if p.version != PROTOCOL_VERSION
        || p.network != network
        || p.source == local
        || target.is_some_and(|t| t != p.source)
        || (!peer.is_empty() && peer != p.source)
    {
        return Err("增量同步协议/空间/身份不匹配".into());
    }
    Ok(())
}

pub(super) fn entries(doc: &AutoCommit, key: Option<&str>) -> Result<Value, DataError> {
    let keys: Vec<String> = match key {
        Some(k) => {
            valid(k)?;
            vec![k.into()]
        }
        None => doc.keys(ROOT).collect(),
    };
    let mut entries = serde_json::Map::new();
    for k in keys {
        let winner = doc
            .get(ROOT, k.as_str())
            .map_err(err)?
            .and_then(|(v, _)| v.as_str().map(str::to_owned))
            .map(|v| serde_json::from_str::<Value>(&v).map_err(DataError::corrupt))
            .transpose()?;
        let versions=doc.get_all(ROOT,k.as_str()).map_err(DataError::corrupt)?.into_iter().map(|(v,id)|{let v=v.as_str().ok_or_else(||DataError::corrupt("不支持的共享值类型"))?;Ok(json!({"value":serde_json::from_str::<Value>(v).map_err(DataError::corrupt)?,"version":id.to_string()}))}).collect::<Result<Vec<Value>,DataError>>()?;
        entries.insert(
            k,
            json!({"value":winner,"versions":versions,"conflict":versions.len()>1}),
        );
    }

    Ok(Value::Object(entries))
}
