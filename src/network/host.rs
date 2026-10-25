//! Low-level host adapter for an existing application lifecycle and admission policy.
//! Does not create another Node, identity directory, event loop, or membership authority.
use crate::platform::{private_dir, write_private};
use std::{
    ffi::{CStr, CString},
    os::raw::c_char,
    path::Path,
};
pub type EventHandler = Option<unsafe extern "C" fn(*mut std::ffi::c_void)>;
fn check(label: &str, rc: i32) -> Result<(), String> {
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("{label}: {rc}"))
    }
}
pub fn identity_id(public: &str) -> Result<String, String> {
    crate::platform::identity_id(public)
}
pub fn load_or_create_identity(dir: &Path) -> Result<(String, String), String> {
    private_dir(dir)?;
    let public = dir.join("identity.public");
    let secret = dir.join("identity.secret");
    if public.exists() || secret.exists() {
        let p =
            std::fs::read_to_string(public).map_err(|_| "incomplete identity; restore profile")?;
        let s =
            std::fs::read_to_string(secret).map_err(|_| "incomplete identity; restore profile")?;
        if identity_id(p.trim())? != identity_id(s.trim())? {
            return Err("identity pair mismatch".into());
        }
        return Ok((p.trim().into(), s.trim().into()));
    }
    let (mut p, mut s) = (vec![0 as c_char; 512], vec![0 as c_char; 512]);
    let (mut pl, mut sl) = (512, 512);
    unsafe {
        check(
            "generate identity",
            libzt::zts_util_generate_identity(p.as_mut_ptr(), &mut pl, s.as_mut_ptr(), &mut sl),
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
pub struct SignedPlanet {
    pub planet: Vec<u8>,
    pub signing_key: Vec<u8>,
    pub planet_id: u64,
}
pub fn sign_planet(public: &str, address: &str, port: u16) -> Result<SignedPlanet, String> {
    address
        .parse::<std::net::IpAddr>()
        .map_err(|_| "invalid root IP")?;
    if port == 0 {
        return Err("root port must be nonzero".into());
    }
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
    let planet_id = u64::from_ne_bytes(random).max(1);
    unsafe {
        check(
            "sign private planet",
            libzt::zts_util_sign_root_set_fresh(
                bytes.as_mut_ptr(),
                &mut len,
                key.as_mut_ptr(),
                &mut key_len,
                planet_id,
                crate::now_millis(),
                &mut roots,
            ),
        )?;
    }
    Ok(SignedPlanet {
        planet: bytes[..len as usize].iter().map(|v| *v as u8).collect(),
        signing_key: key[..key_len as usize].iter().map(|v| *v as u8).collect(),
        planet_id,
    })
}
/// Caller owns one process-wide libzt lifecycle, event callback and shutdown ordering.
pub fn start(
    dir: &Path,
    secret: &str,
    planet: &[u8],
    port: u16,
    controller: bool,
    callback: EventHandler,
) -> Result<(), String> {
    let home = CString::new(dir.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    let secret = CString::new(secret).map_err(|e| e.to_string())?;
    unsafe {
        check("set callback", libzt::zts_init_set_event_handler(callback))?;
        check(
            "initialize storage",
            libzt::zts_init_from_storage(home.as_ptr()),
        )?;
        check(
            "load identity",
            libzt::zts_init_from_memory(secret.as_ptr(), secret.as_bytes().len() as u32),
        )?;
        check(
            "load private roots",
            libzt::zts_init_set_roots(planet.as_ptr() as _, planet.len() as u32),
        )?;
        check("set UDP port", libzt::zts_init_set_port(port))?;
        if controller {
            check("fixed root port", libzt::zts_init_allow_secondary_port(0))?;
            check(
                "disable random root port",
                libzt::zts_init_allow_port_mapping(0),
            )?;
            check("enable controller", libzt::zts_init_enable_controller(1))?;
        }
        check("start node", libzt::zts_node_start())
    }
}
pub fn overlay_ipv4(network: u64) -> Option<String> {
    let mut bytes = vec![0 as c_char; 64];
    if unsafe {
        libzt::zts_addr_get_str(
            network,
            libzt::ZTS_AF_INET,
            bytes.as_mut_ptr(),
            bytes.len() as u32,
        )
    } != 0
    {
        return None;
    }
    let ip = unsafe { CStr::from_ptr(bytes.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    ip.parse::<std::net::Ipv4Addr>().ok().map(|_| ip)
}
