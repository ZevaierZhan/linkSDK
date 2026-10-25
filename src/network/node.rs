use crate::{
    platform::{identity_id, private_dir, write_private},
    runtime::Sdk,
    SpaceConfig,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::{CStr, CString},
    os::raw::c_char,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
unsafe extern "C" fn event(message: *mut std::ffi::c_void) {
    if message.is_null() {
        return;
    }
    let event = &*(message as *const libzt::zts_event_msg_t);
    if event.event_code == 215 {
        READY.store(true, Ordering::SeqCst);
    }
    if event.event_code == 218 {
        READY.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bootstrap {
    pub network_id: u64,
    pub planet: String,
    pub root_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    pub ip: String,
}

pub struct Node {
    _sdk: Sdk,
    pub id: String,
    pub ip: String,
    pub network_id: u64,
    peers: Arc<Mutex<HashMap<String, String>>>,
    bootstrap: Bootstrap,
    running: bool,
    root: bool,
}

fn check(rc: i32, operation: &str) -> Result<(), String> {
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("{operation}: {rc}"))
    }
}
fn wait(mut test: impl FnMut() -> bool, label: &str) -> Result<(), String> {
    let end = Instant::now() + Duration::from_secs(45);
    while Instant::now() < end {
        if test() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("timeout: {label}"))
}

/// Generates a new identity only for an empty profile; never silently replaces a partial one.
pub fn prepare_identity(home: &Path) -> Result<String, String> {
    let _lock = crate::platform::ProfileLock::acquire(home)?;
    let (public, _) = identity(home)?;
    identity_id(&public)
}
fn identity(home: &Path) -> Result<(String, String), String> {
    let home = home.join("space");
    private_dir(&home)?;
    let public = home.join("identity.public");
    let secret = home.join("identity.secret");
    if public.exists() || secret.exists() {
        let p =
            std::fs::read_to_string(&public).map_err(|_| "incomplete identity; restore profile")?;
        let s =
            std::fs::read_to_string(&secret).map_err(|_| "incomplete identity; restore profile")?;
        if identity_id(p.trim())? != identity_id(s.trim())? {
            return Err("identity pair mismatch".into());
        }
        return Ok((p.trim().into(), s.trim().into()));
    }
    let (mut p, mut s) = (vec![0 as c_char; 512], vec![0 as c_char; 512]);
    let (mut pl, mut sl) = (512, 512);
    unsafe {
        check(
            libzt::zts_util_generate_identity(p.as_mut_ptr(), &mut pl, s.as_mut_ptr(), &mut sl),
            "generate identity",
        )?;
    }
    let p = unsafe { CStr::from_ptr(p.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let s = unsafe { CStr::from_ptr(s.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    write_private(&secret, s.as_bytes())?;
    write_private(&public, p.as_bytes())?;
    Ok((p, s))
}

impl Node {
    pub fn prepare_identity(home: impl AsRef<Path>) -> Result<String, String> {
        prepare_identity(home.as_ref())
    }
    pub fn create(home: impl AsRef<Path>, address: &str, port: u16) -> Result<Self, String> {
        address
            .parse::<std::net::IpAddr>()
            .map_err(|_| "invalid root IP")?;
        if port == 0 {
            return Err("root port must be nonzero".into());
        }
        let sdk = Sdk::open(home)?;
        let dir = sdk.home().join("space");
        let (public, secret) = identity(sdk.home())?;
        let id = identity_id(&public)?;
        let config_path = dir.join("bootstrap.json");
        let bootstrap = if config_path.exists() {
            serde_json::from_slice::<Bootstrap>(
                &std::fs::read(config_path).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?
        } else {
            let public = CString::new(public).map_err(|e| e.to_string())?;
            let endpoint = CString::new(format!("{address}/{port}")).map_err(|e| e.to_string())?;
            let mut roots: libzt::zts_root_set_t = unsafe { std::mem::zeroed() };
            roots.public_id_str[0] = public.as_ptr() as *mut _;
            roots.endpoint_ip_str[0][0] = endpoint.as_ptr() as *mut _;
            let mut bytes = vec![0 as c_char; 4 * 1024 * 1024];
            let mut len = bytes.len() as u32;
            let mut key = vec![0 as c_char; 128];
            let mut key_len = key.len() as u32;
            let mut random = [0u8; 8];
            getrandom::fill(&mut random).map_err(|e| e.to_string())?;
            unsafe {
                check(
                    libzt::zts_util_sign_root_set_fresh(
                        bytes.as_mut_ptr(),
                        &mut len,
                        key.as_mut_ptr(),
                        &mut key_len,
                        u64::from_ne_bytes(random).max(1),
                        crate::now_millis(),
                        &mut roots,
                    ),
                    "sign private planet",
                )?;
            }
            write_private(
                &dir.join("planet-signing.key"),
                &key[..key_len as usize]
                    .iter()
                    .map(|v| *v as u8)
                    .collect::<Vec<_>>(),
            )?;
            Bootstrap {
                network_id: 0,
                planet: B64.encode(
                    bytes[..len as usize]
                        .iter()
                        .map(|v| *v as u8)
                        .collect::<Vec<_>>(),
                ),
                root_id: id.clone(),
            }
        };
        if bootstrap.root_id != id {
            return Err("root identity mismatch".into());
        }
        let mut node = Self::start(sdk, &secret, bootstrap, port, true)?;
        if node.network_id == 0 {
            let name = CString::new("linkSDK private space").unwrap();
            unsafe {
                check(
                    libzt::zts_controller_create_network(name.as_ptr(), &mut node.network_id),
                    "create network",
                )?;
            }
            node.bootstrap.network_id = node.network_id;
        }
        node.approve(&node.id)?;
        node.join_network()?;
        node.save()?;
        Ok(node)
    }

    /// Bootstrap must arrive via a trusted out-of-band channel. It is not an invitation.
    pub fn join(home: impl AsRef<Path>, bootstrap: Bootstrap, port: u16) -> Result<Self, String> {
        if bootstrap.network_id == 0
            || identity_id(&bootstrap.root_id)? != format!("{:010x}", bootstrap.network_id >> 24)
        {
            return Err("invalid network/root binding".into());
        }
        let sdk = Sdk::open(home)?;
        let (_, secret) = identity(sdk.home())?;
        if let Ok(bytes) = std::fs::read(sdk.home().join("space/space.json")) {
            let old: SpaceConfig = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if old.network_id != bootstrap.network_id {
                return Err("profile belongs to another space".into());
            }
        }
        let mut node = Self::start(sdk, &secret, bootstrap, port, false)?;
        node.join_network()?;
        node.save()?;
        Ok(node)
    }

    fn start(
        sdk: Sdk,
        secret: &str,
        bootstrap: Bootstrap,
        port: u16,
        root: bool,
    ) -> Result<Self, String> {
        if ACTIVE.swap(true, Ordering::SeqCst) {
            return Err("one node per process; use independent processes".into());
        }
        let planet = B64.decode(&bootstrap.planet).map_err(|e| e.to_string())?;
        let dir = sdk.home().join("space");
        let home = CString::new(dir.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
        let secret = CString::new(secret).map_err(|e| e.to_string())?;
        let mut node = Self {
            _sdk: sdk,
            id: identity_id(secret.to_str().map_err(|e| e.to_string())?)?,
            ip: String::new(),
            network_id: bootstrap.network_id,
            peers: Arc::new(Mutex::new(HashMap::new())),
            bootstrap,
            running: true,
            root,
        };
        unsafe {
            check(
                libzt::zts_init_set_event_handler(Some(event)),
                "set callback",
            )?;
            check(
                libzt::zts_init_from_storage(home.as_ptr()),
                "initialize storage",
            )?;
            check(
                libzt::zts_init_from_memory(secret.as_ptr(), secret.as_bytes().len() as u32),
                "load identity",
            )?;
            check(
                libzt::zts_init_set_roots(planet.as_ptr() as _, planet.len() as u32),
                "load private roots",
            )?;
            check(libzt::zts_init_set_port(port), "set UDP port")?;
            if root {
                check(libzt::zts_init_allow_secondary_port(0), "fixed root port")?;
                check(
                    libzt::zts_init_allow_port_mapping(0),
                    "disable random root port",
                )?;
                check(libzt::zts_init_enable_controller(1), "enable controller")?;
            }
            check(libzt::zts_node_start(), "start node")?;
        }
        wait(
            || unsafe { libzt::zts_node_is_online() != 0 },
            "node online",
        )?;
        node.id = format!("{:010x}", unsafe { libzt::zts_node_get_id() });
        Ok(node)
    }
    fn join_network(&mut self) -> Result<(), String> {
        // Storage restoration can emit READY before join_network is called.
        // Keep that event: rejoining an already joined network need not emit it again.
        unsafe {
            check(libzt::zts_net_join(self.network_id), "join network")?;
        }
        let n = self.network_id;
        wait(
            || READY.load(Ordering::SeqCst) && unsafe { libzt::zts_net_transport_is_ready(n) != 0 },
            "IPv4 transport ready",
        )?;
        let mut bytes = vec![0 as c_char; 64];
        unsafe {
            check(
                libzt::zts_addr_get_str(
                    n,
                    libzt::ZTS_AF_INET,
                    bytes.as_mut_ptr(),
                    bytes.len() as u32,
                ),
                "get IPv4",
            )?;
        }
        self.ip = unsafe { CStr::from_ptr(bytes.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        Ok(())
    }
    fn save(&self) -> Result<(), String> {
        let dir = self._sdk.home().join("space");
        write_private(
            &dir.join("space.json"),
            &serde_json::to_vec(&SpaceConfig {
                network_id: self.network_id,
            })
            .map_err(|e| e.to_string())?,
        )?;
        write_private(
            &dir.join("bootstrap.json"),
            &serde_json::to_vec(&self.bootstrap).map_err(|e| e.to_string())?,
        )
    }
    pub fn approve(&self, id: &str) -> Result<(), String> {
        if !self.root {
            return Err("approval requires the local organizer".into());
        }
        let id = identity_id(id)?;
        let raw = u64::from_str_radix(&id, 16).map_err(|e| e.to_string())?;
        let rc = unsafe { libzt::zts_controller_authorize_member(self.network_id, raw) };
        let mut len = 0;
        unsafe {
            libzt::zts_controller_list_members(self.network_id, std::ptr::null_mut(), &mut len);
        }
        if len == 0 {
            return Err(format!("approval read-back unavailable: {rc}"));
        }
        let mut buf = vec![0 as c_char; len as usize];
        unsafe {
            check(
                libzt::zts_controller_list_members(self.network_id, buf.as_mut_ptr(), &mut len),
                "read approved members",
            )?;
        }
        let text = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
        let members: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if !members
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v["id"] == id && v["authorized"] == true))
        {
            return Err(format!("approval not confirmed: {rc}"));
        }
        Ok(())
    }
    /// Pins a peer identity to its controller-assigned address. Supplied by the trusted owner.
    pub fn trust_peer(&self, peer: Peer) -> Result<(), String> {
        let id = identity_id(&peer.id)?;
        peer.ip
            .parse::<std::net::Ipv4Addr>()
            .map_err(|_| "invalid peer IPv4")?;
        if id == self.id {
            return Err("cannot trust self as a remote peer".into());
        }
        self.peers.lock().insert(id, peer.ip);
        Ok(())
    }
    pub fn peer(&self, id: &str) -> Result<Peer, String> {
        self.peers
            .lock()
            .get(id)
            .map(|ip| Peer {
                id: id.into(),
                ip: ip.clone(),
            })
            .ok_or("peer not in local trusted roster".into())
    }
    pub fn connect(&self, id: &str, port: u16) -> Result<Arc<super::Stream>, String> {
        let peer = self.peer(id)?;
        super::Stream::connect_bounded(&peer.ip, port, Duration::from_secs(5))
    }
    pub fn listen(&self, port: u16) -> Result<Arc<super::Stream>, String> {
        super::Stream::listen(&self.ip, port)
    }
    pub fn bootstrap(&self) -> Bootstrap {
        self.bootstrap.clone()
    }
    pub fn home(&self) -> PathBuf {
        self._sdk.home().to_path_buf()
    }
    #[cfg(feature = "data")]
    pub fn dataset(&self, app: &str, dataset: &str) -> crate::data::Dataset {
        self._sdk.dataset(app, dataset)
    }
    pub fn roster(&self) -> Arc<Mutex<HashMap<String, String>>> {
        self.peers.clone()
    }
    pub fn shutdown(&mut self) {
        if self.running {
            unsafe {
                libzt::zts_node_stop();
            }
            self.running = false;
        }
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        self.shutdown();
    }
}
