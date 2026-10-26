use super::{
    store::{self, Store},
    DataError, DataErrorKind, DataResult, Operation,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use parking_lot::{Condvar, Mutex};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, OnceLock, Weak},
    time::Duration,
};
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct DatasetKey {
    pub app: String,
    pub dataset: String,
}
impl DatasetKey {
    pub fn new(app: impl Into<String>, dataset: impl Into<String>) -> Self {
        Self {
            app: app.into(),
            dataset: dataset.into(),
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct DatasetSnapshot {
    pub key: DatasetKey,
    pub entries: Value,
    pub heads: Vec<String>,
}
/// Immutable, transactionally consistent selection. Version is opaque, scoped to space/selection.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub network_id: u64,
    pub version: String,
    pub observed_at_ms: u64,
    pub datasets: Vec<DatasetSnapshot>,
}
#[derive(Debug, Clone)]
pub enum SnapshotEvent {
    Fresh {
        snapshot: Arc<Snapshot>,
        changed: bool,
    },
    /// No implicit stale success. Host must enforce its independent authorization/lease policy.
    Unavailable {
        last_good: Option<Arc<Snapshot>>,
        error: DataError,
    },
}
#[derive(Default)]
pub(super) struct Gate {
    writer: Mutex<()>,
    epoch: Mutex<u64>,
    changed: Condvar,
}
static GATES: OnceLock<Mutex<HashMap<PathBuf, Weak<Gate>>>> = OnceLock::new();
fn gate(directory: &Path) -> Arc<Gate> {
    let mut registry = GATES.get_or_init(Default::default).lock();
    registry.retain(|_, w| w.strong_count() > 0);
    let key = directory
        .canonicalize()
        .unwrap_or_else(|_| directory.to_owned());
    if let Some(g) = registry.get(&key).and_then(Weak::upgrade) {
        return g;
    }
    let g = Arc::new(Gate::default());
    registry.insert(key, Arc::downgrade(&g));
    g
}
impl Gate {
    pub(super) fn try_write(&self) -> DataResult<parking_lot::MutexGuard<'_, ()>> {
        self.writer
            .try_lock()
            .ok_or_else(|| DataError::new(DataErrorKind::Busy, "local replica writer is busy"))
    }
    fn notify(&self) {
        let mut e = self.epoch.lock();
        *e = e.wrapping_add(1);
        self.changed.notify_all();
    }
}
pub(super) fn writer(db: &rusqlite::Connection) -> Option<Arc<Gate>> {
    db.path()
        .filter(|p| !p.is_empty())
        .and_then(|p| Path::new(p).parent())
        .map(gate)
}
pub(super) fn committed(db: &rusqlite::Connection) {
    if let Some(path) = db.path() {
        if let Some(dir) = Path::new(path).parent() {
            gate(dir).notify();
        }
    }
}
#[derive(Clone)]
pub struct DataStore {
    directory: PathBuf,
    network_id: u64,
    gate: Arc<Gate>,
}
impl DataStore {
    /// Caller owns directory/context authentication. Opening never starts networking.
    pub fn open_in(
        directory: impl AsRef<Path>,
        network_id: u64,
        op: &Operation,
    ) -> DataResult<Self> {
        op.check()?;
        let directory = directory
            .as_ref()
            .canonicalize()
            .map_err(|e| DataError::new(DataErrorKind::Io, e.to_string()))?;
        op.retry(|| {
            if Store::initialized(&directory)? {
                Store::open_read_at(&directory, network_id, true).map(|_| ())
            } else {
                Store::open_at(&directory, network_id).map(|_| ())
            }
        })?;
        Ok(Self {
            gate: gate(&directory),
            directory,
            network_id,
        })
    }
    pub fn open_at(home: impl AsRef<Path>, op: &Operation) -> DataResult<Self> {
        op.check()?;
        let dir = home.as_ref().join("space");
        let bytes = std::fs::read(dir.join("space.json"))
            .map_err(|e| DataError::new(DataErrorKind::Io, e.to_string()))?;
        let cfg: crate::SpaceConfig = serde_json::from_slice(&bytes)
            .map_err(|e| DataError::new(DataErrorKind::InvalidInput, e.to_string()))?;
        Self::open_in(dir, cfg.network_id, op)
    }
    pub fn action(&self, request: Value, op: &Operation) -> DataResult<Value> {
        op.check()?;
        if !matches!(
            request["op"].as_str(),
            Some("put" | "delete" | "limits" | "get" | "list" | "heads" | "status")
        ) {
            return Err(DataError::new(
                DataErrorKind::InvalidInput,
                "支持status/limits/get/list/put/delete/heads",
            ));
        }
        let write = matches!(request["op"].as_str(), Some("put" | "delete" | "limits"));
        op.retry(|| {
            let (mut store, n) = Store::open_read_at(&self.directory, self.network_id, !write)?;
            op.configure(&store.db)?;
            op.check()?;
            op.translate(store::local_action_with(
                &mut store,
                n,
                request.clone(),
                Some(op),
            ))
        })
    }
    /// One SQLite read transaction for all selected datasets; never initializes or writes.
    pub fn snapshot(&self, keys: &[DatasetKey], op: &Operation) -> DataResult<Arc<Snapshot>> {
        let keys = validate(keys)?;
        op.retry(|| {
            let (mut store, n) = Store::open_read_at(&self.directory, self.network_id, true)?;
            op.configure(&store.db)?;
            let result = (|| {
                let tx = store.db.transaction().map_err(|e| DataError::sqlite(&e))?;
                let mut datasets = Vec::new();
                for key in &keys {
                    op.check()?;
                    let mut doc = Store::load(&tx, n, &key.app, &key.dataset)?;
                    let mut heads: Vec<String> =
                        doc.get_heads().iter().map(ToString::to_string).collect();
                    heads.sort();
                    let entries = store::entries(&doc, None)?;
                    datasets.push(DatasetSnapshot {
                        key: key.clone(),
                        entries,
                        heads,
                    });
                }
                op.check()?;
                tx.commit().map_err(|e| DataError::sqlite(&e))?;
                let revision: Vec<_> = datasets.iter().map(|d| (&d.key, &d.heads)).collect();
                let version = B64.encode(
                    serde_json::to_vec(&(n, revision)).expect("serializable snapshot revision"),
                );
                Ok(Arc::new(Snapshot {
                    network_id: n,
                    version,
                    observed_at_ms: crate::now_millis(),
                    datasets,
                }))
            })();
            op.translate(result)
        })
    }
    pub fn reader(&self, keys: Vec<DatasetKey>) -> DataResult<SnapshotReader> {
        Ok(SnapshotReader {
            store: self.clone(),
            keys: validate(&keys)?,
            last_good: None,
        })
    }
    /// Bounded coalescing notifications. Local commits wake the worker; polling repairs
    /// missed notifications and observes other processes/legacy writers. No per-event log.
    pub fn subscribe(&self, keys: Vec<DatasetKey>, interval: Duration) -> DataResult<Subscription> {
        let mut reader = self.reader(keys)?;
        let (tx, rx) = mpsc::sync_channel(1);
        let stop = Operation::default();
        let worker_stop = stop.clone();
        let gate = self.gate.clone();
        let interval = interval.clamp(Duration::from_millis(10), Duration::from_secs(60));
        let thread = std::thread::Builder::new()
            .name("linksdk-data-watch".into())
            .spawn(move || {
                let mut delivered = String::new();
                let mut delivered_version: Option<String> = None;
                loop {
                    if worker_stop.is_cancelled() {
                        break;
                    }
                    let before = *gate.epoch.lock();
                    let mut event =
                        reader.refresh(&worker_stop.for_timeout(Duration::from_millis(250)));
                    let fresh_version =
                        if let SnapshotEvent::Fresh { snapshot, changed } = &mut event {
                            *changed = delivered_version.as_ref() != Some(&snapshot.version);
                            Some(snapshot.version.clone())
                        } else {
                            None
                        };
                    let fingerprint = match &event {
                        SnapshotEvent::Fresh { snapshot, .. } => {
                            format!("fresh:{}", snapshot.version)
                        }
                        SnapshotEvent::Unavailable { error, .. } => {
                            format!("error:{:?}", error.kind)
                        }
                    };
                    if fingerprint != delivered {
                        match tx.try_send(event) {
                            Ok(()) => {
                                delivered = fingerprint;
                                if let Some(version) = fresh_version {
                                    delivered_version = Some(version);
                                }
                            }
                            Err(mpsc::TrySendError::Disconnected(_)) => break,
                            Err(mpsc::TrySendError::Full(_)) => {}
                        }
                    }
                    let mut epoch = gate.epoch.lock();
                    if worker_stop.is_cancelled() {
                        break;
                    }
                    if *epoch == before {
                        gate.changed.wait_for(&mut epoch, interval);
                    }
                }
            })
            .map_err(|e| DataError::new(DataErrorKind::Io, e.to_string()))?;
        Ok(Subscription {
            rx,
            stop,
            gate: self.gate.clone(),
            thread: Some(thread),
        })
    }
}
fn validate(keys: &[DatasetKey]) -> DataResult<Vec<DatasetKey>> {
    if keys.is_empty() || keys.len() > 128 {
        return Err(DataError::new(
            DataErrorKind::InvalidInput,
            "select 1..128 datasets",
        ));
    }
    let mut keys = keys.to_vec();
    keys.sort();
    keys.dedup();
    for key in &keys {
        store::valid(&key.app)?;
        store::valid(&key.dataset)?;
    }
    Ok(keys)
}
pub struct SnapshotReader {
    store: DataStore,
    keys: Vec<DatasetKey>,
    last_good: Option<Arc<Snapshot>>,
}
impl SnapshotReader {
    pub fn last_good(&self) -> Option<Arc<Snapshot>> {
        self.last_good.clone()
    }
    pub fn refresh(&mut self, op: &Operation) -> SnapshotEvent {
        match self.store.snapshot(&self.keys, op) {
            Ok(snapshot) => {
                let changed = self
                    .last_good
                    .as_ref()
                    .is_none_or(|s| s.version != snapshot.version);
                self.last_good = Some(snapshot.clone());
                SnapshotEvent::Fresh { snapshot, changed }
            }
            Err(error) => SnapshotEvent::Unavailable {
                last_good: self.last_good.clone(),
                error,
            },
        }
    }
}
pub struct Subscription {
    rx: mpsc::Receiver<SnapshotEvent>,
    stop: Operation,
    gate: Arc<Gate>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Subscription {
    pub fn recv_timeout(&self, timeout: Duration) -> Result<SnapshotEvent, mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        let _epoch = self.gate.epoch.lock();
        self.stop.cancel();
        self.gate.changed.notify_all();
        drop(_epoch);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
