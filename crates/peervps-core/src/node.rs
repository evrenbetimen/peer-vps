//! A fully wired PeerVPS node: the façade the REST API, CLI and desktop app share.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::api::market::{Offer, OfferQuery, demo_offers};
use crate::billing::payments::WebhookVerifier;
use crate::billing::{AccountKind, Collateral, Ledger, LedgerEntry, MICROS_PER_CREDIT, UsageMeter};
use crate::events::EventBus;
use crate::failover::FailoverController;
use crate::failover::heartbeat::HeartbeatConfig;
use crate::failover::sla::{SlaEnforcer, SlaPolicy};
use crate::network::{OVERLAY_NET, Route, RoutingTable};
use crate::peer::Peers;
use crate::peer::proto::Request as PeerRequest;
use crate::storage::{Store, now_secs};
use crate::virtualization::accel::{AcceleratorDevice, AcceleratorKind, PartitionedAccelerators};
use crate::virtualization::confidential::{self, MemoryEncryption, NoEncryption, SevSnpStub, TeeKind};
use crate::virtualization::mock::MockHypervisor;
use crate::virtualization::{GuestAccess, HostBudget, Hypervisor, Provisioner, VmId, VmSpec, VmState};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InstanceState {
    Running,
    ScaledToZero,
    Terminated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: String,
    pub vm: VmId,
    pub renter: String,
    pub offer_id: String,
    pub spec: VmSpec,
    pub state: InstanceState,
    pub virtual_ip: Ipv4Addr,
    pub price_per_sec: i64,
    pub created_at: i64,
    /// Peer id of the node running it, when rented from another node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip)]
    billing_segment: u32,
}

impl Instance {
    fn billing_id(&self) -> String {
        format!("{}#{}", self.id, self.billing_segment)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployRequest {
    pub offer_id: String,
    pub spec: VmSpec,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    pub account: String,
    pub balance: i64,
    pub history: Vec<LedgerEntry>,
}

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub node_id: String,
    pub budget: HostBudget,
    pub fee_bps: i64,
    pub min_collateral: i64,
    pub webhook_secret: Vec<u8>,
    /// Seconds of runway a renter must hold before deploying.
    pub min_runway_secs: i64,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            node_id: "local".into(),
            budget: HostBudget { cores: (0..8).collect(), mem_mib: 32 * 1024, disk_gib: 500 },
            fee_bps: 500,
            min_collateral: 100 * MICROS_PER_CREDIT,
            webhook_secret: b"whsec_dev_only".to_vec(),
            min_runway_secs: 60,
        }
    }
}

/// Everything a node runs, behind cheap clones.
#[derive(Debug, Clone)]
pub struct Node {
    pub config: Arc<NodeConfig>,
    pub events: EventBus,
    pub store: Store,
    pub ledger: Ledger,
    pub meter: UsageMeter,
    pub collateral: Collateral,
    pub provisioner: Provisioner,
    pub routes: RoutingTable,
    pub failover: FailoverController,
    pub webhooks: WebhookVerifier,
    offers: Arc<RwLock<Vec<Offer>>>,
    instances: Arc<Mutex<HashMap<String, Instance>>>,
    api_keys: Arc<RwLock<HashMap<blake3::Hash, String>>>,
    next_vip: Arc<Mutex<u32>>,
    peers: Arc<std::sync::OnceLock<Peers>>,
}

impl Node {
    pub fn new(config: NodeConfig, store: Store, hypervisor: Arc<dyn Hypervisor>) -> Result<Self> {
        let events = EventBus::default();
        let ledger = Ledger::new(store.clone(), events.clone())?;
        let meter = UsageMeter::new(ledger.clone(), config.fee_bps);
        let collateral = Collateral::new(ledger.clone(), config.min_collateral);
        let tee: Arc<dyn MemoryEncryption> = match confidential::probe_host() {
            TeeKind::None => Arc::new(NoEncryption),
            kind => Arc::new(SevSnpStub { kind }),
        };
        let accel = PartitionedAccelerators::new(vec![AcceleratorDevice {
            id: "gpu0".into(),
            kind: AcceleratorKind::Gpu,
            model: "Simulated GPU".into(),
            vram_mib: 24 * 1024,
            slices: 4,
        }]);
        let provisioner = Provisioner::new(hypervisor, Arc::new(accel), tee, events.clone(), config.budget.clone());
        let routes = RoutingTable::new(events.clone());
        let sla = SlaEnforcer::new(store.clone(), collateral.clone(), SlaPolicy::default());
        let failover = FailoverController::new(HeartbeatConfig::default(), routes.clone(), Some(sla), events.clone());
        let webhooks = WebhookVerifier::new(config.webhook_secret.clone(), 300);
        Ok(Self {
            config: Arc::new(config),
            events,
            store,
            ledger,
            meter,
            collateral,
            provisioner,
            routes,
            failover,
            webhooks,
            offers: Arc::new(RwLock::new(Vec::new())),
            instances: Arc::new(Mutex::new(HashMap::new())),
            api_keys: Arc::new(RwLock::new(HashMap::new())),
            next_vip: Arc::new(Mutex::new(10)),
            peers: Arc::new(std::sync::OnceLock::new()),
        })
    }

    /// In-memory node with the mock hypervisor, demo offers and a funded demo renter.
    /// Returns the node and the demo renter's API key.
    pub async fn demo() -> Result<(Self, String)> {
        Self::demo_with(Store::in_memory()?).await
    }

    /// Like [`Self::demo`] but persisting to `store`. Seeding is skipped if the
    /// demo accounts already exist, so restarting keeps balances.
    pub async fn demo_with(store: Store) -> Result<(Self, String)> {
        Self::demo_with_hypervisor(store, Arc::new(MockHypervisor::default())).await
    }

    /// Demo marketplace and accounts on top of a real hypervisor backend.
    pub async fn demo_with_hypervisor(store: Store, hypervisor: Arc<dyn Hypervisor>) -> Result<(Self, String)> {
        let node = Self::new(NodeConfig::default(), store, hypervisor)?;
        *node.offers.write().await = demo_offers();
        let key = "pvps_demo_key".to_owned();
        if node.ledger.balance("demo-agent").await.is_ok() {
            node.create_renter("demo-agent", &key).await?;
            return Ok((node, key));
        }
        node.ledger.open_account(&node.config.node_id, AccountKind::Provider).await?;
        node.ledger.top_up(&node.config.node_id, 250 * MICROS_PER_CREDIT, "demo-seed-provider").await?;
        node.collateral.lock(&node.config.node_id, 150 * MICROS_PER_CREDIT).await?;
        node.create_renter("demo-agent", &key).await?;
        node.ledger.top_up("demo-agent", 50 * MICROS_PER_CREDIT, "demo-seed-renter").await?;
        Ok((node, key))
    }

    pub async fn create_renter(&self, account: &str, api_key: &str) -> Result<()> {
        self.ledger.open_account(account, AccountKind::Renter).await?;
        self.api_keys.write().await.insert(blake3::hash(api_key.as_bytes()), account.to_owned());
        Ok(())
    }

    /// Resolve an API key to its account. Hash lookup avoids timing leaks on the key itself.
    pub async fn authenticate(&self, api_key: &str) -> Result<String> {
        self.api_keys
            .read()
            .await
            .get(&blake3::hash(api_key.as_bytes()))
            .cloned()
            .ok_or_else(|| Error::Unauthorized("invalid api key".into()))
    }

    pub async fn publish_offer(&self, offer: Offer) {
        let mut offers = self.offers.write().await;
        offers.retain(|o| o.id != offer.id);
        offers.push(offer);
    }

    /// Our own offers and those of the peers we rent from.
    pub async fn offers(&self, query: &OfferQuery) -> Vec<Offer> {
        let mut all = self.offers.read().await.clone();
        if let Some(peers) = self.peers() {
            all.extend(peers.remote_offers().await);
        }
        query.apply(&all)
    }

    /// Offers this machine runs itself (what peers may rent).
    pub async fn local_offers(&self) -> Vec<Offer> {
        self.offers.read().await.iter().filter(|o| o.provider == self.config.node_id).cloned().collect()
    }

    /// The peer network, once one is attached.
    pub fn peers(&self) -> Option<&Peers> {
        self.peers.get()
    }

    pub(crate) fn attach_peers(&self, peers: Peers) -> Result<()> {
        self.peers.set(peers).map_err(|_| Error::Invalid("node already has a peer network".into()))
    }

    /// The peer running `inst`, if it is rented from another node.
    fn remote<'a>(&'a self, inst: &Instance) -> Result<Option<(&'a Peers, String)>> {
        match &inst.host {
            None => Ok(None),
            Some(host) => {
                let peers = self.peers().ok_or_else(|| Error::Peer(format!("no peer network to reach {host}")))?;
                Ok(Some((peers, host.clone())))
            }
        }
    }

    async fn deploy_remote(&self, peers: &Peers, renter: &str, host: &str, req: DeployRequest) -> Result<Instance> {
        let offer_id = req.offer_id.clone();
        let price = peers
            .remote_offers()
            .await
            .into_iter()
            .find(|o| o.id == offer_id)
            .map(|o| o.price_per_sec)
            .ok_or_else(|| Error::NotFound(format!("offer {offer_id}")))?;
        let needed = price * self.config.min_runway_secs;
        let available = self.ledger.balance(renter).await?;
        if available < needed {
            return Err(Error::InsufficientFunds { needed, available });
        }
        // The host bills from what we prepaid, so pay before asking it to start.
        peers.fund(host, renter, self.host_rate(host).await + price).await?;
        let (_, remote_offer) = offer_id.split_once('/').unwrap_or((host, &offer_id));
        let theirs = match peers.deploy(host, remote_offer, req).await {
            Ok(theirs) => theirs,
            Err(e) => {
                self.settle_host(peers, host, renter).await;
                return Err(e);
            }
        };
        let inst =
            Instance { renter: renter.to_owned(), offer_id, host: Some(host.to_owned()), billing_segment: 0, ..theirs };
        self.instances.lock().await.insert(inst.id.clone(), inst.clone());
        Ok(inst)
    }

    /// What our running rentals on `host` cost per second.
    async fn host_rate(&self, host: &str) -> i64 {
        self.instances
            .lock()
            .await
            .values()
            .filter(|i| i.host.as_deref() == Some(host) && i.state == InstanceState::Running)
            .map(|i| i.price_per_sec)
            .sum()
    }

    /// Once nothing of ours runs on `host`, take back what we prepaid there.
    async fn settle_host(&self, peers: &Peers, host: &str, renter: &str) {
        if self.host_rate(host).await > 0 {
            return;
        }
        if let Err(e) = peers.settle(host, renter).await {
            tracing::warn!(%host, error = %e, "could not take back prepaid credits");
        }
    }

    /// Our rentals on other nodes, refreshed from their hosts where reachable.
    pub(crate) async fn sync_remote(&self) -> Vec<Instance> {
        let ours: Vec<Instance> = self
            .instances
            .lock()
            .await
            .values()
            .filter(|i| i.host.is_some() && i.state != InstanceState::Terminated)
            .cloned()
            .collect();
        let Some(peers) = self.peers() else { return ours };
        let mut out = Vec::with_capacity(ours.len());
        for inst in ours {
            let host = inst.host.clone().unwrap_or_default();
            out.push(match peers.instance_call(&host, PeerRequest::Get { id: inst.id.clone() }).await {
                Ok(theirs) => self.keep_remote(&inst, theirs).await,
                Err(_) => inst,
            });
        }
        out
    }

    /// The meter ran out of a renter's credits: stop the VM so it does not run unpaid.
    pub async fn suspend_exhausted(&self, billing_id: &str) -> Result<()> {
        let inst = self.instances.lock().await.values().find(|i| i.billing_id() == billing_id).cloned();
        let Some(inst) = inst.filter(|i| i.state == InstanceState::Running) else { return Ok(()) };
        self.provisioner.hibernate(inst.vm).await?;
        let id = inst.id.clone();
        self.instances.lock().await.insert(id, Instance { state: InstanceState::ScaledToZero, ..inst });
        Ok(())
    }

    /// Stop everything before the process exits: terminate every live
    /// instance (local ones stop billing and their VM; rentals on peers are
    /// ended there and refunded), then destroy any VM still left. Each step
    /// gets `per_step` so an unreachable peer cannot hold up the exit.
    ///
    /// Instances live only in memory, so anything left running would be
    /// orphaned: unreachable from the next start, and billed until its
    /// prepaid credit ran out.
    pub async fn shutdown(&self, per_step: Duration) {
        let live: Vec<Instance> =
            self.instances.lock().await.values().filter(|i| i.state != InstanceState::Terminated).cloned().collect();
        for inst in live {
            match tokio::time::timeout(per_step, self.terminate(&inst.renter, &inst.id)).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(id = %inst.id, error = %e, "could not terminate on shutdown"),
                Err(_) => tracing::warn!(id = %inst.id, "terminating on shutdown timed out"),
            }
        }
        for vm in self.provisioner.list().await {
            if let Err(e) = tokio::time::timeout(per_step, self.provisioner.destroy(vm.id))
                .await
                .unwrap_or_else(|_| Err(Error::Hypervisor("timed out".into())))
            {
                tracing::warn!(vm = %vm.id, error = %e, "could not stop a VM on shutdown");
            }
        }
    }

    /// Store the host's view of a remote instance under our renter and offer id.
    async fn keep_remote(&self, ours: &Instance, theirs: Instance) -> Instance {
        let inst = Instance {
            renter: ours.renter.clone(),
            offer_id: ours.offer_id.clone(),
            host: ours.host.clone(),
            billing_segment: 0,
            ..theirs
        };
        self.instances.lock().await.insert(inst.id.clone(), inst.clone());
        inst
    }

    pub async fn deploy(&self, renter: &str, req: DeployRequest) -> Result<Instance> {
        if let Some(peers) = self.peers()
            && let Some((host, _)) = req.offer_id.split_once('/')
            && peers.is_peer(host).await
        {
            let host = host.to_owned();
            return self.deploy_remote(peers, renter, &host, req).await;
        }
        let offer = self
            .offers
            .read()
            .await
            .iter()
            .find(|o| o.id == req.offer_id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("offer {}", req.offer_id)))?;
        if req.spec.vcpus > offer.vcpus || req.spec.mem_mib > offer.mem_mib || req.spec.disk_gib > offer.disk_gib {
            return Err(Error::Capacity("spec exceeds the offer".into()));
        }
        let needed = offer.price_per_sec * self.config.min_runway_secs;
        let available = self.ledger.balance(renter).await?;
        if available < needed {
            return Err(Error::InsufficientFunds { needed, available });
        }

        let vm = self.provisioner.provision(req.spec.clone()).await?;
        let vip = {
            let mut n = self.next_vip.lock().await;
            *n += 1;
            Ipv4Addr::from(u32::from(OVERLAY_NET) + *n)
        };
        self.routes.insert(
            vip,
            Route {
                peer_id: offer.provider.clone(),
                endpoint: "127.0.0.1:51820".parse().map_err(|_| Error::Invalid("endpoint".into()))?,
                backup: None,
            },
        );
        let instance = Instance {
            id: format!("inst-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            vm: vm.id,
            renter: renter.to_owned(),
            offer_id: offer.id.clone(),
            spec: req.spec,
            state: InstanceState::Running,
            virtual_ip: vip,
            price_per_sec: offer.price_per_sec,
            created_at: now_secs(),
            host: None,
            billing_segment: 0,
        };
        self.ensure_billable(&offer.provider).await?;
        self.meter.start(&instance.billing_id(), renter, &offer.provider, offer.price_per_sec, now_secs()).await?;
        self.instances.lock().await.insert(instance.id.clone(), instance.clone());
        Ok(instance)
    }

    async fn ensure_billable(&self, provider: &str) -> Result<()> {
        self.ledger.open_account(provider, AccountKind::Provider).await
    }

    pub async fn instance(&self, renter: &str, id: &str) -> Result<Instance> {
        self.instances
            .lock()
            .await
            .get(id)
            .filter(|i| i.renter == renter)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("instance {id}")))
    }

    pub async fn instances(&self, renter: &str) -> Vec<Instance> {
        let mut v: Vec<Instance> =
            self.instances.lock().await.values().filter(|i| i.renter == renter).cloned().collect();
        v.sort_by_key(|i| std::cmp::Reverse(i.created_at));
        v
    }

    /// Scale to zero (hibernate + stop billing) or back to one.
    pub async fn scale(&self, renter: &str, id: &str, replicas: u32) -> Result<Instance> {
        let inst = self.instance(renter, id).await?;
        if let Some((peers, host)) = self.remote(&inst)? {
            let resuming = replicas == 1 && inst.state == InstanceState::ScaledToZero;
            if resuming {
                peers.fund(&host, renter, self.host_rate(&host).await + inst.price_per_sec).await?;
            }
            let theirs = peers.instance_call(&host, PeerRequest::Scale { id: id.to_owned(), replicas }).await?;
            let updated = self.keep_remote(&inst, theirs).await;
            if updated.state != InstanceState::Running {
                self.settle_host(peers, &host, renter).await;
            }
            return Ok(updated);
        }
        let provider = self
            .offers
            .read()
            .await
            .iter()
            .find(|o| o.id == inst.offer_id)
            .map(|o| o.provider.clone())
            .unwrap_or_default();
        let updated = match (inst.state, replicas) {
            (InstanceState::Running, 0) => {
                self.provisioner.hibernate(inst.vm).await?;
                self.meter.stop(&inst.billing_id(), now_secs()).await?;
                Instance { state: InstanceState::ScaledToZero, ..inst }
            }
            (InstanceState::ScaledToZero, 1) => {
                self.provisioner.resume(inst.vm).await?;
                let next =
                    Instance { state: InstanceState::Running, billing_segment: inst.billing_segment + 1, ..inst };
                self.meter.start(&next.billing_id(), renter, &provider, next.price_per_sec, now_secs()).await?;
                next
            }
            (s, r) if (s == InstanceState::Running && r == 1) || (s == InstanceState::ScaledToZero && r == 0) => inst,
            (InstanceState::Terminated, _) => return Err(Error::Invalid("instance is terminated".into())),
            (_, r) => return Err(Error::Invalid(format!("replicas must be 0 or 1 (got {r})"))),
        };
        self.instances.lock().await.insert(updated.id.clone(), updated.clone());
        Ok(updated)
    }

    /// Terminate every instance before the node exits. Instances live in memory
    /// only, so a guest left running would be unreachable by the next run and,
    /// on a peer, billed with no one to stop it.
    pub async fn shutdown(&self) {
        let live: Vec<Instance> =
            self.instances.lock().await.values().filter(|i| i.state != InstanceState::Terminated).cloned().collect();
        for inst in live {
            if let Err(e) = self.terminate(&inst.renter, &inst.id).await {
                tracing::warn!(id = %inst.id, error = %e, "could not terminate on shutdown");
            }
        }
    }

    pub async fn terminate(&self, renter: &str, id: &str) -> Result<Instance> {
        let inst = self.instance(renter, id).await?;
        if let Some((peers, host)) = self.remote(&inst)? {
            let theirs = match peers.instance_call(&host, PeerRequest::Terminate { id: id.to_owned() }).await {
                Ok(theirs) => theirs,
                // The host already forgot it (e.g. it restarted): nothing left to stop.
                Err(Error::NotFound(_)) => Instance { state: InstanceState::Terminated, ..inst.clone() },
                Err(e) => return Err(e),
            };
            peers.stop_forwards(id).await;
            let done = self.keep_remote(&inst, theirs).await;
            self.settle_host(peers, &host, renter).await;
            return Ok(done);
        }
        if inst.state == InstanceState::Running {
            self.meter.stop(&inst.billing_id(), now_secs()).await?;
        }
        if inst.state != InstanceState::Terminated {
            self.provisioner.destroy(inst.vm).await?;
        }
        let done = Instance { state: InstanceState::Terminated, ..inst };
        self.instances.lock().await.insert(done.id.clone(), done.clone());
        Ok(done)
    }

    /// Tail of the instance's serial console (`None` when the backend does not capture it).
    pub async fn console(&self, renter: &str, id: &str, max_bytes: usize) -> Result<Option<String>> {
        let inst = self.instance(renter, id).await?;
        if inst.state == InstanceState::Terminated {
            return Ok(None);
        }
        if let Some((peers, host)) = self.remote(&inst)? {
            return peers.console(&host, id, max_bytes).await;
        }
        self.provisioner.hypervisor().console_tail(inst.vm, max_bytes).await
    }

    /// Type into the instance's serial console, here or on the peer hosting it.
    pub async fn console_input(&self, renter: &str, id: &str, data: &str) -> Result<()> {
        let inst = self.instance(renter, id).await?;
        if inst.state != InstanceState::Running {
            return Err(Error::Invalid(format!("instance {id} is not running")));
        }
        if let Some((peers, host)) = self.remote(&inst)? {
            return peers.console_input(&host, id, data).await;
        }
        self.provisioner.hypervisor().console_write(inst.vm, data.as_bytes()).await
    }

    /// How to log in to the instance (`None` when the backend gives guests no SSH endpoint).
    pub async fn access(&self, renter: &str, id: &str) -> Result<Option<GuestAccess>> {
        let inst = self.instance(renter, id).await?;
        if inst.state == InstanceState::Terminated {
            return Ok(None);
        }
        if let Some((peers, host)) = self.remote(&inst)? {
            return peers.access(&host, id).await;
        }
        self.provisioner.hypervisor().access(inst.vm).await
    }

    pub async fn account(&self, account: &str) -> Result<AccountSummary> {
        Ok(AccountSummary {
            account: account.to_owned(),
            balance: self.ledger.balance(account).await?,
            history: self.ledger.history(account, 50).await?,
        })
    }

    /// Instances whose VM is currently running (for metrics).
    pub async fn running_vms(&self) -> usize {
        self.provisioner.list().await.iter().filter(|v| v.state == VmState::Running).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deploy_scale_terminate_lifecycle() {
        let (node, key) = Node::demo().await.expect("demo");
        let renter = node.authenticate(&key).await.expect("auth");
        let spec = VmSpec {
            vcpus: 2,
            mem_mib: 4096,
            disk_gib: 20,
            image: "ubuntu-24.04".into(),
            accelerator: None,
            confidential: false,
        };
        let inst = node.deploy(&renter, DeployRequest { offer_id: "fra-cpu-1".into(), spec }).await.expect("deploy");
        assert_eq!(inst.state, InstanceState::Running);
        assert!(inst.virtual_ip.octets()[..2] == [10, 147]);

        let z = node.scale(&renter, &inst.id, 0).await.expect("to zero");
        assert_eq!(z.state, InstanceState::ScaledToZero);
        let up = node.scale(&renter, &inst.id, 1).await.expect("back up");
        assert_eq!(up.state, InstanceState::Running);
        let gone = node.terminate(&renter, &inst.id).await.expect("terminate");
        assert_eq!(gone.state, InstanceState::Terminated);
        assert!(node.instance("someone-else", &inst.id).await.is_err());
    }

    #[tokio::test]
    async fn shutdown_stops_every_vm_and_its_billing() {
        let (node, key) = Node::demo().await.expect("demo");
        let renter = node.authenticate(&key).await.expect("auth");
        let spec = VmSpec {
            vcpus: 1,
            mem_mib: 1024,
            disk_gib: 10,
            image: "ubuntu-24.04".into(),
            accelerator: None,
            confidential: false,
        };
        let running =
            node.deploy(&renter, DeployRequest { offer_id: "fra-cpu-1".into(), spec: spec.clone() }).await.expect("a");
        let parked = node.deploy(&renter, DeployRequest { offer_id: "fra-cpu-1".into(), spec }).await.expect("b");
        node.scale(&renter, &parked.id, 0).await.expect("to zero");

        node.shutdown(Duration::from_secs(5)).await;

        for id in [&running.id, &parked.id] {
            assert_eq!(node.instance(&renter, id).await.expect("kept").state, InstanceState::Terminated);
        }
        assert!(node.provisioner.list().await.is_empty(), "no VM left behind");
        let billing = running.billing_id();
        let state: String = node
            .store
            .with(move |c| Ok(c.query_row("SELECT state FROM instances WHERE id = ?1", [billing], |r| r.get(0))?))
            .expect("billing row");
        assert_eq!(state, "stopped");
    }
}
