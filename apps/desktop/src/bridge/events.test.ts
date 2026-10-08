import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

async function freshBridge() {
  vi.resetModules();
  return import("./events");
}

describe("live event store", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("feeds metrics into capped history and renders at most once per frame", async () => {
    const ev = await freshBridge();
    let renders = 0;
    const { result } = renderHook(() => {
      renders += 1;
      return ev.useLive((s) => s.cpuHistory);
    });
    const stop = await ev.startEventBridge();
    // 300 ticks of 250 ms = 75 s of samples; history keeps the last 60 s.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(300 * 250);
    });
    expect(result.current).toHaveLength(240);
    expect(renders).toBeLessThanOrEqual(1 + 300 + 1);
    stop();
  });

  it("starts only once", async () => {
    const ev = await freshBridge();
    const a = await ev.startEventBridge();
    const b = await ev.startEventBridge();
    expect(b).not.toBe(a);
    b(); // the second call is a no-op unlisten
    a();
  });

  it("tracks balances, routes and failovers from mock events", async () => {
    const ev = await freshBridge();
    const { commands } = await import("./commands");
    const { result } = renderHook(() => ({
      routes: ev.useLive((s) => s.routes),
      failovers: ev.useLive((s) => s.failovers),
      balance: ev.useLive((s) => s.balances["demo-agent"]),
    }));
    const stop = await ev.startEventBridge();
    await act(async () => {
      const p = commands.topUp(5_000_000);
      await vi.advanceTimersByTimeAsync(100);
      await p;
      const k = commands.killPeer("host-b");
      await vi.advanceTimersByTimeAsync(2000);
      await k;
    });
    expect(result.current.balance).toBeGreaterThan(50_000_000);
    expect(result.current.routes["10.147.0.200"]?.peer).toBe("host-c");
    expect(result.current.failovers.map((f) => f.phase)).toContain("rerouted");
    stop();
  });
});
