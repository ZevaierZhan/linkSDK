//! The public SDK IPv4 field is renamed by Windows headers during bindgen.
use std::net::Ipv4Addr;
pub(crate) fn encode(ip: Ipv4Addr) -> libzt::zts_in_addr {
    let value = u32::from_ne_bytes(ip.octets());
    #[cfg(windows)]
    {
        libzt::zts_in_addr { S_addr: value }
    }
    #[cfg(not(windows))]
    {
        libzt::zts_in_addr { s_addr: value }
    }
}
pub(crate) fn decode(address: &libzt::zts_in_addr) -> Ipv4Addr {
    #[cfg(windows)]
    let value = address.S_addr;
    #[cfg(not(windows))]
    let value = address.s_addr;
    Ipv4Addr::from(value.to_ne_bytes())
}
#[cfg(test)]
mod tests {
    #[test]
    fn sdk_ipv4_roundtrip() {
        assert_eq!(std::mem::size_of::<libzt::zts_in_addr>(), 4);
        for ip in [
            std::net::Ipv4Addr::new(10, 247, 0, 122),
            std::net::Ipv4Addr::BROADCAST,
        ] {
            assert_eq!(super::decode(&super::encode(ip)), ip);
        }
    }
}
