// Browser-only stand-in for the Rust side, so `pnpm dev` works without Tauri
// (UI iteration, screenshots, Playwright). Never used inside the desktop app.

import type {
  AccountSummary,
  Batch,
  HostAllocation,
  HostSnapshot,
  Images,
  Instance,
  InternetStatus,
  NearbyPeer,
  RelayStatus,
  NodeEvent,
  Offer,
  OfferQuery,
  PeerInfo,
  PeerOverview,
  Topology,
  VmSpec,
} from "./types";

const C = 1_000_000;
const offers: Offer[] = [
  { id: "this-machine", provider: "local", region: "local", vcpus: 8, memMib: 32768, diskGib: 500, accelerator: "gpu", acceleratorModel: "Simulated GPU (¼ slices)", vramMib: 24576, pricePerSec: 1200, slaPct: 100, confidential: false, collateralLocked: 150 * C },
  { id: "fra-cpu-1", provider: "node-fra-cpu-1", region: "eu-central", vcpus: 4, memMib: 4096, diskGib: 200, accelerator: "none", acceleratorModel: null, vramMib: 0, pricePerSec: 555, slaPct: 99.95, confidential: false, collateralLocked: 500 * C },
  { id: "ams-4090-2", provider: "node-ams-4090-2", region: "eu-west", vcpus: 8, memMib: 32768, diskGib: 200, accelerator: "gpu", acceleratorModel: "RTX 4090 (½)", vramMib: 12288, pricePerSec: 5000, slaPct: 99.7, confidential: false, collateralLocked: 500 * C },
  { id: "iad-h100-3", provider: "node-iad-h100-3", region: "us-east", vcpus: 16, memMib: 131072, diskGib: 200, accelerator: "gpu", acceleratorModel: "H100 MIG 3g.40gb", vramMib: 40960, pricePerSec: 26388, slaPct: 99.9, confidential: true, collateralLocked: 500 * C },
];
const ME = "pv-5c0ffee15ea1ab1e";
/** A peer's own "this-machine" offer, as it lists it to us. */
const remoteOffer = (): Offer => ({ id: "this-machine", provider: "local", region: "local", vcpus: 4, memMib: 16384, diskGib: 200, accelerator: "none", acceleratorModel: null, vramMib: 0, pricePerSec: 600, slaPct: 100, confidential: false, collateralLocked: 150 * C });
const peers: PeerInfo[] = [
  { id: "pv-3f9c1a7e2b4d6c80", publicKey: "3f9c1a7e2b4d6c80".padEnd(64, "0"), address: "192.168.1.20:7071", trusted: true, status: "online", offers: [remoteOffer()], lastSeen: Math.floor(Date.now() / 1000), error: null, flows: { paid: 0, earned: 3 * C } },
  { id: "pv-a17b0c55e9d24f13", publicKey: "a17b0c55e9d24f13".padEnd(64, "0"), address: "192.168.1.31:7071", trusted: false, status: "pending", offers: [], lastSeen: Math.floor(Date.now() / 1000), error: null, flows: { paid: 0, earned: 0 } },
];
let internet: InternetStatus = { state: "off", address: null, detail: null };
let relay: RelayStatus = { state: "off", address: null, detail: null };
const nearby: NearbyPeer[] = [{ id: "pv-b2e4f6a8c0d1e3f5", invite: "pv-b2e4f6a8c0d1e3f5@192.168.1.44:7071", lastSeen: Math.floor(Date.now() / 1000) }];
/** Peers' offers as the node lists them: `<peer>/<offer>`, provided by the peer. */
function peerOffers(): Offer[] {
  return peers.filter((p) => p.status === "online").flatMap((p) => p.offers.map((o) => ({ ...o, id: `${p.id}/${o.id}`, provider: p.id, region: `${o.region} via ${p.id}` })));
}
let allocation: HostAllocation = { maxCores: 8, maxMemMib: 32768, maxDiskGib: 500, gpuEnabled: true, pricePerCoreSec: 150 };
const instances: Instance[] = [];
const wallet: AccountSummary = { account: "demo-agent", balance: 50 * C, history: [] };
const alive: Record<string, boolean> = { "host-b": true, "host-c": true };
let activePeer = "host-b";
const catalog = [
  { name: "ubuntu-24.04", title: "Ubuntu 24.04 LTS" },
  { name: "ubuntu-22.04", title: "Ubuntu 22.04 LTS" },
  { name: "debian-13", title: "Debian 13" },
];
const installed: Images["installed"] = [{ name: "ubuntu-24.04", sizeBytes: 625_612_288, kind: "disk" }];
/** What the mock file picker "chooses". */
const MOCK_ISO = { name: "win11_24h2_english_arm64", sizeBytes: 5_800_000_000, kind: "iso" as const, iso: { label: "CPBA_A64FRE_EN-US_DV9", windows: true, arch: "aarch64" as const } };
const downloads: Images["downloads"] = {};
const MOCK_IMAGE_BYTES = 400_000_000;

function simulateDownload(name: string) {
  downloads[name] = { done: 0, total: MOCK_IMAGE_BYTES, error: null };
  const id = setInterval(() => {
    const d = downloads[name];
    if (!d) return clearInterval(id);
    d.done = Math.min(MOCK_IMAGE_BYTES, d.done + MOCK_IMAGE_BYTES / 8);
    if (d.done >= MOCK_IMAGE_BYTES) {
      clearInterval(id);
      delete downloads[name];
      installed.push({ name, sizeBytes: MOCK_IMAGE_BYTES, kind: "disk" });
    }
  }, 250);
}
let emit: ((b: Batch) => void) | null = null;

function event(e: NodeEvent) {
  emit?.({ metrics: null, balances: {}, events: [e], dropped: 0 });
}

function topology(): Topology {
  return {
    nodes: [
      { id: "client", label: "You (client)", role: "client", endpoint: "nat:symmetric", alive: true },
      { id: "host-b", label: "Frankfurt", role: "primary", endpoint: "198.51.100.2:51820", alive: !!alive["host-b"] },
      { id: "host-c", label: "Amsterdam", role: "standby", endpoint: "198.51.100.3:51820", alive: !!alive["host-c"] },
    ],
    virtualIp: "10.147.0.200",
    activePeer,
    heartbeatMs: 500,
  };
}

function simulateFailure(peer: string) {
  const phases = ["suspect", "suspect", "down"] as const;
  phases.forEach((phase, i) =>
    setTimeout(() => {
      if (alive[peer]) return;
      event({ type: "failover", peer, phase, missedHeartbeats: i + 1, movedVips: [], atMs: Date.now() });
      if (phase === "down" && peer === activePeer) {
        activePeer = peer === "host-b" ? "host-c" : "host-b";
        event({ type: "routeChanged", virtualIp: "10.147.0.200", peer: activePeer, endpoint: "198.51.100.3:51820" });
        event({ type: "failover", peer, phase: "rerouted", missedHeartbeats: 3, movedVips: ["10.147.0.200"], atMs: Date.now() });
      }
    }, (i + 1) * 500),
  );
}

export async function mockInvoke<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  await new Promise((r) => setTimeout(r, 60));
  const out = ((): unknown => {
    switch (cmd) {
      case "get_host_snapshot":
        return {
          allocation,
          hostCores: navigator.hardwareConcurrency || 8,
          hostMemMib: 32768,
          freeCores: allocation.maxCores,
          freeMemMib: allocation.maxMemMib,
          freeDiskGib: allocation.maxDiskGib,
          vms: [],
          collateral: { provider: "local", locked: 150 * C, minimum: 100 * C, eligible: true },
          earnings: 100 * C,
          hypervisor: "mock (browser)",
          hypervisorNote: null,
        } satisfies HostSnapshot;
      case "set_host_allocation":
        allocation = args.allocation as HostAllocation;
        return allocation;
      case "list_offers": {
        const q = (args.query ?? {}) as OfferQuery;
        return [...offers, ...peerOffers()].filter(
          (o) =>
            (q.minVramMib === undefined || o.vramMib >= q.minVramMib) &&
            (q.maxPricePerHour === undefined || o.pricePerSec * 3600 <= q.maxPricePerHour) &&
            (q.accelerator === undefined || o.accelerator === q.accelerator),
        );
      }
      case "deploy_instance": {
        const { offerId, spec } = args.request as { offerId: string; spec: VmSpec };
        const offer = [...offers, ...peerOffers()].find((o) => o.id === offerId);
        if (!offer) throw { code: "not_found", message: `offer ${offerId}` };
        const inst: Instance = {
          id: `inst-${Math.random().toString(16).slice(2, 14)}`,
          vm: crypto.randomUUID(),
          renter: "demo-agent",
          offerId,
          spec,
          state: "running",
          virtualIp: `10.147.0.${11 + instances.length}`,
          pricePerSec: offer.pricePerSec,
          createdAt: Math.floor(Date.now() / 1000),
          ...(offerId.includes("/") ? { host: offer.provider } : {}),
        };
        const host = peers.find((p) => p.id === inst.host);
        if (host) {
          // Like the node: prepay ten minutes on the host out of the wallet.
          const prepay = Math.min(offer.pricePerSec * 600, wallet.balance);
          if (prepay <= 0) throw { code: "insufficient_funds", message: "not enough credits to prepay the host" };
          wallet.balance -= prepay;
          host.flows.paid += prepay;
          wallet.history.unshift({ id: Date.now(), account: "demo-agent", delta: -prepay, balanceAfter: wallet.balance, kind: "peer_payment", reference: host.id, at: Math.floor(Date.now() / 1000) });
        }
        instances.unshift(inst);
        return inst;
      }
      case "list_instances":
        return instances;
      case "scale_instance":
      case "terminate_instance": {
        const inst = instances.find((i) => i.id === args.id);
        if (!inst) throw { code: "not_found", message: `instance ${String(args.id)}` };
        inst.state = cmd === "terminate_instance" ? "terminated" : args.replicas === 0 ? "scaledToZero" : "running";
        return inst;
      }
      case "get_wallet":
        return wallet;
      case "top_up": {
        wallet.balance += args.amount as number;
        wallet.history.unshift({ id: Date.now(), account: "demo-agent", delta: args.amount as number, balanceAfter: wallet.balance, kind: "top_up", reference: "mock", at: Math.floor(Date.now() / 1000) });
        emit?.({ metrics: null, balances: { "demo-agent": wallet.balance }, events: [], dropped: 0 });
        return { credited: { balance: wallet.balance } };
      }
      case "get_topology":
        return topology();
      case "kill_peer":
        alive[args.peer as string] = false;
        simulateFailure(args.peer as string);
        return topology();
      case "restore_peer":
        alive[args.peer as string] = true;
        event({ type: "failover", peer: args.peer as string, phase: "recovered", missedHeartbeats: 0, movedVips: [], atMs: Date.now() });
        if (alive["host-b"] && alive["host-c"]) activePeer = "host-b";
        return topology();
      case "get_instance_access": {
        const inst = instances.find((i) => i.id === args.id);
        if (!inst) throw { code: "not_found", message: `instance ${String(args.id)}` };
        if (inst.state === "terminated") return null;
        const port = 2200 + instances.indexOf(inst);
        const image = installed.find((i) => i.name === inst.spec.image);
        if (image?.iso?.windows) {
          return { sshHost: "127.0.0.1", sshPort: port, user: "peervps", password: "mock-password", windows: true, rdp: `127.0.0.1:${port + 1000}`, display: "vnc://127.0.0.1:5900", displayPassword: "mock-pas" };
        }
        return { sshHost: "127.0.0.1", sshPort: port, user: "peervps", password: "mock-password" };
      }
      case "open_guest_screen":
        return null;
      case "import_image": {
        if (!downloads[MOCK_ISO.name] && !installed.some((i) => i.name === MOCK_ISO.name)) {
          downloads[MOCK_ISO.name] = { done: 0, total: MOCK_ISO.sizeBytes, error: null, import: true };
          setTimeout(() => {
            delete downloads[MOCK_ISO.name];
            installed.push(structuredClone(MOCK_ISO));
          }, 1000);
        }
        return MOCK_ISO.name;
      }
      case "get_console": {
        const inst = instances.find((i) => i.id === args.id);
        if (!inst) throw { code: "not_found", message: `instance ${String(args.id)}` };
        return null;
      }
      case "get_peers":
        return {
          id: ME,
          listen: "0.0.0.0:7071",
          invite: `${ME}@192.168.1.10:7071`,
          internetInvite: internet.address ? `${ME}@${internet.address}` : null,
          internet,
          relayInvite: relay.state === "connected" && relay.address ? `${ME}@relay://${relay.address}` : null,
          relay,
          nearby: nearby.filter((n) => !peers.some((p) => p.id === n.id)),
          peers,
        } satisfies PeerOverview;
      case "set_relay": {
        const address = typeof args.address === "string" && args.address.trim() ? args.address.trim() : null;
        if (!address) {
          relay = { state: "off", address: null, detail: null };
        } else {
          const full = /:\d+$/.test(address) ? address : `${address}:7073`;
          relay = { state: "connecting", address: full, detail: null };
          setTimeout(() => {
            if (relay.address === full) relay = { ...relay, state: full.startsWith("down.") ? "retrying" : "connected", detail: full.startsWith("down.") ? `relay: cannot reach ${full}` : null };
          }, 300);
        }
        return relay;
      }
      case "set_internet":
        if (!args.enabled) {
          internet = { state: "off", address: null, detail: null };
        } else {
          internet = { state: "checking", address: null, detail: null };
          setTimeout(() => {
            if (internet.state === "checking") internet = { state: "open", address: "203.0.113.7:7071", detail: "the router forwards TCP 7071 to 192.168.1.10:7071" };
          }, 300);
        }
        return internet;
      case "add_peer": {
        const address = String(args.address ?? "").trim();
        const [want, addr] = address.includes("@") ? address.split("@", 2) : [null, address];
        if (!addr) throw { code: "invalid_argument", message: "give the peer's address, e.g. 192.168.1.20:7071" };
        if (addr.startsWith("127.0.0.1:1")) throw { code: "peer_unavailable", message: `cannot reach ${addr}: connection refused` };
        const id = want ?? `pv-${Array.from(addr).reduce((h, c) => (h * 31 + c.charCodeAt(0)) >>> 0, 7).toString(16).padStart(16, "0").slice(0, 16)}`;
        let peer = peers.find((p) => p.id === id);
        if (!peer) {
          peer = { id, publicKey: id.slice(3).padEnd(64, "0"), address: addr.includes(":") ? addr : `${addr}:7071`, trusted: true, status: "waitingForApproval", offers: [], lastSeen: Math.floor(Date.now() / 1000), error: `waiting for the owner of ${id} to approve ${ME}`, flows: { paid: 0, earned: 0 } };
          peers.push(peer);
        }
        return peer;
      }
      case "approve_peer": {
        const peer = peers.find((p) => p.id === args.id);
        if (!peer) throw { code: "not_found", message: `peer ${String(args.id)}` };
        Object.assign(peer, { trusted: true, status: "online", offers: [remoteOffer()], error: null });
        return peer;
      }
      case "remove_peer": {
        const i = peers.findIndex((p) => p.id === args.id);
        if (i < 0) throw { code: "not_found", message: `peer ${String(args.id)}` };
        peers.splice(i, 1);
        return null;
      }
      case "list_images":
        return { dir: "~/PeerVPS/images", installed, catalog, downloads } satisfies Images;
      case "pull_image": {
        const name = args.name as string;
        if (!catalog.some((c) => c.name === name)) throw { code: "not_found", message: `no catalog image ${name}` };
        if (!downloads[name] && !installed.some((i) => i.name === name)) simulateDownload(name);
        return null;
      }
      default:
        throw { code: "unsupported", message: `mock: unknown command ${cmd}` };
    }
  })();
  // Real IPC hands back freshly deserialized objects; never leak shared mutable state to React.
  return structuredClone(out) as T;
}

export function startMockPump(sink: (b: Batch) => void): () => void {
  emit = sink;
  let t = 0;
  const id = setInterval(() => {
    t += 1;
    const running = instances.filter((i) => i.state === "running");
    const burn = running.reduce((s, i) => s + i.pricePerSec / 4, 0);
    wallet.balance = Math.max(0, Math.round(wallet.balance - burn));
    sink({
      metrics: {
        cpuLoadPct: 22 + 14 * Math.sin(t / 9) + Math.random() * 6,
        cpuTempC: 54 + 6 * Math.sin(t / 15) + Math.random(),
        memUsedMib: 9800 + running.length * 4096,
        memTotalMib: 32768,
        runningVms: running.length,
        slaPct: 99.97,
        netRxBps: 2e6 + Math.random() * 3e6,
        netTxBps: 1e6 + Math.random() * 2e6,
      },
      balances: burn > 0 ? { "demo-agent": wallet.balance } : {},
      events: [],
      dropped: 0,
    });
  }, 250);
  return () => {
    clearInterval(id);
    emit = null;
  };
}
