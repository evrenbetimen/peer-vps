//! Linux TUN interface + the packet pump between it and the UDP tunnel.
//!
//! Creating the interface needs `CAP_NET_ADMIN`, so this path is exercised on
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
pub fn create(name: &str, address: Ipv4Addr, prefix: u8) -> Result<tun::AsyncDevice> {
    let netmask = Ipv4Addr::from(u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0));
    let mut cfg = tun::Configuration::default();
    cfg.tun_name(name).address(address).netmask(netmask).mtu(TUNNEL_MTU).up();
    tun::create_as_async(&cfg).map_err(|e| Error::Io(std::io::Error::other(format!("create tun {name}: {e}"))))
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
