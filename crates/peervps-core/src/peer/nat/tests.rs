use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};

use super::*;

const DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"><device>
  <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
  <serviceList><service>
    <serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
    <controlURL>/l3f</controlURL>
  </service></serviceList>
  <deviceList><device><deviceList><device><serviceList><service>
    <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
    <controlURL>/ctl/IPConn</controlURL>
  </service></serviceList></device></deviceList></device></deviceList>
</device></root>"#;

#[derive(Default)]
pub(crate) struct FakeRouter {
    external_ip: String,
    /// External ports already taken by another machine.
    taken: Vec<u16>,
    only_permanent: bool,
    calls: Vec<String>,
}

pub(crate) type Fake = Arc<Mutex<FakeRouter>>;

async fn control(State(fake): State<Fake>, headers: HeaderMap, body: String) -> (StatusCode, String) {
    let action = headers.get("soapaction").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
    let mut f = fake.lock().expect("lock");
    f.calls.push(format!("{action} {body}"));
    let fault = |code: u16| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>{code}</errorCode><errorDescription>nope</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"
            ),
        )
    };
    if action.contains("#GetExternalIPAddress") {
        return (StatusCode::OK, format!("<NewExternalIPAddress>{}</NewExternalIPAddress>", f.external_ip));
    }
    if action.contains("#AddPortMapping") {
        let port: u16 = tag(&body, "NewExternalPort").and_then(|p| p.parse().ok()).unwrap_or(0);
        if f.taken.contains(&port) {
            return fault(718);
        }
        if f.only_permanent && tag(&body, "NewLeaseDuration") != Some("0") {
            return fault(725);
        }
        return (StatusCode::OK, String::new());
    }
    if action.contains("#DeletePortMapping") {
        return (StatusCode::OK, String::new());
    }
    fault(401)
}

pub(crate) async fn fake_router(external_ip: &str, taken: Vec<u16>, only_permanent: bool) -> (String, Fake) {
    let fake: Fake =
        Arc::new(Mutex::new(FakeRouter { external_ip: external_ip.into(), taken, only_permanent, calls: vec![] }));
    let app = Router::new()
        .route("/rootDesc.xml", get(|| async { DESCRIPTION }))
        .route("/ctl/IPConn", post(control))
        .with_state(fake.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = l.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(l, app).await });
    (format!("http://{addr}/rootDesc.xml"), fake)
}

/// A STUN server on loopback that reports `seen` as the caller's address.
async fn fake_stun(seen: IpAddr) -> String {
    let s = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let addr = s.local_addr().expect("addr");
    tokio::spawn(async move {
        let mut buf = [0u8; 512];
        while let Ok((n, from)) = s.recv_from(&mut buf).await {
            let mut tid = [0u8; 12];
            tid.copy_from_slice(&buf[8..20.min(n)]);
            let _ = s.send_to(&stun::binding_response(&tid, SocketAddr::new(seen, from.port())), from).await;
        }
    });
    addr.to_string()
}

const LAN: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));

#[tokio::test]
async fn opens_a_port_on_the_router_and_reports_the_internet_address() {
    let (igd, fake) = fake_router("203.0.113.7", vec![7071], false).await;
    let stun = fake_stun("203.0.113.7".parse().expect("ip")).await;
    let cfg = NatConfig { igd: Some(igd), stun: vec![stun] };
    let (status, mapping) = open(&cfg, Some(LAN), 7071).await;
    assert_eq!(status.state, InternetState::Open, "{status:?}");
    assert_eq!(status.address.as_deref(), Some("203.0.113.7:7072"), "7071 was taken, so the next port");
    let mapping = mapping.expect("mapping");
    assert_eq!((mapping.external_port, mapping.internal), (7072, SocketAddr::new(LAN, 7071)));
    let calls = fake.lock().expect("lock").calls.clone();
    let add = calls.iter().rfind(|c| c.contains("#AddPortMapping")).expect("mapped");
    for part in
        ["<NewInternalClient>192.168.1.10<", "<NewInternalPort>7071<", "<NewProtocol>TCP<", "<NewLeaseDuration>3600<"]
    {
        assert!(add.contains(part), "{part} in {add}");
    }
    mapping.gateway.unmap(7072).await.expect("unmap");
}

#[tokio::test]
async fn retries_with_a_permanent_lease_for_old_routers() {
    let (igd, fake) = fake_router("203.0.113.7", vec![], true).await;
    let (status, _) = open(&NatConfig { igd: Some(igd), stun: vec![] }, Some(LAN), 7071).await;
    assert_eq!(status.state, InternetState::Open, "{status:?}");
    assert!(fake.lock().expect("lock").calls.iter().any(|c| c.contains("<NewLeaseDuration>0<")));
}

#[tokio::test]
async fn says_so_when_the_provider_shares_one_address() {
    let (igd, fake) = fake_router("100.72.14.3", vec![], false).await;
    let (status, mapping) = open(&NatConfig { igd: Some(igd), stun: vec![] }, Some(LAN), 7071).await;
    assert_eq!(status.state, InternetState::CarrierNat, "{status:?}");
    assert!(status.detail.expect("detail").contains("CGNAT"));
    assert!(mapping.is_none());
    assert!(!fake.lock().expect("lock").calls.iter().any(|c| c.contains("#AddPortMapping")), "no pointless mapping");

    // Router has a public address but the internet sees another one: a second NAT in front.
    let (igd, fake) = fake_router("203.0.113.7", vec![], false).await;
    let stun = fake_stun("198.51.100.9".parse().expect("ip")).await;
    let (status, mapping) = open(&NatConfig { igd: Some(igd), stun: vec![stun] }, Some(LAN), 7071).await;
    assert_eq!(status.state, InternetState::CarrierNat, "{status:?}");
    assert!(mapping.is_none());
    assert!(fake.lock().expect("lock").calls.iter().any(|c| c.contains("#DeletePortMapping")), "mapping taken back");
}

#[tokio::test]
async fn public_machines_and_missing_routers() {
    let public: IpAddr = "203.0.113.50".parse().expect("ip");
    let stun = fake_stun(public).await;
    let (status, _) = open(&NatConfig { igd: None, stun: vec![stun] }, Some(public), 7071).await;
    assert_eq!((status.state, status.address.as_deref()), (InternetState::Open, Some("203.0.113.50:7071")));
    let (igd, _) = fake_router("203.0.113.7", vec![], false).await;
    let broken = igd.replace("rootDesc.xml", "missing.xml");
    let (status, _) = open(&NatConfig { igd: Some(broken), stun: vec![] }, Some(LAN), 7071).await;
    assert_eq!(status.state, InternetState::Failed, "{status:?}");
}

#[test]
fn parses_ssdp_answers_and_descriptions() {
    let answer = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nLOCATION: http://192.168.1.1:5000/rootDesc.xml\r\n\r\n";
    assert_eq!(ssdp_location(answer).as_deref(), Some("http://192.168.1.1:5000/rootDesc.xml"));
    assert_eq!(
        ssdp_location("HTTP/1.1 200 OK\r\nST: upnp:rootdevice\r\nLocation: http://192.168.1.5/x\r\n"),
        None,
        "a TV is not a router"
    );
    let g = parse_description("http://192.168.1.1:5000/rootDesc.xml", DESCRIPTION).expect("gateway");
    assert_eq!(g.control_url, "http://192.168.1.1:5000/ctl/IPConn");
    assert_eq!(g.service, "urn:schemas-upnp-org:service:WANIPConnection:1");
    assert_eq!(g.ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
    assert!(parse_description("http://192.168.1.1/", "<root/>").is_err());
    for (ip, private) in
        [("100.64.0.1", true), ("100.128.0.1", false), ("10.0.0.1", true), ("8.8.8.8", false), ("fd00::1", true)]
    {
        assert_eq!(is_private(ip.parse().expect("ip")), private, "{ip}");
    }
}
