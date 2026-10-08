import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Batch, Instance, Offer, Topology } from "./types";

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
});
