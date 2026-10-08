import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Batch, Instance, Offer, PeerInfo, PeerOverview, Topology } from "./types";

// The mock keeps module-level state, so every test gets a fresh copy.
async function freshMock() {
  vi.resetModules();
  return import("./mock");
}

const spec = { vcpus: 2, memMib: 4096, diskGib: 20, image: "ubuntu-24.04", confidential: false };

describe("browser mock bridge", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  async function call<T>(m: Awaited<ReturnType<typeof freshMock>>, cmd: string, args?: Record<string, unknown>) {
    const p = m.mockInvoke<T>(cmd, args);
    p.catch(() => {}); // observed below; keeps the pending rejection from being reported as unhandled
    await vi.advanceTimersByTimeAsync(100);
    return p;
  }

  it("filters offers by VRAM, price and accelerator", async () => {
    const m = await freshMock();
    const all = await call<Offer[]>(m, "list_offers", { query: {} });
    expect(all.length).toBeGreaterThan(1);
    const gpu = await call<Offer[]>(m, "list_offers", { query: { accelerator: "gpu", minVramMib: 24 * 1024 } });
    expect(gpu.map((o) => o.id).sort()).toEqual(["iad-h100-3", "this-machine"]);
    const cheap = await call<Offer[]>(m, "list_offers", { query: { maxPricePerHour: 5_000_000 } });
    expect(cheap.every((o) => o.pricePerSec * 3600 <= 5_000_000)).toBe(true);
  });

  it("runs the instance lifecycle", async () => {
    const m = await freshMock();
    const inst = await call<Instance>(m, "deploy_instance", { request: { offerId: "fra-cpu-1", spec } });
    expect(inst.state).toBe("running");
    expect(inst.virtualIp).toMatch(/^10\.147\.0\.\d+$/);
    expect(await call<Instance[]>(m, "list_instances")).toHaveLength(1);
    expect((await call<Instance>(m, "scale_instance", { id: inst.id, replicas: 0 })).state).toBe("scaledToZero");
    expect((await call<Instance>(m, "scale_instance", { id: inst.id, replicas: 1 })).state).toBe("running");
    expect((await call<Instance>(m, "terminate_instance", { id: inst.id })).state).toBe("terminated");
  });

  it("rejects unknown offers, instances and commands with typed errors", async () => {
    const m = await freshMock();
    await expect(call(m, "deploy_instance", { request: { offerId: "nope", spec } })).rejects.toMatchObject({ code: "not_found" });
    await expect(call(m, "scale_instance", { id: "nope", replicas: 0 })).rejects.toMatchObject({ code: "not_found" });
    await expect(call(m, "format_disk")).rejects.toMatchObject({ code: "unsupported" });
  });

  it("credits top-ups and journals them", async () => {
    const m = await freshMock();
    const before = (await call<{ balance: number }>(m, "get_wallet")).balance;
    await call(m, "top_up", { amount: 25_000_000 });
    const after = await call<{ balance: number; history: { kind: string; delta: number }[] }>(m, "get_wallet");
    expect(after.balance).toBe(before + 25_000_000);
    expect(after.history[0]).toMatchObject({ kind: "top_up", delta: 25_000_000 });
  });

  it("fails over to the standby after three missed heartbeats", async () => {
    const m = await freshMock();
    const batches: Batch[] = [];
    const stop = m.startMockPump((b) => batches.push(b));
    const topo = await call<Topology>(m, "kill_peer", { peer: "host-b" });
    expect(topo.nodes.find((n) => n.id === "host-b")?.alive).toBe(false);
    await vi.advanceTimersByTimeAsync(2000);
    const phases = batches.flatMap((b) => b.events).filter((e) => e.type === "failover").map((e) => e.phase);
    expect(phases).toEqual(["suspect", "suspect", "down", "rerouted"]);
    const route = batches.flatMap((b) => b.events).find((e) => e.type === "routeChanged");
    expect(route).toMatchObject({ virtualIp: "10.147.0.200", peer: "host-c" });
    expect((await call<Topology>(m, "get_topology")).activePeer).toBe("host-c");

    await call(m, "restore_peer", { peer: "host-b" });
    expect((await call<Topology>(m, "get_topology")).activePeer).toBe("host-b");
    stop();
  });

  it("killing the standby does not move the route", async () => {
    const m = await freshMock();
    const batches: Batch[] = [];
    const stop = m.startMockPump((b) => batches.push(b));
    await call(m, "kill_peer", { peer: "host-c" });
    await vi.advanceTimersByTimeAsync(2000);
    expect(batches.flatMap((b) => b.events).some((e) => e.type === "routeChanged")).toBe(false);
    expect((await call<Topology>(m, "get_topology")).activePeer).toBe("host-b");
    stop();
  });

  it("streams metrics at 4 Hz and bills running instances per tick", async () => {
    const m = await freshMock();
    await call(m, "deploy_instance", { request: { offerId: "fra-cpu-1", spec } });
    const start = (await call<{ balance: number }>(m, "get_wallet")).balance;
    const batches: Batch[] = [];
    const stop = m.startMockPump((b) => batches.push(b));
    await vi.advanceTimersByTimeAsync(1000);
    stop();
    const ticks = batches.filter((b) => b.metrics);
    expect(ticks).toHaveLength(4);
    expect(ticks.every((b) => b.metrics!.cpuLoadPct >= 0 && b.metrics!.cpuLoadPct <= 100)).toBe(true);
    // fra-cpu-1 costs 555 µcr/s → ~555 µcr over one second.
    const end = (await call<{ balance: number }>(m, "get_wallet")).balance;
    expect(start - end).toBeGreaterThanOrEqual(550);
    expect(start - end).toBeLessThanOrEqual(560);
  });

  it("exposes SSH access for live instances only", async () => {
    const m = await freshMock();
    const inst = await call<Instance>(m, "deploy_instance", { request: { offerId: "fra-cpu-1", spec } });
    const access = await call<{ sshPort: number; user: string } | null>(m, "get_instance_access", { id: inst.id });
    expect(access).toMatchObject({ user: "peervps" });
    await call(m, "terminate_instance", { id: inst.id });
    expect(await call(m, "get_instance_access", { id: inst.id })).toBeNull();
  });

  it("downloads catalog images with progress", async () => {
    const m = await freshMock();
    type Imgs = { installed: { name: string }[]; downloads: Record<string, { done: number; total: number }> };
    const before = await call<Imgs>(m, "list_images");
    expect(before.installed.map((i) => i.name)).toEqual(["ubuntu-24.04"]);
    await call(m, "pull_image", { name: "debian-13" });
    await vi.advanceTimersByTimeAsync(200);
    const mid = await call<Imgs>(m, "list_images");
    expect(mid.downloads["debian-13"]!.done).toBeGreaterThan(0);
    await vi.advanceTimersByTimeAsync(3000);
    const after = await call<Imgs>(m, "list_images");
    expect(after.installed.map((i) => i.name)).toContain("debian-13");
    expect(after.downloads).toEqual({});
    await expect(call(m, "pull_image", { name: "windows-11" })).rejects.toMatchObject({ code: "not_found" });
  });

  it("imports a Windows ISO and gives its guests a screen and Remote Desktop", async () => {
    const m = await freshMock();
    type Imgs = { installed: { name: string; kind: string; iso?: { windows: boolean } }[]; downloads: Record<string, { import?: boolean }> };
    const name = await call<string>(m, "import_image");
    expect((await call<Imgs>(m, "list_images")).downloads[name]).toMatchObject({ import: true });
    await vi.advanceTimersByTimeAsync(1500);
    const iso = (await call<Imgs>(m, "list_images")).installed.find((i) => i.name === name);
    expect(iso).toMatchObject({ kind: "iso", iso: { windows: true } });

    const inst = await call<Instance>(m, "deploy_instance", { request: { offerId: "fra-cpu-1", spec: { ...spec, image: name } } });
    const access = await call<Record<string, unknown>>(m, "get_instance_access", { id: inst.id });
    expect(access).toMatchObject({ windows: true, user: "peervps", display: "vnc://127.0.0.1:5900" });
    expect(access.rdp).toMatch(/^127\.0\.0\.1:\d+$/);
    await expect(call(m, "open_guest_screen", { id: inst.id })).resolves.toBeNull();
  });

  it("adds, approves and removes peers and lists their offers", async () => {
    const m = await freshMock();
    const view = await call<PeerOverview>(m, "get_peers");
    expect(view.invite).toBe(`${view.id}@192.168.1.10:7071`);
    const added = await call<PeerInfo>(m, "add_peer", { address: "192.168.1.50" });
    expect(added).toMatchObject({ address: "192.168.1.50:7071", trusted: true, status: "waitingForApproval" });
    expect(await call<PeerInfo>(m, "add_peer", { address: "192.168.1.50" })).toMatchObject({ id: added.id });
    await expect(call(m, "add_peer", { address: " " })).rejects.toMatchObject({ code: "invalid_argument" });
    await expect(call(m, "add_peer", { address: "127.0.0.1:1" })).rejects.toMatchObject({ code: "peer_unavailable" });

    expect((await call<Offer[]>(m, "list_offers", { query: {} })).filter((o) => o.id.includes("/"))).toHaveLength(1);
    expect(await call<PeerInfo>(m, "approve_peer", { id: "pv-a17b0c55e9d24f13" })).toMatchObject({ trusted: true, status: "online" });
    const remote = (await call<Offer[]>(m, "list_offers", { query: {} })).filter((o) => o.id.includes("/"));
    expect(remote.map((o) => o.provider).sort()).toEqual(["pv-3f9c1a7e2b4d6c80", "pv-a17b0c55e9d24f13"]);
    const [first] = remote;
    if (!first) throw new Error("no peer offer");
    const inst = await call<Instance>(m, "deploy_instance", { request: { offerId: first.id, spec } });
    expect(inst.host).toBe(first.provider);

    await call(m, "remove_peer", { id: added.id });
    await expect(call(m, "remove_peer", { id: added.id })).rejects.toMatchObject({ code: "not_found" });
    await expect(call(m, "approve_peer", { id: "pv-nope" })).rejects.toMatchObject({ code: "not_found" });
    expect((await call<PeerOverview>(m, "get_peers")).peers.map((p) => p.id)).not.toContain(added.id);
  });
});
