//! AP shared JSON registers. Automerge owns causality and merge; SQLite owns durability.
//! Values are atomic JSON (nested fields are not independently merged). No history GC.
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
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
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
fn valid(s: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > 128 || s.chars().any(char::is_control) {
        Err("共享数据标识需为1–128字节且不包含控制字符".into())
    } else {
        Ok(())
    }
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
impl Store {
    fn initialize(db: &mut Connection) -> Result<(), String> {
        let version: i64 = db
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(err)?;
        if version > STORAGE_VERSION {
            return Err(format!(
                "共享存储格式{version}高于本应用支持的{STORAGE_VERSION}；请升级应用，原数据未改写"
            ));
        }
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
          BEGIN IMMEDIATE;
          CREATE TABLE IF NOT EXISTS shared_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS shared_docs(network TEXT NOT NULL,app TEXT NOT NULL,dataset TEXT NOT NULL,doc BLOB NOT NULL,PRIMARY KEY(network,app,dataset));
          CREATE TABLE IF NOT EXISTS shared_sync(network TEXT NOT NULL,peer TEXT NOT NULL,app TEXT NOT NULL,dataset TEXT NOT NULL,state BLOB NOT NULL,PRIMARY KEY(network,peer,app,dataset));
          CREATE TABLE IF NOT EXISTS shared_sync_metrics(peer TEXT PRIMARY KEY,success_ms INTEGER NOT NULL DEFAULT 0,error TEXT,sessions INTEGER NOT NULL DEFAULT 0,sent INTEGER NOT NULL DEFAULT 0,received INTEGER NOT NULL DEFAULT 0,change_sent INTEGER NOT NULL DEFAULT 0,change_received INTEGER NOT NULL DEFAULT 0,last_rounds INTEGER NOT NULL DEFAULT 0,last_change_sent INTEGER NOT NULL DEFAULT 0,last_change_received INTEGER NOT NULL DEFAULT 0);
          PRAGMA user_version=1; COMMIT;").map_err(err)
    }
    fn limits(db: &Connection) -> Result<(usize, usize), String> {
        let read = |key: &str, default: usize| -> Result<usize, String> {
            db.query_row("SELECT value FROM shared_meta WHERE key=?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(err)?
            .map(|v| v.parse().map_err(err))
            .unwrap_or(Ok(default))
        };
        let bytes = read("max_bytes", MAX)?;
        let docs = read("max_datasets", MAX_DOCS)?;
        if !(1024..=HARD_MAX).contains(&bytes) || !(1..=HARD_DOCS).contains(&docs) {
            return Err("共享配额配置无效".into());
        }
        Ok((bytes, docs))
    }
    pub(super) fn open(home: &Path) -> Result<(Self, u64), String> {
        let dir = home.join("space");
        let config: SpaceConfig =
            serde_json::from_slice(&std::fs::read(dir.join("space.json")).map_err(err)?)
                .map_err(err)?;
        Self::open_at(&dir, config.network_id)
    }
    pub(super) fn open_at(dir: &Path, network_id: u64) -> Result<(Self, u64), String> {
        if network_id == 0 {
            return Err("Invalid network context".into());
        }
        // Never invent a new replica when identity/configuration is unavailable.
        let mut db = Connection::open(dir.join("shared-data.sqlite")).map_err(err)?;
        db.busy_timeout(Duration::from_secs(5)).map_err(err)?;
        Self::initialize(&mut db)?;
        // Actor incarnation survives restarts and is independent of clock and address.
        db.execute(
            "INSERT OR IGNORE INTO shared_meta VALUES('actor',lower(hex(randomblob(16))))",
            [],
        )
        .map_err(err)?;
        let actor: String = db
            .query_row("SELECT value FROM shared_meta WHERE key='actor'", [], |r| {
                r.get(0)
            })
            .map_err(err)?;
        db.execute(
            "INSERT OR IGNORE INTO shared_meta VALUES('network',?1)",
            [format!("{:016x}", network_id)],
        )
        .map_err(err)?;
        let saved: String = db
            .query_row(
                "SELECT value FROM shared_meta WHERE key='network'",
                [],
                |r| r.get(0),
            )
            .map_err(err)?;
        if saved != format!("{:016x}", network_id) {
            return Err("共享副本所属空间不匹配".into());
        }
        Ok((
            Self {
                db,
                actor: ActorId::from(actor.as_bytes()),
            },
            network_id,
        ))
    }
    fn load(db: &Connection, n: u64, a: &str, d: &str) -> Result<AutoCommit, String> {
        let bytes: Option<Vec<u8>> = db
            .query_row(
                "SELECT doc FROM shared_docs WHERE network=?1 AND app=?2 AND dataset=?3",
                params![format!("{n:016x}"), a, d],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        match bytes {
            Some(b) => AutoCommit::load(&b).map_err(err),
            None => Ok(AutoCommit::new()),
        }
    }
    fn save(db: &Connection, n: u64, a: &str, d: &str, doc: &mut AutoCommit) -> Result<(), String> {
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
            return Err(CAPACITY_ERROR.into());
        }
        db.execute("INSERT INTO shared_docs VALUES(?1,?2,?3,?4) ON CONFLICT(network,app,dataset) DO UPDATE SET doc=excluded.doc",params![format!("{n:016x}"),a,d,b]).map_err(err)?;
        Ok(())
    }
    fn replicas(&self, n: u64) -> Result<Vec<Replica>, String> {
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
    ) -> Result<Vec<SyncReplica>, String> {
        if incoming.len() > HARD_DOCS {
            return Err("过多数据集".into());
        }
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
            return Err(CAPACITY_ERROR.into());
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
        *states = next;
        metrics.change_received += change_received;
        Ok(output)
    }
    pub(super) fn record(&self, m: &Metrics, result: &Result<(), String>) -> Result<(), String> {
        if m.peer.is_empty() {
            return Ok(());
        }
        self.db.execute("INSERT INTO shared_sync_metrics(peer,success_ms,error,sessions,sent,received,change_sent,change_received,last_rounds,last_change_sent,last_change_received) VALUES(?1,?2,?3,1,?4,?5,?6,?7,?8,?6,?7) ON CONFLICT(peer) DO UPDATE SET success_ms=CASE WHEN excluded.error IS NULL THEN excluded.success_ms ELSE success_ms END,error=excluded.error,sessions=sessions+1,sent=sent+excluded.sent,received=received+excluded.received,change_sent=change_sent+excluded.change_sent,change_received=change_received+excluded.change_received,last_rounds=excluded.last_rounds,last_change_sent=excluded.last_change_sent,last_change_received=excluded.last_change_received",params![m.peer,if result.is_ok(){crate::now_millis() as i64}else{0},result.as_ref().err(),m.sent as i64,m.received as i64,m.change_sent as i64,m.change_received as i64,m.rounds as i64]).map_err(err)?;
        Ok(())
    }
    fn sync_metrics(&self) -> Result<Vec<Value>, String> {
        let mut q=self.db.prepare("SELECT peer,success_ms,error,sessions,sent,received,change_sent,change_received,last_rounds,last_change_sent,last_change_received FROM shared_sync_metrics ORDER BY peer").map_err(err)?;
        let rows=q.query_map([],|r|Ok(json!({"peer":r.get::<_,String>(0)?,"successMs":r.get::<_,i64>(1)?,"error":r.get::<_,Option<String>>(2)?,"sessions":r.get::<_,i64>(3)?,"sentBytes":r.get::<_,i64>(4)?,"receivedBytes":r.get::<_,i64>(5)?,"sentChangeBytes":r.get::<_,i64>(6)?,"receivedChangeBytes":r.get::<_,i64>(7)?,"lastRounds":r.get::<_,i64>(8)?,"lastSentChangeBytes":r.get::<_,i64>(9)?,"lastReceivedChangeBytes":r.get::<_,i64>(10)?}))).map_err(err)?;
        rows.map(|r| r.map_err(err)).collect()
    }
}
/// Local reads and writes work without libzt/organizer availability. All calls reload
/// the durable document inside serialized SQLite transactions, including CLI writers.
pub fn action(home: PathBuf, request: Value) -> Result<Value, String> {
    let (mut s, n) = Store::open(&home)?;
    local_action(&mut s, n, request)
}
pub(super) fn local_action(s: &mut Store, n: u64, request: Value) -> Result<Value, String> {
    let op = request["op"].as_str().ok_or("缺少op")?;
    if op == "limits" {
        let tx =
            s.db.transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(err)?;
        let (old_bytes, old_docs) = Store::limits(&tx)?;
        let number =
            |name: &str, fallback: usize, max: usize, min: usize| -> Result<usize, String> {
                if request.get(name).is_none_or(Value::is_null) {
                    return Ok(fallback);
                }
                let n = request[name].as_u64().ok_or("配额必须是正整数")?;
                if n < min as u64 || n > max as u64 {
                    return Err(format!("{name}范围为{min}–{max}"));
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
        tx.commit().map_err(err)?;
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
    let tx =
        s.db.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(err)?;
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
                    return Err("共享配置已有更新，请重新载入后再保存；未覆盖新版本。".into());
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
            tx.commit().map_err(err)?;
            Ok(json!({"saved":"local","replicated":false}))
        }
        "heads" => {
            let mut heads: Vec<String> = doc.get_heads().iter().map(ToString::to_string).collect();
            heads.sort();
            tx.commit().map_err(err)?;
            Ok(json!({"heads":heads}))
        }
        "get" | "list" => {
            let keys: Vec<String> = if op == "get" {
                let k = request["key"].as_str().ok_or("缺少key")?;
                valid(k)?;
                vec![k.into()]
            } else {
                doc.keys(ROOT).collect()
            };
            let mut entries = serde_json::Map::new();
            for k in keys {
                let winner = doc
                    .get(ROOT, k.as_str())
                    .map_err(err)?
                    .and_then(|(v, _)| v.as_str().map(str::to_owned))
                    .map(|v| serde_json::from_str::<Value>(&v).map_err(err))
                    .transpose()?;
                let versions=doc.get_all(ROOT,k.as_str()).map_err(err)?.into_iter().map(|(v,id)|{let v=v.as_str().ok_or("不支持的共享值类型")?;Ok(json!({"value":serde_json::from_str::<Value>(v).map_err(err)?,"version":id.to_string()}))}).collect::<Result<Vec<Value>,String>>()?;
                entries.insert(
                    k,
                    json!({"value":winner,"versions":versions,"conflict":versions.len()>1}),
                );
            }
            tx.commit().map_err(err)?;
            Ok(Value::Object(entries))
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
) -> Result<(), String> {
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
