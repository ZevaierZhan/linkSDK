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
/// Ordered application-owned frames; implement with OS TCP or the SDK stream.
pub trait FrameChannel {
    fn send_frame(&self, frame: Frame) -> Result<(), String>;
    fn receive_frame(&self, deadline: Instant) -> Result<Frame, String>;
}
impl FrameChannel for ZtSocket {
    fn send_frame(&self, frame: Frame) -> Result<(), String> {
        self.send(frame)
    }
    fn receive_frame(&self, deadline: Instant) -> Result<Frame, String> {
        self.receive(Some(deadline))
    }
}
/// The host serializes bounded local commits with generation retirement.
/// No socket I/O may run while its commit gate is held.
pub trait ReplicationGuard {
    fn current(&self) -> Result<(), String>;
    fn commit<T>(&self, transaction: impl FnOnce() -> Result<T, String>) -> Result<T, String>;
}
struct Unscoped;
impl ReplicationGuard for Unscoped {
    fn current(&self) -> Result<(), String> {
        Ok(())
    }
    fn commit<T>(&self, transaction: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        transaction()
    }
}
fn send(
    socket: &impl FrameChannel,
    p: &Packet,
    metrics: &mut Metrics,
    guard: &impl ReplicationGuard,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(p).map_err(err)?;
    // Base64-wrapped library sync messages use bounded ordered frames.
    if bytes.len() > HARD_MAX * 5 {
        return Err("交换报文超过限额".into());
    }
    for chunk in bytes.chunks(60000) {
        guard.current()?;
        socket.send_frame(Frame {
            kind: 1,
            data: chunk.to_vec(),
        })?;
    }
    guard.current()?;
    socket.send_frame(Frame {
        kind: 2,
        data: vec![],
    })?;
    metrics.sent += bytes.len() as u64;
    metrics.change_sent += p.replicas.iter().map(|r| r.change_bytes).sum::<u64>();
    Ok(())
}
fn receive(
    socket: &impl FrameChannel,
    end: Instant,
    metrics: &mut Metrics,
    guard: &impl ReplicationGuard,
) -> Result<Packet, String> {
    let mut bytes = Vec::new();
    loop {
        guard.current()?;
        let f = socket.receive_frame(end)?;
        guard.current()?;
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
    exchange_with(
        &context.home,
        &context.id,
        socket,
        incoming_ip,
        target,
        |peer| {
            context
                .peers
                .lock()
                .get(peer)
                .cloned()
                .ok_or("source is not trusted".into())
        },
    )
}
/// Reuses an existing host's identity, transport and dynamic member authority.
/// authorize is evaluated before outbound generation and for every received envelope;
/// it must return the currently trusted virtual IP and may register revocation cancellation.
pub fn exchange_with(
    home: &std::path::Path,
    id: &str,
    socket: &Arc<ZtSocket>,
    incoming_ip: Option<&str>,
    target: Option<&str>,
    authorize: impl Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    exchange_channel_with(
        home,
        id,
        socket.as_ref(),
        incoming_ip,
        target,
        authorize,
        &Unscoped,
    )
}
/// Same wire v2 and durable store, with a host-selected channel and generation gate.
pub fn exchange_channel_with(
    home: &std::path::Path,
    id: &str,
    socket: &impl FrameChannel,
    incoming_ip: Option<&str>,
    target: Option<&str>,
    authorize: impl Fn(&str) -> Result<String, String>,
    guard: &impl ReplicationGuard,
) -> Result<(), String> {
    guard.current()?;
    let (store, n) = guard.commit(|| Store::open(home))?;
    exchange_store(store, n, id, socket, incoming_ip, target, authorize, guard)
}
/// Replicate a host-selected directory without fabricating identity/configuration files.
pub fn exchange_in(
    directory: &std::path::Path,
    network_id: u64,
    id: &str,
    socket: &impl FrameChannel,
    incoming_ip: Option<&str>,
    target: Option<&str>,
    authorize: impl Fn(&str) -> Result<String, String>,
    guard: &impl ReplicationGuard,
) -> Result<(), String> {
    guard.current()?;
    let (store, n) = guard.commit(|| Store::open_at(directory, network_id))?;
    exchange_store(store, n, id, socket, incoming_ip, target, authorize, guard)
}
fn exchange_store(
    mut store: Store,
    n: u64,
    id: &str,
    socket: &impl FrameChannel,
    incoming_ip: Option<&str>,
    target: Option<&str>,
    authorize: impl Fn(&str) -> Result<String, String>,
    guard: &impl ReplicationGuard,
) -> Result<(), String> {
    let mut states = States::new();
    let mut metrics = Metrics {
        peer: target.unwrap_or_default().into(),
        ..Metrics::default()
    };
    let end = Instant::now() + Duration::from_secs(30);
    let result = (|| -> Result<(), String> {
        let mut sent_quiet = false;
        if incoming_ip.is_none() {
            authorize(&metrics.peer)?;
            let replicas = guard.commit(|| {
                store.sync_step(n, &metrics.peer.clone(), &mut states, vec![], &mut metrics)
            })?;
            let packet = Packet {
                version: PROTOCOL_VERSION,
                network: n,
                source: id.to_owned(),
                replicas,
            };
            sent_quiet = packet.quiet();
            send(socket, &packet, &mut metrics, guard)?;
        }
        for _ in 0..64 {
            if Instant::now() > end {
                return Err("增量同步会话超时".into());
            }
            let remote = receive(socket, end, &mut metrics, guard)?;
            metrics.rounds += 1;
            validate_packet(&remote, n, &id, target, &metrics.peer)?;
            let ip = authorize(&remote.source)?;
            if incoming_ip.is_some_and(|source| source != ip) {
                return Err("source address mismatch".into());
            }
            let remote_quiet = remote.quiet();
            metrics.peer = remote.source;
            if incoming_ip.is_none() && sent_quiet && remote_quiet {
                return Ok(());
            }
            let replicas = guard.commit(|| {
                store.sync_step(
                    n,
                    &metrics.peer.clone(),
                    &mut states,
                    remote.replicas,
                    &mut metrics,
                )
            })?;
            let packet = Packet {
                version: PROTOCOL_VERSION,
                network: n,
                source: id.to_owned(),
                replicas,
            };
            sent_quiet = packet.quiet();
            // Only generate/respond after the received data and safe sync state commit.
            send(socket, &packet, &mut metrics, guard)?;
            if incoming_ip.is_some() && sent_quiet && remote_quiet {
                return Ok(());
            }
        }
        Err("增量同步超过64轮".into())
    })();
    guard.commit(|| {
    store.record(&metrics, &result)?;
    if result.is_ok() {
        store.db.execute("INSERT INTO shared_meta VALUES('last_exchange_ms',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[crate::now_millis().to_string()]).map_err(err)?;
    }
    Ok(())
    })?;
    result
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    struct Guard(AtomicBool);
    impl ReplicationGuard for Guard {
        fn current(&self) -> Result<(), String> {
            if self.0.load(Ordering::SeqCst) {
                Err("retired".into())
            } else {
                Ok(())
            }
        }
        fn commit<T>(&self, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
            self.current()?;
            f()
        }
    }
    struct RetiringChannel<'a>(&'a Guard);
    impl FrameChannel for RetiringChannel<'_> {
        fn send_frame(&self, _: Frame) -> Result<(), String> {
            panic!("retired input must not produce a response")
        }
        fn receive_frame(&self, _: Instant) -> Result<Frame, String> {
            self.0 .0.store(true, Ordering::SeqCst);
            Ok(Frame {
                kind: 2,
                data: vec![],
            })
        }
    }
    fn home() -> PathBuf {
        std::env::temp_dir().join(format!(
            "sdk-generation-{}-{}",
            std::process::id(),
            crate::now_millis()
        ))
    }
    #[test]
    fn retired_generation_never_opens_the_profile() {
        let home = home();
        let guard = Guard(AtomicBool::new(true));
        assert!(exchange_channel_with(
            &home,
            "aaaaaaaaaa",
            &RetiringChannel(&guard),
            Some("10.0.0.2"),
            None,
            |_| Ok("10.0.0.2".into()),
            &guard
        )
        .is_err());
        assert!(!home.exists());
    }
    #[test]
    fn retirement_during_receive_cannot_apply_changes_or_record_success() {
        let home = home();
        std::fs::create_dir_all(home.join("space")).unwrap();
        std::fs::write(
            home.join("space/space.json"),
            br#"{"network_id":42,"planet_id":42,"space_mode":"assets"}"#,
        )
        .unwrap();
        let guard = Guard(AtomicBool::new(false));
        assert!(exchange_channel_with(
            &home,
            "aaaaaaaaaa",
            &RetiringChannel(&guard),
            Some("10.0.0.2"),
            None,
            |_| Ok("10.0.0.2".into()),
            &guard
        )
        .is_err());
        let (store, _) = Store::open(&home).unwrap();
        for table in ["shared_docs", "shared_sync_metrics"] {
            let n: i64 = store
                .db
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0);
        }
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }
}

#[cfg(test)]
mod explicit_channel_tests {
    use super::*;
    use std::io::{Read, Write};
    struct Tcp(Mutex<std::net::TcpStream>);
    impl FrameChannel for Tcp {
        fn send_frame(&self, frame: Frame) -> Result<(), String> {
            self.0.lock().write_all(&frame.encode()?).map_err(err)
        }
        fn receive_frame(&self, _: Instant) -> Result<Frame, String> {
            {
                let mut socket = self.0.lock();
                let mut header = [0u8; 5];
                socket.read_exact(&mut header).map_err(err)?;
                let size = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
                if size > crate::network::frame::MAX_FRAME {
                    return Err("oversized frame".into());
                }
                let mut data = vec![0; size];
                socket.read_exact(&mut data).map_err(err)?;
                Ok(Frame {
                    kind: header[0],
                    data,
                })
            }
        }
    }
    #[test]
    fn public_directories_exchange_over_os_tcp_without_identity_files() {
        let root = std::env::temp_dir().join(format!(
            "sdk-os-channel-{}-{}",
            std::process::id(),
            crate::now_millis()
        ));
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        crate::data::action_in(&a,42,serde_json::json!({"op":"put","app":"public","dataset":"history","key":"message","value":"retained"})).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let receiver = b.clone();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            exchange_in(
                &receiver,
                42,
                "bbbbbbbbbb",
                &Tcp(Mutex::new(socket)),
                Some("127.0.0.1"),
                None,
                |p| {
                    if p == "aaaaaaaaaa" {
                        Ok("127.0.0.1".into())
                    } else {
                        Err("untrusted".into())
                    }
                },
                &Unscoped,
            )
            .unwrap();
        });
        let socket = std::net::TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        exchange_in(
            &a,
            42,
            "aaaaaaaaaa",
            &Tcp(Mutex::new(socket)),
            None,
            Some("bbbbbbbbbb"),
            |p| {
                if p == "bbbbbbbbbb" {
                    Ok("127.0.0.1".into())
                } else {
                    Err("untrusted".into())
                }
            },
            &Unscoped,
        )
        .unwrap();
        server.join().unwrap();
        let value = crate::data::action_in(
            &b,
            42,
            serde_json::json!({"op":"get","app":"public","dataset":"history","key":"message"}),
        )
        .unwrap();
        assert_eq!(value["message"]["value"], "retained");
        for dir in [&a, &b] {
            assert!(!dir.join("identity.secret").exists());
            assert!(!dir.join("space.json").exists());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
