//! TUN interface + the packet pump between it and the UDP tunnel.
//!
//! Linux uses `/dev/net/tun` and needs `CAP_NET_ADMIN`; macOS uses a kernel
//! `utun` control socket and needs root. Either way this path is exercised on
//! real hosts only; the pieces it composes are unit-tested individually.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use super::TUNNEL_MTU;
use super::codec::TunnelCodec;
use super::routing::RoutingTable;
use crate::{Error, Result};

/// Create and bring up `name` with `address/prefix`.
///
/// macOS only accepts `utun<N>` names; pass `None` to let the OS pick one.
pub fn create(name: Option<&str>, address: Ipv4Addr, prefix: u8) -> Result<tun::AsyncDevice> {
    if let Some(name) = name {
        validate_name(name)?;
    }
    let netmask = Ipv4Addr::from(u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0));
    let mut cfg = tun::Configuration::default();
    if let Some(name) = name {
        cfg.tun_name(name);
    }
    cfg.address(address).netmask(netmask).mtu(TUNNEL_MTU).up();
    let name = name.unwrap_or("(auto)");
    tun::create_as_async(&cfg).map_err(|e| Error::Io(std::io::Error::other(format!("create tun {name}: {e}"))))
}

/// Interface names are at most 15 bytes (IFNAMSIZ − 1); macOS also requires `utun<digits>`.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 15 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::Invalid(format!("bad interface name {name:?}")));
    }
    if cfg!(target_os = "macos")
        && !name.strip_prefix("utun").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(Error::Invalid(format!("macOS tunnel names must look like utun7, got {name:?}")));
    }
    Ok(())
}

/// Codecs keyed by peer id.
pub type Sessions = Arc<Mutex<HashMap<String, TunnelCodec>>>;

/// Forward packets both ways until either side errors.
pub async fn run_pump(
    dev: tun::AsyncDevice,
    udp: Arc<UdpSocket>,
    routes: RoutingTable,
    sessions: Sessions,
    peers_by_addr: Arc<Mutex<HashMap<SocketAddr, String>>>,
) -> Result<()> {
    let dev = Arc::new(dev);
    tokio::try_join!(
        pump_outbound(dev.clone(), udp.clone(), routes, sessions.clone()),
        pump_inbound(dev, udp, sessions, peers_by_addr),
    )
    .map(|_| ())
}

async fn pump_outbound(
    dev: Arc<tun::AsyncDevice>,
    udp: Arc<UdpSocket>,
    routes: RoutingTable,
    sessions: Sessions,
) -> Result<()> {
    let mut buf = vec![0u8; 65_535];
    loop {
        let n = dev.recv(&mut buf).await?;
        let Ok(route) = routes.route_packet(&buf[..n]) else { continue };
        let datagram = {
            let mut s = sessions.lock().await;
            let Some(codec) = s.get_mut(&route.peer_id) else { continue };
            codec.seal(&buf[..n])?
        };
        udp.send_to(&datagram, route.endpoint).await?;
    }
}

async fn pump_inbound(
    dev: Arc<tun::AsyncDevice>,
    udp: Arc<UdpSocket>,
    sessions: Sessions,
    peers_by_addr: Arc<Mutex<HashMap<SocketAddr, String>>>,
) -> Result<()> {
    let mut buf = vec![0u8; 65_535];
    loop {
        let (n, from) = udp.recv_from(&mut buf).await?;
        let Some(peer) = peers_by_addr.lock().await.get(&from).cloned() else { continue };
        let packet = {
            let mut s = sessions.lock().await;
            let Some(codec) = s.get_mut(&peer) else { continue };
            match codec.open(&buf[..n], 65_535) {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(%from, error = %e, "dropping tunnel datagram");
                    continue;
                }
            }
        };
        dev.send(&packet).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_names() {
        assert!(validate_name("").is_err());
        assert!(validate_name("a-name-that-is-way-too-long").is_err());
        assert!(validate_name("bad/name").is_err());
        assert!(validate_name("utun7").is_ok());
        assert_eq!(validate_name("pvps0").is_ok(), cfg!(target_os = "linux"));
    }
}
