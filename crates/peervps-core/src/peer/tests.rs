use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::api::market::OfferQuery;
use crate::node::InstanceState;
use crate::storage::Store;
use crate::virtualization::accel::AcceleratorKind;
use crate::virtualization::mock::MockHypervisor;
use crate::virtualization::{Hypervisor, Placement, VmId, VmSnapshot, VmSpec};

/// The mock hypervisor, with every guest's SSH "server" being a loopback echo service.
#[derive(Debug)]
struct EchoGuests {
    inner: MockHypervisor,
    port: u16,
}

#[async_trait]
impl Hypervisor for EchoGuests {
    fn name(&self) -> &'static str {
        "echo"
    }
    async fn create(&self, id: VmId, spec: &VmSpec, p: &Placement) -> crate::Result<()> {
        self.inner.create(id, spec, p).await
    }
    async fn start(&self, id: VmId) -> crate::Result<()> {
        self.inner.start(id).await
    }
    async fn pause(&self, id: VmId) -> crate::Result<()> {
        self.inner.pause(id).await
    }
    async fn resume(&self, id: VmId) -> crate::Result<()> {
        self.inner.resume(id).await
    }
    async fn snapshot(&self, id: VmId) -> crate::Result<VmSnapshot> {
        self.inner.snapshot(id).await
    }
    async fn restore(&self, s: VmSnapshot, p: &Placement) -> crate::Result<()> {
        self.inner.restore(s, p).await
    }
    async fn destroy(&self, id: VmId) -> crate::Result<()> {
        self.inner.destroy(id).await
    }
    async fn console_tail(&self, _id: VmId, _max: usize) -> crate::Result<Option<String>> {
        Ok(Some("login: ".into()))
    }
    async fn access(&self, _id: VmId) -> crate::Result<Option<GuestAccess>> {
        Ok(Some(GuestAccess {
            ssh_host: "127.0.0.1".into(),
            ssh_port: self.port,
            user: "peervps".into(),
            password: Some("secret".into()),
            windows: false,
            rdp: None,
            display: None,
            display_password: None,
        }))
    }
}

async fn echo_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let port = l.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    port
}

fn host_offer(node: &Node) -> Offer {
    Offer {
        id: "this-machine".into(),
        provider: node.config.node_id.clone(),
        region: "local".into(),
        vcpus: 4,
        mem_mib: 8192,
        disk_gib: 100,
        accelerator: AcceleratorKind::None,
        accelerator_model: None,
        vram_mib: 0,
        price_per_sec: 600,
        sla_pct: 100.0,
        confidential: false,
        collateral_locked: 0,
    }
}

async fn node_with(hv: Arc<dyn Hypervisor>) -> (Node, String, Peers) {
    let (node, key) = Node::demo_with_hypervisor(Store::in_memory().expect("store"), hv).await.expect("node");
    let renter = node.authenticate(&key).await.expect("renter");
    let peers = Peers::attach(&node, Identity::generate().expect("key"), None).expect("attach");
    peers.listen("127.0.0.1:0".parse().expect("addr")).await.expect("listen");
    (node, renter, peers)
}

fn spec() -> VmSpec {
    VmSpec {
        vcpus: 2,
        mem_mib: 2048,
        disk_gib: 20,
        image: "ubuntu-24.04".into(),
        accelerator: None,
        confidential: false,
    }
}

#[tokio::test]
async fn a_node_rents_a_vm_from_its_peer_after_approval() {
    let echo = echo_server().await;
    let (host, _, host_peers) = node_with(Arc::new(EchoGuests { inner: MockHypervisor::default(), port: echo })).await;
    host.publish_offer(host_offer(&host)).await;
    let (renter_node, renter, peers) = node_with(Arc::new(MockHypervisor::default())).await;
    let host_addr = host_peers.overview().await.listen.expect("listening");

    // Adding pins the host's key; the host has not let us in yet.
    let added = peers.add(&format!("{}@{host_addr}", host_peers.id())).await.expect("add");
    assert_eq!(added.status, PeerStatus::WaitingForApproval, "{added:?}");
    assert!(renter_node.offers(&OfferQuery::default()).await.iter().all(|o| !o.id.contains('/')));
    let pending = host_peers.get(peers.id()).await.expect("host saw us");
    assert_eq!((pending.status, pending.trusted), (PeerStatus::Pending, false));
    assert!(pending.address.is_some(), "the host can dial back");
    let refused = peers.call(host_peers.id(), &Request::Deploy { offer_id: "this-machine".into(), spec: spec() }).await;
    assert!(matches!(refused, Err(Error::Unauthorized(_))), "{refused:?}");

    // Approving lets us in and dials us back, so renting works both ways.
    let approved = host_peers.approve(peers.id()).await.expect("approve");
    assert_eq!(approved.status, PeerStatus::Online, "{approved:?}");
    peers.refresh().await;
    assert_eq!(peers.get(host_peers.id()).await.expect("peer").status, PeerStatus::Online);
    let offers = renter_node.offers(&OfferQuery::default()).await;
    let remote_id = format!("{}/this-machine", host_peers.id());
    let offer = offers.iter().find(|o| o.id == remote_id).expect("host offer listed");
    assert_eq!(offer.provider, host_peers.id());

    let inst = renter_node
        .deploy(&renter, DeployRequest { offer_id: remote_id.clone(), spec: spec() })
        .await
        .expect("remote deploy");
    assert_eq!(inst.host.as_deref(), Some(host_peers.id()));
    assert_eq!((inst.renter.as_str(), inst.offer_id.as_str()), (renter.as_str(), remote_id.as_str()));
    assert_eq!(host.provisioner.list().await.len(), 1, "the VM runs on the host");
    assert!(renter_node.provisioner.list().await.is_empty(), "not on the renter");
    let account = format!("peer-{}", peers.id());
    assert_eq!(host.instances(&account).await.len(), 1);
    assert_eq!(host.ledger.balance(&account).await.expect("peer account"), WELCOME_CREDITS * MICROS_PER_CREDIT);

    // SSH reaches the guest through a local port.
    let access = renter_node.access(&renter, &inst.id).await.expect("access").expect("endpoint");
    assert_eq!(access.ssh_host, "127.0.0.1");
    assert_ne!(access.ssh_port, echo, "carried, not the host's own port");
    let mut s = TcpStream::connect(("127.0.0.1", access.ssh_port)).await.expect("connect");
    s.write_all(b"SSH-2.0-test\r\n").await.expect("write");
    let mut buf = [0u8; 14];
    s.read_exact(&mut buf).await.expect("read");
    assert_eq!(&buf, b"SSH-2.0-test\r\n");
    drop(s);
    let again = renter_node.access(&renter, &inst.id).await.expect("access").expect("endpoint");
    assert_eq!(again.ssh_port, access.ssh_port, "one local port per guest port");

    assert_eq!(renter_node.console(&renter, &inst.id, 1024).await.expect("console").as_deref(), Some("login: "));
    let z = renter_node.scale(&renter, &inst.id, 0).await.expect("to zero");
    assert_eq!((z.state, z.host.as_deref()), (InstanceState::ScaledToZero, Some(host_peers.id())));
    assert_eq!(renter_node.scale(&renter, &inst.id, 1).await.expect("up").state, InstanceState::Running);
    let gone = renter_node.terminate(&renter, &inst.id).await.expect("terminate");
    assert_eq!(gone.state, InstanceState::Terminated);
    assert_eq!(host.instances(&account).await[0].state, InstanceState::Terminated);
    assert!(TcpStream::connect(("127.0.0.1", access.ssh_port)).await.is_err(), "local port closed");
}

#[tokio::test]
async fn keys_are_checked_and_peers_only_rent_local_offers() {
    let (host, _, host_peers) = node_with(Arc::new(MockHypervisor::default())).await;
    let (_, _, peers) = node_with(Arc::new(MockHypervisor::default())).await;
    let overview = host_peers.overview().await;
    let addr = overview.listen.expect("listening");
    assert_eq!(overview.invite.as_deref(), Some(format!("{}@{addr}", host_peers.id()).as_str()));

    let wrong = peers.add(&format!("pv-0000000000000000@{addr}")).await;
    assert!(matches!(wrong, Err(Error::Peer(_))), "{wrong:?}");
    assert!(peers.get(host_peers.id()).await.is_err(), "not trusted on a mismatch");
    let me = peers.overview().await.listen.expect("listening");
    assert!(matches!(peers.add(&me).await, Err(Error::Invalid(_))), "cannot add itself");
    assert!(matches!(peers.add("127.0.0.1:1").await, Err(Error::Peer(_))));

    peers.add(&addr).await.expect("add");
    host_peers.approve(peers.id()).await.expect("approve");
    // The demo marketplace's offers belong to other providers: not the host's to rent out.
    let demo = peers.call(host_peers.id(), &Request::Deploy { offer_id: "fra-cpu-1".into(), spec: spec() }).await;
    assert!(matches!(demo, Err(Error::NotFound(_))), "{demo:?}");
    assert!(host.provisioner.list().await.is_empty());
}

#[tokio::test]
async fn identity_and_trusted_peers_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("peervps-peer-{}", uuid::Uuid::new_v4().simple()));
    let key = dir.join("node.key");
    let first = Identity::load_or_create(&key).expect("create");
    let again = Identity::load_or_create(&key).expect("load");
    assert_eq!((first.id.clone(), first.keypair.public.clone()), (again.id, again.keypair.public));
    assert!(first.id.starts_with("pv-") && first.id.len() == 19);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&key).expect("meta").permissions().mode() & 0o777, 0o600);
    }

    let (host, _, host_peers) = node_with(Arc::new(MockHypervisor::default())).await;
    let _ = host;
    let addr = host_peers.overview().await.listen.expect("listening");
    let file = dir.join("peers.json");
    let (node, _) = Node::demo().await.expect("node");
    let peers = Peers::attach(&node, first.clone(), Some(file.clone())).expect("attach");
    assert!(Peers::attach(&node, first.clone(), None).is_err(), "one peer network per node");
    peers.add(&addr).await.expect("add");

    let (node2, _) = Node::demo().await.expect("node");
    let restored = Peers::attach(&node2, first, Some(file)).expect("reattach");
    let p = restored.get(host_peers.id()).await.expect("remembered");
    assert!(p.trusted && p.address.as_deref() == Some(addr.as_str()));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn finds_machines_on_the_lan_and_opens_the_router_port() {
    let (_, _, a) = node_with(Arc::new(MockHypervisor::default())).await;
    let (_, _, b) = node_with(Arc::new(MockHypervisor::default())).await;
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("probe");
    let beacons: SocketAddr = probe.local_addr().expect("addr");
    drop(probe);
    b.discover(beacons, vec![beacons]).await.expect("b listens");
    a.discover("127.0.0.1:0".parse().expect("addr"), vec![beacons]).await.expect("a announces");
    let a_addr = a.overview().await.listen.expect("listening");
    let mut nearby = Vec::new();
    for _ in 0..40 {
        nearby = b.overview().await.nearby;
        if !nearby.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(nearby.len(), 1, "{nearby:?}");
    assert_eq!(nearby[0].invite, format!("{}@{a_addr}", a.id()));
    b.add(&nearby[0].invite).await.expect("add from the nearby list");
    assert!(b.overview().await.nearby.is_empty(), "peers are not listed as nearby");

    let (igd, _) = super::nat::tests::fake_router("203.0.113.7", vec![], false).await;
    a.set_nat_config(NatConfig { igd: Some(igd), stun: vec![] });
    assert_eq!(a.set_internet(true).await.expect("on").state, InternetState::Checking);
    let mut view = a.overview().await;
    for _ in 0..40 {
        if view.internet.state != InternetState::Checking {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        view = a.overview().await;
    }
    assert_eq!(view.internet.state, InternetState::Open, "{:?}", view.internet);
    let port = a_addr.rsplit_once(':').expect("port").1;
    assert_eq!(view.internet_invite, Some(format!("{}@203.0.113.7:{port}", a.id())));
    assert_eq!(a.set_internet(false).await.expect("off").state, InternetState::Off);
    assert_eq!(a.overview().await.internet_invite, None);
}
