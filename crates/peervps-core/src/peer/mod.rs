//! Renting between real nodes.
//!
//! Every node has a long-term X25519 key ([`Identity`]); its peer id is
//! `pv-` and the first 8 bytes of that key in hex. Nodes talk over an encrypted
//! TCP [`channel`] that authenticates both keys, one request per connection.
//!
//! Trust is explicit and per key:
//! * **Adding** a peer by address dials it, pins the key it presents and
//!   trusts it (`pv-…@host:port` also checks the id before trusting).
//! * The other side sees an unknown key asking to rent and lists it as
//!   **pending** until its owner **approves** it. Approving also dials back,
//!   so both can rent from each other.
//!
//! Once trusted, a host's own offers appear in the renter's marketplace as
//! `<host id>/<offer id>`. Deploying, scaling, terminating, the console and the
//! login details are carried to the host, which runs the VM under a
//! `peer-<renter id>` account. SSH, Remote Desktop and the installer screen are
//! carried back through the channel to ports on the renter's 127.0.0.1, so
//! nothing on the host listens beyond loopback.
//!
//! Money does not cross nodes yet: a host gives each new peer account a
//! one-time welcome credit and bills it per second in its own ledger.

pub mod channel;
pub mod proto;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

use crate::api::market::Offer;
use crate::billing::{AccountKind, MICROS_PER_CREDIT};
use crate::network::noise::{StaticKeypair, generate_keypair};
use crate::node::{DeployRequest, Instance, Node};
use crate::storage::now_secs;
use crate::virtualization::GuestAccess;
use crate::{Error, Result};
use channel::Channel;
use proto::{GuestPort, Request, Response};

/// Port `peervps serve --peer-listen` and the desktop app use unless told otherwise.
pub const DEFAULT_PORT: u16 = 7071;
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
/// Deploying can take a while on the host (copying a disk, booting QEMU).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
const WELCOME_CREDITS: i64 = 50;
/// Unknown keys are remembered for approval up to this many, so a noisy
/// network cannot grow the peer list without bound.
const MAX_PENDING: usize = 32;

/// This node's long-term key and the peer id derived from it.
#[derive(Debug, Clone)]
pub struct Identity {
    pub keypair: StaticKeypair,
    pub id: String,
}

#[derive(Serialize, Deserialize)]
struct KeyFile {
    private: String,
    public: String,
}

impl Identity {
    pub fn id_for(public: &[u8]) -> String {
        format!("pv-{}", hex::encode(&public[..public.len().min(8)]))
    }

    pub fn generate() -> Result<Self> {
        let keypair = generate_keypair()?;
        Ok(Self { id: Self::id_for(&keypair.public), keypair })
    }

    /// Read the key at `path`, creating it (readable by the owner only) on first run.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let f: KeyFile = serde_json::from_slice(&std::fs::read(path)?)?;
            let bad = |_| Error::Crypto(format!("{} is not a PeerVPS node key", path.display()));
            let keypair = StaticKeypair {
                private: hex::decode(f.private).map_err(bad)?,
                public: hex::decode(f.public).map_err(bad)?,
            };
            if keypair.private.len() != 32 || keypair.public.len() != 32 {
                return Err(Error::Crypto(format!("{} is not a PeerVPS node key", path.display())));
            }
            return Ok(Self { id: Self::id_for(&keypair.public), keypair });
        }
        let me = Self::generate()?;
        let body = serde_json::to_vec_pretty(&KeyFile {
            private: hex::encode(&me.keypair.private),
            public: hex::encode(&me.keypair.public),
        })?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            std::io::Write::write_all(&mut opts.open(&tmp)?, &body)?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(me)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerStatus {
    /// Answering, and its offers are in our marketplace.
    Online,
    /// We trust it, but its owner has not approved us yet.
    WaitingForApproval,
    /// We trust it but could not reach it on the last try.
    Unreachable,
    /// It asked to rent from us; waiting for our owner to approve it.
    Pending,
    /// We trust it and it can rent from us, but we have no address to reach it.
    Inbound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub id: String,
    pub public_key: String,
    /// `host:port` we dial it on.
    pub address: Option<String>,
    pub trusted: bool,
    pub status: PeerStatus,
    /// Its own offers, as it last listed them (ids are the host's).
    #[serde(default)]
    pub offers: Vec<Offer>,
    pub last_seen: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerOverview {
    /// Our own peer id.
    pub id: String,
    /// Where we accept peers, when listening.
    pub listen: Option<String>,
    /// What to give someone so they can add this node: `pv-…@<LAN address>:<port>`.
    pub invite: Option<String>,
    pub peers: Vec<PeerInfo>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedPeer {
    id: String,
    public_key: String,
    address: Option<String>,
    trusted: bool,
}

/// A local listener carrying one port of a remote guest: its port and accept loop.
type Forward = (u16, JoinHandle<()>);

/// The peer network of one node; cheap to clone.
#[derive(Debug, Clone)]
pub struct Peers {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    identity: Identity,
    node: Node,
    file: Option<PathBuf>,
    peers: RwLock<HashMap<String, PeerInfo>>,
    listen: std::sync::Mutex<Option<SocketAddr>>,
    forwards: Mutex<HashMap<(String, GuestPort), Forward>>,
}

impl Peers {
    /// Attach a peer network to `node`. Trusted and pending peers are kept in
    /// `file` (JSON) across restarts; `None` keeps them in memory.
    pub fn attach(node: &Node, identity: Identity, file: Option<PathBuf>) -> Result<Self> {
        let mut peers = HashMap::new();
        if let Some(path) = file.as_ref().filter(|p| p.exists()) {
            let saved: Vec<SavedPeer> = serde_json::from_slice(&std::fs::read(path)?)?;
            for s in saved {
                let status = match (s.trusted, &s.address) {
                    (false, _) => PeerStatus::Pending,
                    (true, Some(_)) => PeerStatus::Unreachable,
                    (true, None) => PeerStatus::Inbound,
                };
                peers.insert(
                    s.id.clone(),
                    PeerInfo {
                        id: s.id,
                        public_key: s.public_key,
                        address: s.address,
                        trusted: s.trusted,
                        status,
                        offers: Vec::new(),
                        last_seen: None,
                        error: None,
                    },
                );
            }
        }
        let me = Self {
            inner: Arc::new(Inner {
                identity,
                node: node.clone(),
                file,
                peers: RwLock::new(peers),
                listen: std::sync::Mutex::new(None),
                forwards: Mutex::new(HashMap::new()),
            }),
        };
        node.attach_peers(me.clone())?;
        Ok(me)
    }

    pub fn id(&self) -> &str {
        &self.inner.identity.id
    }

    fn listen_addr(&self) -> Option<SocketAddr> {
        *self.inner.listen.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Accept peers on `addr`; returns the bound address (useful with port 0).
    pub async fn listen(&self, addr: SocketAddr) -> Result<SocketAddr> {
        let listener = TcpListener::bind(addr).await?;
        let bound = listener.local_addr()?;
        *self.inner.listen.lock().unwrap_or_else(|e| e.into_inner()) = Some(bound);
        tracing::info!(%bound, id = %self.id(), "accepting peers");
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, from)) => {
                        let me = me.clone();
                        tokio::spawn(async move {
                            if let Err(e) = me.serve(stream, from).await {
                                tracing::debug!(%from, error = %e, "peer connection ended");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accepting peers failed");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });
        Ok(bound)
    }

    /// Ask every peer we can dial for its offers now and then every `every`.
    pub fn spawn_refresh(&self, every: Duration) -> JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                me.refresh().await;
                tokio::time::sleep(every).await;
            }
        })
    }

    pub async fn overview(&self) -> PeerOverview {
        let mut peers: Vec<PeerInfo> = self.inner.peers.read().await.values().cloned().collect();
        peers.sort_by(|a, b| a.id.cmp(&b.id));
        let listen = self.listen_addr();
        PeerOverview {
            id: self.id().to_owned(),
            listen: listen.map(|a| a.to_string()),
            invite: listen.map(|a| {
                let ip = if a.ip().is_unspecified() { lan_ip().unwrap_or(a.ip()) } else { a.ip() };
                format!("{}@{}", self.id(), SocketAddr::new(ip, a.port()))
            }),
            peers,
        }
    }

    pub async fn get(&self, id: &str) -> Result<PeerInfo> {
        self.inner.peers.read().await.get(id).cloned().ok_or_else(|| Error::NotFound(format!("peer {id}")))
    }

    /// Add a peer by `host:port` (port defaults to [`DEFAULT_PORT`]), or
    /// `pv-…@host:port` to also check who answers.
    pub async fn add(&self, target: &str) -> Result<PeerInfo> {
        let target = target.trim();
        let (expect, address) = match target.split_once('@') {
            Some((id, addr)) => (Some(id.trim()), addr.trim()),
            None => (None, target),
        };
        if address.is_empty() {
            return Err(Error::Invalid("give the peer's address, e.g. 192.168.1.20:7071".into()));
        }
        let address = if address.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
            address.to_owned()
        } else {
            format!("{address}:{DEFAULT_PORT}")
        };
        let mut ch = self.dial(&address).await?;
        let id = Identity::id_for(&ch.remote);
        if id == self.id() {
            return Err(Error::Invalid("that address is this node".into()));
        }
        if let Some(want) = expect
            && want != id
        {
            return Err(Error::Peer(format!("{address} is {id}, not {want}")));
        }
        let public_key = hex::encode(&ch.remote);
        {
            let mut peers = self.inner.peers.write().await;
            if let Some(known) = peers.get(&id)
                && known.public_key != public_key
            {
                return Err(Error::Peer(format!("{id} presented a different key than before")));
            }
            let entry = peers.entry(id.clone()).or_insert_with(|| PeerInfo {
                id: id.clone(),
                public_key: public_key.clone(),
                address: None,
                trusted: true,
                status: PeerStatus::Unreachable,
                offers: Vec::new(),
                last_seen: None,
                error: None,
            });
            entry.address = Some(address.clone());
            entry.trusted = true;
        }
        let answer =
            self.exchange(&mut ch, &Request::Hello { listen_port: self.listen_addr().map(|a| a.port()) }).await;
        self.note_hello(&id, answer).await;
        self.save().await?;
        self.get(&id).await
    }

    /// Let a pending peer rent from us, and dial it back when we know where.
    pub async fn approve(&self, id: &str) -> Result<PeerInfo> {
        let address = {
            let mut peers = self.inner.peers.write().await;
            let p = peers.get_mut(id).ok_or_else(|| Error::NotFound(format!("peer {id}")))?;
            p.trusted = true;
            p.status = if p.address.is_some() { PeerStatus::Unreachable } else { PeerStatus::Inbound };
            p.address.clone()
        };
        self.save().await?;
        if address.is_some() {
            self.refresh_one(id).await;
        }
        self.get(id).await
    }

    /// Forget a peer. Instances already running on either side keep running.
    pub async fn remove(&self, id: &str) -> Result<()> {
        self.inner.peers.write().await.remove(id).ok_or_else(|| Error::NotFound(format!("peer {id}")))?;
        self.save().await
    }

    pub async fn refresh(&self) {
        let ids: Vec<String> = self
            .inner
            .peers
            .read()
            .await
            .values()
            .filter(|p| p.trusted && p.address.is_some())
            .map(|p| p.id.clone())
            .collect();
        let mut tasks = tokio::task::JoinSet::new();
        for id in ids {
            let me = self.clone();
            tasks.spawn(async move { me.refresh_one(&id).await });
        }
        while tasks.join_next().await.is_some() {}
    }

    async fn refresh_one(&self, id: &str) {
        let hello = Request::Hello { listen_port: self.listen_addr().map(|a| a.port()) };
        let answer = self.call(id, &hello).await;
        self.note_hello(id, answer).await;
    }

    async fn note_hello(&self, id: &str, answer: Result<Response>) {
        let mut peers = self.inner.peers.write().await;
        let Some(p) = peers.get_mut(id) else { return };
        match answer {
            Ok(Response::Welcome { offers, .. }) => {
                p.status = PeerStatus::Online;
                p.offers = offers;
                p.last_seen = Some(now_secs());
                p.error = None;
            }
            Ok(other) => {
                p.status = PeerStatus::Unreachable;
                p.error = Some(format!("unexpected answer {other:?}"));
            }
            Err(Error::Unauthorized(m)) => {
                p.status = PeerStatus::WaitingForApproval;
                p.offers.clear();
                p.last_seen = Some(now_secs());
                p.error = Some(m);
            }
            Err(e) => {
                p.status = PeerStatus::Unreachable;
                p.offers.clear();
                p.error = Some(e.to_string());
            }
        }
    }

    async fn save(&self) -> Result<()> {
        let Some(path) = &self.inner.file else { return Ok(()) };
        let saved: Vec<SavedPeer> = {
            let peers = self.inner.peers.read().await;
            let mut v: Vec<SavedPeer> = peers
                .values()
                .map(|p| SavedPeer {
                    id: p.id.clone(),
                    public_key: p.public_key.clone(),
                    address: p.address.clone(),
                    trusted: p.trusted,
                })
                .collect();
            v.sort_by(|a, b| a.id.cmp(&b.id));
            v
        };
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(&saved)?).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }

    /// Offers of the peers that answered last, as `<peer>/<offer>` from provider `<peer>`.
    pub async fn remote_offers(&self) -> Vec<Offer> {
        self.inner
            .peers
            .read()
            .await
            .values()
            .filter(|p| p.status == PeerStatus::Online)
            .flat_map(|p| {
                p.offers.iter().map(|o| Offer {
                    id: format!("{}/{}", p.id, o.id),
                    provider: p.id.clone(),
                    region: format!("{} via {}", o.region, p.id),
                    ..o.clone()
                })
            })
            .collect()
    }

    pub async fn is_peer(&self, id: &str) -> bool {
        self.inner.peers.read().await.contains_key(id)
    }

    // ---- renting from a peer ----

    async fn dial(&self, address: &str) -> Result<Channel> {
        let unreachable = |e: String| Error::Peer(format!("cannot reach {address}: {e}"));
        let stream = tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(address))
            .await
            .map_err(|_| unreachable("timed out".into()))?
            .map_err(|e| unreachable(e.to_string()))?;
        tokio::time::timeout(DIAL_TIMEOUT, Channel::connect(stream, &self.inner.identity.keypair))
            .await
            .map_err(|_| unreachable("handshake timed out".into()))?
            .map_err(|e| unreachable(e.to_string()))
    }

    /// Dial a trusted peer and check it still has the key we pinned.
    async fn open(&self, id: &str) -> Result<Channel> {
        let (address, key) = {
            let peers = self.inner.peers.read().await;
            let p = peers.get(id).ok_or_else(|| Error::NotFound(format!("peer {id}")))?;
            if !p.trusted {
                return Err(Error::Unauthorized(format!("peer {id} is not approved")));
            }
            let address = p.address.clone().ok_or_else(|| Error::Peer(format!("no address to reach {id}")))?;
            (address, p.public_key.clone())
        };
        let ch = self.dial(&address).await?;
        if hex::encode(&ch.remote) != key {
            return Err(Error::Peer(format!("{address} no longer has {id}'s key")));
        }
        Ok(ch)
    }

    async fn exchange(&self, ch: &mut Channel, req: &Request) -> Result<Response> {
        let host = Identity::id_for(&ch.remote);
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            ch.send(&serde_json::to_vec(req)?).await?;
            let resp: Response = serde_json::from_slice(&ch.recv().await?)?;
            resp.into_result(&host)
        })
        .await
        .map_err(|_| Error::Peer(format!("{host} did not answer in time")))?
    }

    /// One request to a trusted peer.
    pub async fn call(&self, id: &str, req: &Request) -> Result<Response> {
        let mut ch = self.open(id).await?;
        self.exchange(&mut ch, req).await
    }

    pub(crate) async fn deploy(&self, host: &str, offer_id: &str, req: DeployRequest) -> Result<Instance> {
        match self.call(host, &Request::Deploy { offer_id: offer_id.to_owned(), spec: req.spec }).await? {
            Response::Instance { instance } => Ok(instance),
            other => Err(unexpected(host, &other)),
        }
    }

    pub(crate) async fn instance_call(&self, host: &str, req: Request) -> Result<Instance> {
        match self.call(host, &req).await? {
            Response::Instance { instance } => Ok(instance),
            other => Err(unexpected(host, &other)),
        }
    }

    pub(crate) async fn console(&self, host: &str, id: &str, max_bytes: usize) -> Result<Option<String>> {
        match self.call(host, &Request::Console { id: id.to_owned(), max_bytes }).await? {
            Response::Console { console } => Ok(console),
            other => Err(unexpected(host, &other)),
        }
    }

    /// The host's login details with every endpoint moved to a local port that
    /// carries it here.
    pub(crate) async fn access(&self, host: &str, id: &str) -> Result<Option<GuestAccess>> {
        let access = match self.call(host, &Request::Access { id: id.to_owned() }).await? {
            Response::Access { access } => access,
            other => return Err(unexpected(host, &other)),
        };
        let Some(mut a) = access else { return Ok(None) };
        let ssh = self.forward(host, id, GuestPort::Ssh).await?;
        a.ssh_host = "127.0.0.1".into();
        a.ssh_port = ssh;
        if a.rdp.is_some() {
            a.rdp = Some(format!("127.0.0.1:{}", self.forward(host, id, GuestPort::Rdp).await?));
        }
        if a.display.is_some() {
            a.display = Some(format!("vnc://127.0.0.1:{}", self.forward(host, id, GuestPort::Display).await?));
        }
        Ok(Some(a))
    }

    /// A local port whose connections are carried to `port` of a guest on `host`.
    async fn forward(&self, host: &str, id: &str, port: GuestPort) -> Result<u16> {
        let mut forwards = self.inner.forwards.lock().await;
        let key = (id.to_owned(), port);
        if let Some((local, task)) = forwards.get(&key)
            && !task.is_finished()
        {
            return Ok(*local);
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let local = listener.local_addr()?.port();
        let (me, host, id) = (self.clone(), host.to_owned(), id.to_owned());
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (me, host, id) = (me.clone(), host.clone(), id.clone());
                tokio::spawn(async move {
                    let opened = async {
                        let mut ch = me.open(&host).await?;
                        match me.exchange(&mut ch, &Request::Forward { id: id.clone(), port }).await? {
                            Response::Ok => Ok(ch),
                            other => Err(unexpected(&host, &other)),
                        }
                    };
                    match opened.await {
                        Ok(ch) => pipe(ch, tcp).await,
                        Err(e) => tracing::warn!(%host, %id, ?port, error = %e, "could not carry guest port"),
                    }
                });
            }
        });
        forwards.insert(key, (local, task));
        Ok(local)
    }

    /// Close the local ports carrying an instance (it was terminated).
    pub(crate) async fn stop_forwards(&self, id: &str) {
        self.inner.forwards.lock().await.retain(|(inst, _), (_, task)| {
            let keep = inst != id;
            if !keep {
                task.abort();
            }
            keep
        });
    }

    // ---- hosting for a peer ----

    async fn serve(&self, stream: TcpStream, from: SocketAddr) -> Result<()> {
        let mut ch = tokio::time::timeout(DIAL_TIMEOUT, Channel::accept(stream, &self.inner.identity.keypair))
            .await
            .map_err(|_| Error::Peer(format!("{from}: handshake timed out")))??;
        let peer = Identity::id_for(&ch.remote);
        let req = tokio::time::timeout(DIAL_TIMEOUT, ch.recv())
            .await
            .map_err(|_| Error::Peer(format!("{from}: no request")))??;
        let req: Request = serde_json::from_slice(&req)?;
        let answer = match self.admit(&peer, &ch.remote, from.ip(), &req).await {
            Ok(()) => self.handle(&peer, req.clone()).await,
            Err(e) => Err(e),
        };
        let resp = match answer {
            Ok(r) => r,
            Err(e) => Response::from_error(&e),
        };
        ch.send(&serde_json::to_vec(&resp)?).await?;
        if let (Request::Forward { id, port }, Response::Ok) = (&req, &resp) {
            let target = self.guest_port(&peer, id, *port).await?;
            pipe(ch, TcpStream::connect(target).await?).await;
        }
        Ok(())
    }

    /// Is `peer` approved to rent here? Unknown keys are recorded as pending.
    async fn admit(&self, peer: &str, key: &[u8], ip: IpAddr, req: &Request) -> Result<()> {
        let key = hex::encode(key);
        let dial_back = match req {
            Request::Hello { listen_port: Some(port) } => Some(SocketAddr::new(ip, *port).to_string()),
            _ => None,
        };
        let mut changed = false;
        let result = {
            let mut peers = self.inner.peers.write().await;
            let crowded = peers.values().filter(|p| !p.trusted).count() >= MAX_PENDING;
            match peers.get_mut(peer) {
                Some(p) if p.public_key != key => Err(Error::Unauthorized(format!("{peer} uses a different key"))),
                Some(p) => {
                    if p.address.is_none() && dial_back.is_some() {
                        p.address = dial_back;
                        if p.trusted {
                            p.status = PeerStatus::Unreachable;
                        }
                        changed = true;
                    }
                    if p.trusted {
                        if p.status == PeerStatus::Inbound {
                            p.last_seen = Some(now_secs());
                        }
                        Ok(())
                    } else {
                        p.last_seen = Some(now_secs());
                        Err(self.not_approved(peer))
                    }
                }
                None if crowded => Err(Error::Unauthorized(format!("{} has too many pending requests", self.id()))),
                None => {
                    peers.insert(
                        peer.to_owned(),
                        PeerInfo {
                            id: peer.to_owned(),
                            public_key: key,
                            address: dial_back,
                            trusted: false,
                            status: PeerStatus::Pending,
                            offers: Vec::new(),
                            last_seen: Some(now_secs()),
                            error: None,
                        },
                    );
                    changed = true;
                    tracing::info!(%peer, "new peer asks to rent; approve it to let it in");
                    Err(self.not_approved(peer))
                }
            }
        };
        if changed {
            self.save().await?;
        }
        result
    }

    fn not_approved(&self, peer: &str) -> Error {
        Error::Unauthorized(format!("waiting for the owner of {} to approve {peer}", self.id()))
    }

    async fn handle(&self, peer: &str, req: Request) -> Result<Response> {
        let node = &self.inner.node;
        if let Request::Hello { .. } = req {
            return Ok(Response::Welcome { id: self.id().to_owned(), offers: node.local_offers().await });
        }
        let account = self.account_for(peer).await?;
        Ok(match req {
            Request::Hello { .. } => unreachable!("answered above"),
            Request::Deploy { offer_id, spec } => {
                if !node.local_offers().await.iter().any(|o| o.id == offer_id) {
                    return Err(Error::NotFound(format!("offer {offer_id}")));
                }
                Response::Instance { instance: node.deploy(&account, DeployRequest { offer_id, spec }).await? }
            }
            Request::Get { id } => Response::Instance { instance: node.instance(&account, &id).await? },
            Request::Scale { id, replicas } => {
                Response::Instance { instance: node.scale(&account, &id, replicas).await? }
            }
            Request::Terminate { id } => Response::Instance { instance: node.terminate(&account, &id).await? },
            Request::Console { id, max_bytes } => {
                Response::Console { console: node.console(&account, &id, max_bytes.min(256 * 1024)).await? }
            }
            Request::Access { id } => Response::Access { access: node.access(&account, &id).await? },
            Request::Forward { id, port } => {
                self.guest_port(peer, &id, port).await?;
                Response::Ok
            }
        })
    }

    /// The ledger account a peer rents under, opened with a welcome credit.
    async fn account_for(&self, peer: &str) -> Result<String> {
        let account = format!("peer-{peer}");
        let ledger = &self.inner.node.ledger;
        if ledger.balance(&account).await.is_err() {
            ledger.open_account(&account, AccountKind::Renter).await?;
            ledger.top_up(&account, WELCOME_CREDITS * MICROS_PER_CREDIT, &format!("peer-welcome-{peer}")).await?;
        }
        Ok(account)
    }

    /// The loopback address of a guest port the peer's instance exposes here.
    async fn guest_port(&self, peer: &str, id: &str, port: GuestPort) -> Result<SocketAddr> {
        let account = format!("peer-{peer}");
        let access = self
            .inner
            .node
            .access(&account, id)
            .await?
            .ok_or_else(|| Error::Unsupported("this host gives guests no network endpoint".into()))?;
        let target = match port {
            GuestPort::Ssh => Some(format!("{}:{}", access.ssh_host, access.ssh_port)),
            GuestPort::Rdp => access.rdp,
            GuestPort::Display => access.display.map(|d| d.trim_start_matches("vnc://").to_owned()),
        }
        .ok_or_else(|| Error::NotFound(format!("instance {id} has no {port:?} port")))?;
        let addr: SocketAddr = target
            .replace("localhost", "127.0.0.1")
            .parse()
            .map_err(|_| Error::Invalid(format!("guest endpoint {target}")))?;
        // Only ever reach into this machine's loopback, whatever the backend reports.
        if !addr.ip().is_loopback() {
            return Err(Error::Unsupported("guest endpoint is not on this host's loopback".into()));
        }
        Ok(addr)
    }
}

/// The address other machines on the network reach us on: the source address
/// the OS would use for an outside destination (nothing is sent).
fn lan_ip() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified())
}

fn unexpected(host: &str, r: &Response) -> Error {
    Error::Peer(format!("{host} answered {r:?}"))
}

/// Carry bytes between a channel and a TCP socket until both sides are done.
async fn pipe(ch: Channel, tcp: TcpStream) {
    let _ = tcp.set_nodelay(true);
    let (mut tr, mut tw) = tcp.into_split();
    let Channel { mut tx, mut rx, .. } = ch;
    let up = async {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match tr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send_chunk(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        tx.shutdown().await;
    };
    let down = async {
        while let Ok(Some(chunk)) = rx.recv_chunk().await {
            if tw.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = tw.shutdown().await;
    };
    tokio::join!(up, down);
}

#[cfg(test)]
mod tests;
