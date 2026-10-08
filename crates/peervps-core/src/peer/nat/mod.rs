//! Making this machine reachable from other networks.
//!
//! Most home routers speak UPnP IGD: we ask the router for its external
//! address and to forward a TCP port to the port we accept peers on. A STUN
//! query to a public server tells us the address the internet actually sees;
//! when that differs from the router's, or the router's own address is a
//! private or carrier-grade NAT one (100.64/10), there is another NAT in front
//! of the router that nothing on this network can open, and we say so instead
//! of handing out an invite that cannot work.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;

use crate::network::stun;
use crate::{Error, Result};

const SSDP: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 250)), 1900);
const SEARCH_FOR: Duration = Duration::from_secs(3);
/// Routers drop mappings after this; we renew well before.
pub const LEASE_SECS: u32 = 3600;
pub const RENEW_EVERY: Duration = Duration::from_secs(20 * 60);
pub const STUN_SERVERS: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InternetState {
    /// Not asked to be reachable from the internet.
    Off,
    Checking,
    /// Reachable at `address`.
    Open,
    /// The router did not answer UPnP; a port has to be forwarded by hand.
    NoGateway,
    /// Behind the internet provider's NAT (or a second router): not reachable from outside.
    CarrierNat,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InternetStatus {
    pub state: InternetState,
    /// `ip:port` other networks reach us on, when open.
    pub address: Option<String>,
    /// What happened, for a person to read.
    pub detail: Option<String>,
}

impl InternetStatus {
    pub fn off() -> Self {
        Self { state: InternetState::Off, address: None, detail: None }
    }

    fn new(state: InternetState, address: Option<SocketAddr>, detail: impl Into<String>) -> Self {
        Self { state, address: address.map(|a| a.to_string()), detail: Some(detail.into()) }
    }
}

/// Addresses no one outside this network (or this provider) can reach.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || (o[0] == 100 && (64..128).contains(&o[1])) // RFC 6598 carrier-grade NAT
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// Where to look for the router and the outside view; tests point these at fakes.
#[derive(Debug, Clone)]
pub struct NatConfig {
    /// The router's UPnP description URL; found with SSDP when `None`.
    pub igd: Option<String>,
    pub stun: Vec<String>,
}

impl Default for NatConfig {
    fn default() -> Self {
        Self { igd: None, stun: STUN_SERVERS.iter().map(|s| (*s).to_owned()).collect() }
    }
}

/// A router's WAN connection service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gateway {
    pub control_url: String,
    pub service: String,
    /// The router's address on our network.
    pub ip: IpAddr,
}

fn http() -> Result<reqwest::Client> {
    // The router is on the LAN: never send this through a proxy.
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| Error::Peer(format!("http client: {e}")))
}

/// Ask the LAN for an Internet Gateway Device; returns its description URL.
pub async fn discover() -> Result<Option<String>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    for st in [
        "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
        "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
        "urn:schemas-upnp-org:service:WANIPConnection:1",
    ] {
        let msg = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nST: {st}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\n\r\n"
        );
        socket.send_to(msg.as_bytes(), SSDP).await?;
    }
    let mut buf = vec![0u8; 2048];
    let deadline = tokio::time::Instant::now() + SEARCH_FOR;
    loop {
        match tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
            Err(_) => return Ok(None),
            Ok(Err(e)) => return Err(e.into()),
            Ok(Ok((n, _))) => {
                if let Some(location) = ssdp_location(&String::from_utf8_lossy(&buf[..n])) {
                    return Ok(Some(location));
                }
            }
        }
    }
}

/// The `LOCATION` of an SSDP answer that comes from a gateway.
pub fn ssdp_location(answer: &str) -> Option<String> {
    let mut location = None;
    let mut gateway = false;
    for line in answer.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "location" => location = Some(v.to_owned()),
            "st" | "nt" => gateway |= v.contains("InternetGatewayDevice") || v.contains("WANIPConnection"),
            _ => {}
        }
    }
    location.filter(|l| gateway && l.starts_with("http://"))
}

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))? + start;
    Some(xml[start..end].trim())
}

/// Find the WAN connection service in a device description.
pub fn parse_description(location: &str, xml: &str) -> Result<Gateway> {
    let url = reqwest::Url::parse(location).map_err(|e| Error::Peer(format!("router address {location}: {e}")))?;
    let base = tag(xml, "URLBase").and_then(|b| reqwest::Url::parse(b).ok()).unwrap_or_else(|| url.clone());
    let host = url.host_str().ok_or_else(|| Error::Peer("router address has no host".into()))?;
    let ip: IpAddr = host.parse().map_err(|_| Error::Peer(format!("router address {host} is not an IP")))?;
    let mut rest = xml;
    while let Some(i) = rest.find("<service>") {
        let block_end = rest[i..].find("</service>").map(|e| i + e).unwrap_or(rest.len());
        let block = &rest[i..block_end];
        if let (Some(service), Some(control)) = (tag(block, "serviceType"), tag(block, "controlURL"))
            && (service.contains("WANIPConnection") || service.contains("WANPPPConnection"))
        {
            let control_url = base.join(control).map_err(|e| Error::Peer(format!("router control URL: {e}")))?;
            return Ok(Gateway { control_url: control_url.to_string(), service: service.to_owned(), ip });
        }
        rest = &rest[block_end..];
    }
    Err(Error::Peer("the router offers no WAN connection service over UPnP".into()))
}

impl Gateway {
    pub async fn from_location(location: &str) -> Result<Self> {
        let xml = http()?
            .get(location)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| Error::Peer(format!("router description: {e}")))?
            .text()
            .await
            .map_err(|e| Error::Peer(format!("router description: {e}")))?;
        parse_description(location, &xml)
    }

    async fn soap(&self, action: &str, args: &[(&str, String)]) -> Result<String> {
        let body: String = args.iter().map(|(k, v)| format!("<{k}>{v}</{k}>")).collect();
        let envelope = format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{}">{body}</u:{action}></s:Body></s:Envelope>"#,
            self.service
        );
        let resp = http()?
            .post(&self.control_url)
            .header("Content-Type", r#"text/xml; charset="utf-8""#)
            .header("SOAPAction", format!(r#""{}#{action}""#, self.service))
            .body(envelope)
            .send()
            .await
            .map_err(|e| Error::Peer(format!("router {action}: {e}")))?;
        let ok = resp.status().is_success();
        let text = resp.text().await.map_err(|e| Error::Peer(format!("router {action}: {e}")))?;
        if ok {
            return Ok(text);
        }
        let code = tag(&text, "errorCode").unwrap_or("?");
        let desc = tag(&text, "errorDescription").unwrap_or("");
        Err(Error::Peer(format!("router refused {action}: UPnP error {code} {desc}").trim_end().to_owned()))
    }

    pub async fn external_ip(&self) -> Result<IpAddr> {
        let text = self.soap("GetExternalIPAddress", &[]).await?;
        tag(&text, "NewExternalIPAddress")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Error::Peer("router did not say its external address".into()))
    }

    /// Forward TCP `external` on the router to `internal` on this machine.
    pub async fn map(&self, external: u16, internal: SocketAddr) -> Result<()> {
        let args = |lease: u32| {
            vec![
                ("NewRemoteHost", String::new()),
                ("NewExternalPort", external.to_string()),
                ("NewProtocol", "TCP".to_owned()),
                ("NewInternalPort", internal.port().to_string()),
                ("NewInternalClient", internal.ip().to_string()),
                ("NewEnabled", "1".to_owned()),
                ("NewPortMappingDescription", "PeerVPS".to_owned()),
                ("NewLeaseDuration", lease.to_string()),
            ]
        };
        match self.soap("AddPortMapping", &args(LEASE_SECS)).await {
            // 725 OnlyPermanentLeasesSupported: older routers only take lease 0.
            Err(Error::Peer(m)) if m.contains("error 725") => self.soap("AddPortMapping", &args(0)).await.map(drop),
            r => r.map(drop),
        }
    }

    pub async fn unmap(&self, external: u16) -> Result<()> {
        let args = [
            ("NewRemoteHost", String::new()),
            ("NewExternalPort", external.to_string()),
            ("NewProtocol", "TCP".to_owned()),
        ];
        self.soap("DeletePortMapping", &args).await.map(drop)
    }

    /// Our address on the router's network.
    pub fn local_ip(&self) -> Option<IpAddr> {
        let s = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
        s.connect((self.ip, 1900)).ok()?;
        s.local_addr().ok().map(|a| a.ip())
    }
}

/// The address public STUN servers see us at, if any answers.
pub async fn public_ip(servers: &[String]) -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    for server in servers {
        let Ok(Some(addr)) = tokio::net::lookup_host(server.as_str()).await.map(|mut a| a.find(|a| a.is_ipv4())) else {
            continue;
        };
        if let Ok(seen) = stun::query(&socket, addr, Duration::from_secs(2)).await {
            return Some(seen.ip());
        }
    }
    None
}

/// What [`open`] set up on the router, so it can be renewed and removed.
#[derive(Debug, Clone)]
pub struct Mapping {
    pub gateway: Gateway,
    pub external_port: u16,
    pub internal: SocketAddr,
}

/// Make `port` on this machine reachable from other networks if the router allows it.
pub async fn open(cfg: &NatConfig, local_ip: Option<IpAddr>, port: u16) -> (InternetStatus, Option<Mapping>) {
    // A server with a public address needs no router, but only trust that once
    // the outside view confirms it (a sandbox or office may use public-looking ranges).
    if let Some(ip) = local_ip.filter(|ip| !is_private(*ip))
        && public_ip(&cfg.stun).await == Some(ip)
    {
        let addr = SocketAddr::new(ip, port);
        return (InternetStatus::new(InternetState::Open, Some(addr), "this machine has a public address"), None);
    }
    let location = match &cfg.igd {
        Some(l) => Some(l.clone()),
        None => discover().await.unwrap_or(None),
    };
    let Some(location) = location else {
        return (
            InternetStatus::new(
                InternetState::NoGateway,
                None,
                format!(
                    "the router did not answer UPnP; turn UPnP on in its settings, or forward TCP port {port} to this machine"
                ),
            ),
            None,
        );
    };
    match open_on(cfg, &location, local_ip, port).await {
        Ok(r) => r,
        Err(e) => (InternetStatus::new(InternetState::Failed, None, e.to_string()), None),
    }
}

async fn open_on(
    cfg: &NatConfig,
    location: &str,
    local_ip: Option<IpAddr>,
    port: u16,
) -> Result<(InternetStatus, Option<Mapping>)> {
    let gateway = Gateway::from_location(location).await?;
    let router_ip = gateway.external_ip().await?;
    if is_private(router_ip) {
        let detail = format!(
            "the router's own internet address {router_ip} is not public: your provider shares one address between customers (CGNAT), so other networks cannot connect in"
        );
        return Ok((InternetStatus::new(InternetState::CarrierNat, None, detail), None));
    }
    let internal_ip = local_ip
        .or_else(|| gateway.local_ip())
        .ok_or_else(|| Error::Peer("cannot tell this machine's address on the router's network".into()))?;
    let internal = SocketAddr::new(internal_ip, port);
    let mut external_port = None;
    let mut last = None;
    // 718 ConflictInMappingEntry: another machine has the port; try the next ones.
    for candidate in port..port.saturating_add(4) {
        match gateway.map(candidate, internal).await {
            Ok(()) => {
                external_port = Some(candidate);
                break;
            }
            Err(Error::Peer(m)) if m.contains("error 718") => last = Some(m),
            Err(e) => return Err(e),
        }
    }
    let external_port =
        external_port.ok_or_else(|| Error::Peer(last.unwrap_or_else(|| "no free port on the router".into())))?;
    let mapping = Mapping { gateway, external_port, internal };
    if let Some(seen) = public_ip(&cfg.stun).await
        && seen != router_ip
    {
        let _ = mapping.gateway.unmap(external_port).await;
        let detail = format!(
            "the internet sees this network as {seen} but the router thinks it is {router_ip}: there is another NAT in front of the router (a second modem or the provider's), so other networks cannot connect in"
        );
        return Ok((InternetStatus::new(InternetState::CarrierNat, None, detail), None));
    }
    let address = SocketAddr::new(router_ip, external_port);
    let detail = format!("the router forwards TCP {external_port} to {internal}");
    Ok((InternetStatus::new(InternetState::Open, Some(address), detail), Some(mapping)))
}

#[cfg(test)]
pub(crate) mod tests;
