//! Standalone examples: parent drives two independent SDK processes with JSON over stdin.
//! Stdin is an owner-only bootstrap/approval channel, never a network admin endpoint.
use link_sdk::network::{frame::Frame, Bootstrap, Node, Stream};
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    net::UdpSocket,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
const PORT: u16 = 19996;
#[cfg(feature = "data")]
const SYNC: u16 = 19997;

pub fn entry(shared: bool) {
    if let Err(error) = run(shared) {
        eprintln!("EXAMPLE_FAILED: {error}");
        std::process::exit(1);
    }
}
fn run(shared: bool) -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--identity") {
        let id = Node::prepare_identity(args.get(2).ok_or("missing profile")?)?;
        println!("SDK:{}", json!({"id":id}));
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("--worker") {
        return worker(&args, shared);
    }
    demonstrate(shared)
}
fn reply(value: Value) {
    println!("SDK:{value}");
    let _ = std::io::stdout().flush();
}
fn worker(args: &[String], shared: bool) -> Result<(), String> {
    #[cfg(not(feature = "data"))]
    let _ = shared;
    let role = args.get(2).ok_or("missing role")?;
    let home = PathBuf::from(args.get(3).ok_or("missing profile")?);
    let port = args
        .get(4)
        .ok_or("missing port")?
        .parse::<u16>()
        .map_err(|e| e.to_string())?;
    let config = args.get(5).ok_or("missing bootstrap/address")?;
    let node = if role == "root" {
        Node::create(&home, config, port)?
    } else {
        let bootstrap: Bootstrap =
            serde_json::from_slice(&fs::read(config).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        Node::join(&home, bootstrap, port)?
    };
    let echo = EchoServer::start(&node)?;
    #[cfg(feature = "data")]
    let replica = if shared {
        Some(link_sdk::data::replication::ReplicaServer::start(
            &node, SYNC,
        )?)
    } else {
        None
    };
    reply(
        json!({"ready":true,"id":node.id,"ip":node.ip,"network":node.network_id,"bootstrap":node.bootstrap(),"pid":std::process::id()}),
    );
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let req: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if req["op"] == "stop" {
            reply(json!({"stopped":true}));
            break;
        }
        let result = (|| -> Result<Value, String> {
            match req["op"].as_str().ok_or("missing op")? {
                "approve" => {
                    node.approve(req["id"].as_str().ok_or("missing id")?)?;
                    Ok(json!({"approved":true}))
                }
                "trust" => {
                    node.trust_peer(
                        serde_json::from_value(req["peer"].clone()).map_err(|e| e.to_string())?,
                    )?;
                    Ok(json!({"trusted":true}))
                }
                "echo" => {
                    let peer = req["peer"].as_str().ok_or("missing peer")?;
                    let socket = node.connect(peer, PORT)?;
                    socket.send(Frame::json(1,&json!({"network":node.network_id,"source":node.id,"target":peer,"text":req["text"]}))?)?;
                    let frame = socket.receive(Some(Instant::now() + Duration::from_secs(8)))?;
                    socket.close();
                    let response: Value = frame.parse()?;
                    if frame.kind != 2
                        || response["source"] != peer
                        || response["text"] != req["text"]
                    {
                        return Err("echo response mismatch".into());
                    }
                    Ok(response)
                }
                #[cfg(feature = "data")]
                "put" => node.dataset("example.v1", "notes").put(
                    req["key"].as_str().ok_or("missing key")?,
                    req["value"].clone(),
                ),
                #[cfg(feature = "data")]
                "list" => node.dataset("example.v1", "notes").list(),
                #[cfg(feature = "data")]
                "heads" => node.dataset("example.v1", "notes").heads(),
                #[cfg(feature = "data")]
                "sync" => {
                    link_sdk::data::replication::sync_once(
                        &node,
                        req["peer"].as_str().ok_or("missing peer")?,
                        SYNC,
                    )?;
                    Ok(json!({"synced":true}))
                }
                _ => Err("unknown operation".into()),
            }
        })();
        match result {
            Ok(value) => reply(json!({"ok":value})),
            Err(error) => reply(json!({"error":error})),
        }
    }
    #[cfg(feature = "data")]
    drop(replica);
    drop(echo);
    drop(node);
    Ok(())
}

struct EchoServer {
    stop: Arc<AtomicBool>,
    listener: Arc<Stream>,
    active: Arc<Mutex<Option<Arc<Stream>>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl EchoServer {
    fn start(node: &Node) -> Result<Self, String> {
        let listener = node.listen(PORT)?;
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(Mutex::new(None));
        let (l, s, a) = (listener.clone(), stop.clone(), active.clone());
        let peers = node.roster();
        let local = node.id.clone();
        let network = node.network_id;
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                let Ok((wire, ip)) = l.accept() else { break };
                *a.lock().unwrap() = Some(wire.clone());
                if s.load(Ordering::SeqCst) {
                    wire.close();
                    break;
                }
                let result = (|| -> Result<(), String> {
                    let frame = wire.receive(Some(Instant::now() + Duration::from_secs(5)))?;
                    let value: Value = frame.parse()?;
                    let source = value["source"].as_str().ok_or("missing source")?;
                    if frame.kind != 1
                        || value["target"] != local
                        || value["network"] != network
                        || peers.lock().get(source) != Some(&ip)
                    {
                        return Err("untrusted echo source".into());
                    }
                    wire.send(Frame::json(
                        2,
                        &json!({"source":local,"text":value["text"]}),
                    )?)
                })();
                if let Err(error) = result {
                    eprintln!("echo request rejected: {error}");
                }
                wire.close();
                a.lock().unwrap().take();
            }
        });
        Ok(Self {
            stop,
            listener,
            active,
            thread: Some(thread),
        })
    }
}
impl Drop for EchoServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.listener.close();
        if let Some(wire) = self.active.lock().unwrap().as_ref() {
            wire.close();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Process {
    child: Child,
    input: ChildStdin,
    output: std::sync::mpsc::Receiver<Value>,
    ready: Value,
}
impl Process {
    fn start(
        root: &Path,
        name: &str,
        role: &str,
        home: &Path,
        port: u16,
        config: &str,
    ) -> Result<Self, String> {
        let log =
            File::create(root.join(format!("{name}.stderr.log"))).map_err(|e| e.to_string())?;
        let mut child = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
            .args([
                "--worker",
                role,
                home.to_str().ok_or("non-UTF8 profile")?,
                &port.to_string(),
                config,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .map_err(|e| e.to_string())?;
        let input = child.stdin.take().ok_or("stdin unavailable")?;
        let output = child.stdout.take().ok_or("stdout unavailable")?;
        let (tx, rx) = std::sync::mpsc::channel();
        let raw =
            File::create(root.join(format!("{name}.stdout.log"))).map_err(|e| e.to_string())?;
        std::thread::spawn(move || {
            let mut raw = raw;
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                let _ = writeln!(raw, "{line}");
                if let Some(v) = line
                    .strip_prefix("SDK:")
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        let mut p = Self {
            child,
            input,
            output: rx,
            ready: Value::Null,
        };
        p.ready = p
            .output
            .recv_timeout(Duration::from_secs(100))
            .map_err(|e| format!("{name} startup: {e}; inspect logs"))?;
        if p.ready["ready"] != true {
            return Err(format!("startup failed: {}", p.ready));
        }
        Ok(p)
    }
    fn call(&mut self, value: Value) -> Result<Value, String> {
        writeln!(self.input, "{value}").map_err(|e| e.to_string())?;
        self.input.flush().map_err(|e| e.to_string())?;
        let response = self
            .output
            .recv_timeout(Duration::from_secs(40))
            .map_err(|e| e.to_string())?;
        if let Some(error) = response.get("error") {
            return Err(error.to_string());
        }
        Ok(response.get("ok").cloned().unwrap_or(response))
    }
    fn stop(&mut self) -> Result<(), String> {
        self.call(json!({"op":"stop"}))?;
        let end = Instant::now() + Duration::from_secs(8);
        while Instant::now() < end {
            if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("worker exit: {status}"))
                };
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err("worker did not shut down".into())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
fn port() -> Result<u16, String> {
    Ok(UdpSocket::bind("0.0.0.0:0")
        .map_err(|e| e.to_string())?
        .local_addr()
        .map_err(|e| e.to_string())?
        .port())
}
fn identity(home: &Path) -> Result<String, String> {
    let output = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .arg("--identity")
        .arg(home)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into());
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|l| {
            l.strip_prefix("SDK:")
                .and_then(|v| serde_json::from_str::<Value>(v).ok())
                .and_then(|v| v["id"].as_str().map(str::to_owned))
        })
        .ok_or("identity output missing".into())
}
fn ensure(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn demonstrate(shared: bool) -> Result<(), String> {
    let mode = if shared { "shared-kv" } else { "p2p-echo" };
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let root = PathBuf::from("artifacts").join(format!("{mode}-{run}-{}", std::process::id()));
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    let root = fs::canonicalize(root).map_err(|e| e.to_string())?;
    let home_a = root.join("a");
    let home_b = root.join("b");
    let id_a = identity(&home_a)?;
    let id_b = identity(&home_b)?;
    let address = match std::env::var("LINKSDK_TEST_ROOT_IP") {
        Ok(ip) => ip,
        Err(_) => {
            let s = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
            s.connect("192.0.2.1:9").map_err(|e| e.to_string())?;
            s.local_addr().map_err(|e| e.to_string())?.ip().to_string()
        }
    };
    let mut a = Process::start(&root, "a", "root", &home_a, port()?, &address)?;
    ensure(a.ready["id"] == id_a, "root identity changed")?;
    a.call(json!({"op":"approve","id":id_b}))?;
    let bootstrap = root.join("bootstrap.json");
    fs::write(
        &bootstrap,
        serde_json::to_vec_pretty(&a.ready["bootstrap"]).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let b_port = port()?;
    let mut b = Process::start(
        &root,
        "b",
        "peer",
        &home_b,
        b_port,
        bootstrap.to_str().ok_or("invalid path")?,
    )?;
    ensure(
        b.ready["id"] == id_b && id_a != id_b,
        "distinct stable identities required",
    )?;
    let peer_a = json!({"id":id_a,"ip":a.ready["ip"]});
    let peer_b = json!({"id":id_b,"ip":b.ready["ip"]});
    a.call(json!({"op":"trust","peer":peer_b}))?;
    b.call(json!({"op":"trust","peer":peer_a}))?;
    let mut checks = vec![
        "independent identities",
        "private Planet and explicit organizer approval",
    ];
    let result = (|| -> Result<(), String> {
        let first = b.call(json!({"op":"echo","peer":id_a,"text":"hello from B"}))?;
        ensure(first["text"] == "hello from B", "B to A echo mismatch")?;
        let second = a.call(json!({"op":"echo","peer":id_b,"text":"hello from A"}))?;
        ensure(second["text"] == "hello from A", "A to B echo mismatch")?;
        checks.push("bidirectional real libzt TCP echo");
        #[cfg(feature = "data")]
        if shared {
            a.call(json!({"op":"put","key":"baseline","value":"shared baseline"}))?;
            a.call(json!({"op":"sync","peer":id_b}))?;
            ensure(
                b.call(json!({"op":"list"}))?["baseline"]["value"] == "shared baseline",
                "baseline not replicated",
            )?;
            b.stop()?;
            // No Node is running in this process: this write must be purely local.
            {
                let sdk = link_sdk::runtime::Sdk::open(&home_b)?;
                sdk.dataset("example.v1", "notes")
                    .put("same", json!("B offline"))?;
            }
            a.call(json!({"op":"put","key":"same","value":"A concurrent"}))?;
            checks.push("offline durable write without network runtime");
            b = Process::start(
                &root,
                "b-restarted",
                "peer",
                &home_b,
                b_port,
                bootstrap.to_str().ok_or("invalid path")?,
            )?;
            ensure(b.ready["id"] == id_b, "identity lost on restart")?;
            a.call(json!({"op":"trust","peer":{"id":id_b,"ip":b.ready["ip"]}}))?;
            b.call(json!({"op":"trust","peer":peer_a}))?;
            // Network configuration readiness is not proof of end-to-end reachability.
            // Exercise the public repair operation until real transport recovers.
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                match b.call(json!({"op":"sync","peer":id_a})) {
                    Ok(_) => break,
                    Err(error) if Instant::now() < deadline => {
                        eprintln!("repair waiting for reconnect: {error}");
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    Err(error) => return Err(error),
                }
            }
            let entries_a = a.call(json!({"op":"list"}))?;
            let entries_b = b.call(json!({"op":"list"}))?;
            ensure(
                entries_a["same"]["conflict"] == true
                    && entries_b["same"]["versions"]
                        .as_array()
                        .is_some_and(|v| v.len() == 2),
                "concurrent versions were lost",
            )?;
            ensure(
                a.call(json!({"op":"heads"}))? == b.call(json!({"op":"heads"}))?,
                "replica heads diverged",
            )?;
            checks.push("reconnect converges and preserves both concurrent versions");
            a.call(json!({"op":"put","key":"same","value":"resolved"}))?;
            a.call(json!({"op":"sync","peer":id_b}))?;
            ensure(
                b.call(json!({"op":"list"}))?["same"]["conflict"] == false,
                "observed conflict not resolved",
            )?;
            b.stop()?;
            a.stop()?;
            let sdk = link_sdk::runtime::Sdk::open(&home_b)?;
            let saved = sdk.dataset("example.v1", "notes").list()?;
            ensure(
                saved["same"]["value"] == "resolved"
                    && saved["baseline"]["value"] == "shared baseline",
                "restart lost durable data",
            )?;
            checks.push("conflict resolution and full shutdown persistence");
        }
        if !shared {
            b.stop()?;
            a.stop()?;
        }
        Ok(())
    })();
    let report = json!({"result":if result.is_ok(){"passed"}else{"failed"},"example":mode,"transport":"real libzt TCP; two independent processes; private Planet","checks":checks,"root":root,"nodes":{"a":a.ready,"b":b.ready},"error":result.as_ref().err()});
    fs::write(
        root.join("report.json"),
        serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!("{report}");
    result
}
