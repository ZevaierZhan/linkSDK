//! Ordered, bounded Automerge synchronization over trusted libzt streams.
use super::store::*;
use crate::network::{frame::Frame, Node, Stream as ZtSocket};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
#[derive(Clone)]
struct Context {
    id: String,
    home: PathBuf,
    peers: Arc<Mutex<HashMap<String, String>>>,
}
/// Borrows the node: stop and join this server before stopping the libzt runtime.
pub struct ReplicaServer<'a> {
    _node: &'a Node,
    stop: Arc<AtomicBool>,
    listener: Arc<ZtSocket>,
    active: Arc<Mutex<Option<Arc<ZtSocket>>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    errors: Arc<Mutex<Vec<String>>>,
}
impl<'a> ReplicaServer<'a> {
    pub fn start(node: &'a Node, port: u16) -> Result<Self, String> {
        let listener = node.listen(port)?;
        let context = Context {
            id: node.id.clone(),
            home: node.home(),
            peers: node.roster(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(Mutex::new(None));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let (l, s, a, e) = (
            listener.clone(),
            stop.clone(),
            active.clone(),
            errors.clone(),
        );
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                match l.accept() {
                    Ok((wire, ip)) => {
                        *a.lock() = Some(wire.clone());
                        if s.load(Ordering::SeqCst) {
                            wire.close();
                            break;
                        }
                        if let Err(error) = exchange(&context, &wire, Some(&ip), None) {
                            let mut errors = e.lock();
                            if errors.len() == 32 {
                                errors.remove(0);
                            }
                            errors.push(error);
                        }
                        wire.close();
                        a.lock().take();
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            _node: node,
            stop,
            listener,
            active,
            thread: Some(thread),
            errors,
        })
    }
    pub fn errors(&self) -> Vec<String> {
        self.errors.lock().clone()
    }
}
impl Drop for ReplicaServer<'_> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.listener.close();
        if let Some(socket) = self.active.lock().as_ref() {
            socket.close();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
/// Explicit repair cycle. Applications may schedule it while peers are reachable.
pub fn sync_once(node: &Node, peer: &str, port: u16) -> Result<(), String> {
    let socket = node.connect(peer, port)?;
    let result = exchange(
        &Context {
            id: node.id.clone(),
            home: node.home(),
            peers: node.roster(),
        },
        &socket,
        None,
        Some(peer),
    );
    socket.close();
    result
}
fn send(socket: &ZtSocket, p: &Packet, metrics: &mut Metrics) -> Result<(), String> {
    let bytes = serde_json::to_vec(p).map_err(err)?;
    // Base64-wrapped library sync messages use bounded ordered frames.
    if bytes.len() > HARD_MAX * 5 {
        return Err("交换报文超过限额".into());
    }
    for chunk in bytes.chunks(60000) {
        socket.send(Frame {
            kind: 1,
            data: chunk.to_vec(),
        })?;
    }
    socket.send(Frame {
        kind: 2,
        data: vec![],
    })?;
    metrics.sent += bytes.len() as u64;
    metrics.change_sent += p.replicas.iter().map(|r| r.change_bytes).sum::<u64>();
    Ok(())
}
fn receive(socket: &ZtSocket, end: Instant, metrics: &mut Metrics) -> Result<Packet, String> {
    let mut bytes = Vec::new();
    loop {
        let f = socket.receive(Some(end))?;
        if f.kind == 2 && f.data.is_empty() {
            break;
        }
        if f.kind != 1 || bytes.len() + f.data.len() > HARD_MAX * 5 {
            return Err("共享交换协议或长度无效".into());
        }
        bytes.extend(f.data);
    }
    metrics.received += bytes.len() as u64;
    serde_json::from_slice(&bytes).map_err(err)
}
fn exchange(
    context: &Context,
    socket: &Arc<ZtSocket>,
    incoming_ip: Option<&str>,
    target: Option<&str>,
) -> Result<(), String> {
    let home = &context.home;
    let (mut store, n) = Store::open(home)?;
    let id = context.id.clone();
    let mut states = States::new();
    let mut metrics = Metrics {
        peer: target.unwrap_or_default().into(),
        ..Metrics::default()
    };
    let end = Instant::now() + Duration::from_secs(30);
    let result = (|| -> Result<(), String> {
        let mut sent_quiet = false;
        if incoming_ip.is_none() {
            if !context.peers.lock().contains_key(&metrics.peer) {
                return Err("节点已移出空间".into());
            }
            let replicas =
                store.sync_step(n, &metrics.peer.clone(), &mut states, vec![], &mut metrics)?;
            let packet = Packet {
                version: PROTOCOL_VERSION,
                network: n,
                source: id.clone(),
                replicas,
            };
            sent_quiet = packet.quiet();
            send(socket, &packet, &mut metrics)?;
        }
        for _ in 0..64 {
            if Instant::now() > end {
                return Err("增量同步会话超时".into());
            }
            let remote = receive(socket, end, &mut metrics)?;
            metrics.rounds += 1;
            validate_packet(&remote, n, &id, target, &metrics.peer)?;
            let peers = context.peers.lock();
            let ip = peers.get(&remote.source).ok_or("source is not trusted")?;
            if incoming_ip.is_some_and(|source| source != ip) {
                return Err("source address mismatch".into());
            }
            drop(peers);
            let remote_quiet = remote.quiet();
            metrics.peer = remote.source;
            if incoming_ip.is_none() && sent_quiet && remote_quiet {
                return Ok(());
            }
            let replicas = store.sync_step(
                n,
                &metrics.peer.clone(),
                &mut states,
                remote.replicas,
                &mut metrics,
            )?;
            let packet = Packet {
                version: PROTOCOL_VERSION,
                network: n,
                source: id.clone(),
                replicas,
            };
            sent_quiet = packet.quiet();
            // Only generate/respond after the received data and safe sync state commit.
            send(socket, &packet, &mut metrics)?;
            if incoming_ip.is_some() && sent_quiet && remote_quiet {
                return Ok(());
            }
        }
        Err("增量同步超过64轮".into())
    })();
    store.record(&metrics, &result)?;
    if result.is_ok() {
        store.db.execute("INSERT INTO shared_meta VALUES('last_exchange_ms',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[crate::now_millis().to_string()]).map_err(err)?;
    }
    result
}
