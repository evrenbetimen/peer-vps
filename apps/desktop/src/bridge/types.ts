// TypeScript mirrors of the Rust types the bridge exchanges. Field names follow
// the `#[serde(rename_all = "camelCase")]` attributes on the Rust side.

/** µcredits: 1 credit = 1_000_000. Always integers. */
export type Micros = number;

export type AcceleratorKind = "none" | "gpu" | "npu";
export type VmState = "created" | "running" | "paused" | "hibernated" | "stopped";
export type InstanceState = "running" | "scaledToZero" | "terminated";

export interface HostMetrics {
  cpuLoadPct: number;
  cpuTempC: number;
  memUsedMib: number;
  memTotalMib: number;
  runningVms: number;
  slaPct: number;
  netRxBps: number;
  netTxBps: number;
}

export interface AcceleratorRequest {
  kind: AcceleratorKind;
  slices: number;
  minVramMib?: number;
}

export interface VmSpec {
  vcpus: number;
  memMib: number;
  diskGib: number;
  image: string;
  accelerator?: AcceleratorRequest | null;
  confidential?: boolean;
}

export interface AcceleratorPartition {
  deviceId: string;
  kind: AcceleratorKind;
  slices: number;
  vramMib: number;
  vfioGroup: string | null;
}

export interface Placement {
  pinnedCores: number[];
  memMib: number;
  diskGib: number;
  accelerator: AcceleratorPartition | null;
}

export interface VmRecord {
  id: string;
  spec: VmSpec;
  placement: Placement;
  state: VmState;
}

export interface Offer {
  id: string;
  provider: string;
  region: string;
  vcpus: number;
  memMib: number;
  diskGib: number;
  accelerator: AcceleratorKind;
  acceleratorModel: string | null;
  vramMib: number;
  pricePerSec: Micros;
  slaPct: number;
  confidential: boolean;
  collateralLocked: Micros;
}

export interface OfferQuery {
  minVramMib?: number;
  maxPricePerHour?: Micros;
  minSlaPct?: number;
  accelerator?: AcceleratorKind;
  confidential?: boolean;
  minVcpus?: number;
  minMemMib?: number;
  sort?: "price" | "vram" | "sla";
  limit?: number;
}

export interface Instance {
  id: string;
  vm: string;
  renter: string;
  offerId: string;
  spec: VmSpec;
  state: InstanceState;
  virtualIp: string;
  pricePerSec: Micros;
  createdAt: number;
  /** Peer id of the machine running it, when rented from another PeerVPS. */
  host?: string;
}

export interface LedgerEntry {
  id: number;
  account: string;
  delta: Micros;
  balanceAfter: Micros;
  kind: string;
  reference: string | null;
  at: number;
}

export interface AccountSummary {
  account: string;
  balance: Micros;
  history: LedgerEntry[];
}

export interface CollateralState {
  provider: string;
  locked: Micros;
  minimum: Micros;
  eligible: boolean;
}

export interface HostAllocation {
  maxCores: number;
  maxMemMib: number;
  maxDiskGib: number;
  gpuEnabled: boolean;
  pricePerCoreSec: Micros;
}

export interface HostSnapshot {
  allocation: HostAllocation;
  hostCores: number;
  hostMemMib: number;
  freeCores: number;
  freeMemMib: number;
  freeDiskGib: number;
  vms: VmRecord[];
  collateral: CollateralState;
  earnings: Micros;
  hypervisor: string;
  /** Why guests are simulated (QEMU missing); null when real VMs run. */
  hypervisorNote: string | null;
}

export interface GuestAccess {
  sshHost: string;
  sshPort: number;
  /** Empty when the login is chosen during an OS installation. */
  user: string;
  password: string | null;
  /** Windows guest: sign in over Remote Desktop, there is no SSH server by default. */
  windows?: boolean;
  /** Remote Desktop endpoint, `host:port`. */
  rdp?: string;
  /** The guest's screen, `vnc://host:port`, for guests installed from an ISO. */
  display?: string;
  displayPassword?: string;
}

export interface CatalogImage {
  name: string;
  title: string;
}

export interface IsoInfo {
  label: string;
  windows: boolean;
  arch: "x86_64" | "aarch64" | null;
}

export interface LocalImage {
  name: string;
  sizeBytes: number;
  kind: "disk" | "iso";
  iso?: IsoInfo;
}

export interface Download {
  done: number;
  total: number | null;
  error: string | null;
  /** A local file being copied in rather than downloaded. */
  import?: boolean;
}

export interface Images {
  dir: string;
  installed: LocalImage[];
  catalog: CatalogImage[];
  downloads: Record<string, Download>;
}

export type FailoverPhase = "healthy" | "suspect" | "down" | "rerouted" | "stranded" | "recovered";

export interface FailoverTransition {
  peer: string;
  phase: FailoverPhase;
  missedHeartbeats: number;
  movedVips: string[];
  atMs: number;
}

export interface TopologyNode {
  id: string;
  label: string;
  role: "client" | "primary" | "standby";
  endpoint: string;
  alive: boolean;
}

export interface Topology {
  nodes: TopologyNode[];
  virtualIp: string;
  activePeer: string | null;
  heartbeatMs: number;
}

export type WebhookOutcome = { credited: { balance: Micros } } | "duplicate";

/** Discrete events (serde internally tagged on `type`). */
export type NodeEvent =
  | ({ type: "vmState" } & { vm: string; state: VmState })
  | ({ type: "routeChanged" } & { virtualIp: string; peer: string; endpoint: string })
  | ({ type: "failover" } & FailoverTransition)
  | ({ type: "slashed" } & { provider: string; renter: string; amount: Micros; reason: string });

/** One frame's worth of node updates, pushed at ≤ 60 Hz. */
export interface Batch {
  metrics: HostMetrics | null;
  balances: Record<string, Micros>;
  events: NodeEvent[];
  dropped: number;
}

export type PeerStatus = "online" | "waitingForApproval" | "unreachable" | "pending" | "inbound";

export interface PeerInfo {
  id: string;
  publicKey: string;
  address: string | null;
  trusted: boolean;
  status: PeerStatus;
  offers: Offer[];
  lastSeen: number | null;
  error: string | null;
}

export type InternetState = "off" | "checking" | "open" | "noGateway" | "carrierNat" | "failed";

export interface InternetStatus {
  state: InternetState;
  /** `ip:port` other networks reach this machine on, when open. */
  address: string | null;
  detail: string | null;
}

export type RelayState = "off" | "connecting" | "connected" | "retrying";

export interface RelayStatus {
  state: RelayState;
  /** `host:port` of the relay. */
  address: string | null;
  detail: string | null;
}

export interface NearbyPeer {
  id: string;
  /** `pv-…@ip:port`, ready to add. */
  invite: string;
  lastSeen: number;
}

export interface PeerOverview {
  /** This machine's peer id. */
  id: string;
  listen: string | null;
  /** What another machine on this network pastes to add this one: `pv-…@host:port`. */
  invite: string | null;
  /** The same for other networks, when the router forwards a port. */
  internetInvite: string | null;
  internet: InternetStatus;
  /** The same through a relay (`pv-…@relay://host:port`), when registered there. */
  relayInvite: string | null;
  relay: RelayStatus;
  /** PeerVPS machines announcing themselves on this network that are not peers yet. */
  nearby: NearbyPeer[];
  peers: PeerInfo[];
}

export interface CmdError {
  code:
    | "not_found"
    | "insufficient_capacity"
    | "insufficient_funds"
    | "invalid_argument"
    | "unauthorized"
    | "unsupported"
    | "hypervisor_error"
    | "peer_unavailable"
    | "internal";
  message: string;
}
