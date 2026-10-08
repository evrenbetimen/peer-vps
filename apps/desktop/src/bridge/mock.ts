// Browser-only stand-in for the Rust side, so `pnpm dev` works without Tauri
// (UI iteration, screenshots, Playwright). Never used inside the desktop app.

import type {
  AccountSummary,
  Batch,
  HostAllocation,
  HostSnapshot,
  Instance,
  NodeEvent,
  Offer,
  OfferQuery,
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
let allocation: HostAllocation = { maxCores: 8, maxMemMib: 32768, maxDiskGib: 500, gpuEnabled: true, pricePerCoreSec: 150 };
const instances: Instance[] = [];
const wallet: AccountSummary = { account: "demo-agent", balance: 50 * C, history: [] };
const alive: Record<string, boolean> = { "host-b": true, "host-c": true };
let activePeer = "host-b";
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
        } satisfies HostSnapshot;
      case "set_host_allocation":
        allocation = args.allocation as HostAllocation;
        return allocation;
      case "list_offers": {
        const q = (args.query ?? {}) as OfferQuery;
        return offers.filter(
          (o) =>
            (q.minVramMib === undefined || o.vramMib >= q.minVramMib) &&
            (q.maxPricePerHour === undefined || o.pricePerSec * 3600 <= q.maxPricePerHour) &&
            (q.accelerator === undefined || o.accelerator === q.accelerator),
        );
      }
      case "deploy_instance": {
        const { offerId, spec } = args.request as { offerId: string; spec: VmSpec };
        const offer = offers.find((o) => o.id === offerId);
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
        };
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
      default:
        throw { code: "unsupported", message: `mock: unknown command ${cmd}` };
    }
  })();
  return out as T;
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
