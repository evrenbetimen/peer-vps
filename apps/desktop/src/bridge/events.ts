// Live node state fed by the Rust event pump.
//
// Rust already throttles to one `node://batch` per frame; on top of that we
// apply incoming batches inside requestAnimationFrame, so React re-renders at
// most once per painted frame no matter how bursty IPC delivery is.

import { listen } from "@tauri-apps/api/event";
import { useSyncExternalStore } from "react";

import { inTauri } from "./commands";
import { startMockPump } from "./mock";
import type { Batch, FailoverTransition, HostMetrics, Micros, NodeEvent } from "./types";

export const BATCH_EVENT = "node://batch";
const HISTORY = 240; // 60 s of 250 ms samples
const LOG = 60;

export interface LiveState {
  metrics: HostMetrics | null;
  cpuHistory: number[];
  netHistory: number[];
  balances: Record<string, Micros>;
  failovers: FailoverTransition[];
  routes: Record<string, { peer: string; endpoint: string }>;
  log: NodeEvent[];
  dropped: number;
}

let state: LiveState = {
  metrics: null,
  cpuHistory: [],
  netHistory: [],
  balances: {},
  failovers: [],
  routes: {},
  log: [],
  dropped: 0,
};

const subscribers = new Set<() => void>();
let pending: Batch[] = [];
let frame = 0;

function push<T>(arr: T[], items: T[], cap: number): T[] {
  const next = arr.concat(items);
  return next.length > cap ? next.slice(next.length - cap) : next;
}

function flush() {
  frame = 0;
  const batches = pending;
  pending = [];
  let s = state;
  for (const b of batches) {
    const metrics = b.metrics ?? s.metrics;
    const failovers = b.events.filter((e): e is Extract<NodeEvent, { type: "failover" }> => e.type === "failover");
    const routes = { ...s.routes };
    for (const e of b.events) if (e.type === "routeChanged") routes[e.virtualIp] = { peer: e.peer, endpoint: e.endpoint };
    s = {
      metrics,
      cpuHistory: b.metrics ? push(s.cpuHistory, [b.metrics.cpuLoadPct], HISTORY) : s.cpuHistory,
      netHistory: b.metrics ? push(s.netHistory, [b.metrics.netRxBps + b.metrics.netTxBps], HISTORY) : s.netHistory,
      balances: { ...s.balances, ...b.balances },
      failovers: push(s.failovers, failovers, LOG),
      routes,
      log: push(s.log, b.events, LOG),
      dropped: s.dropped + b.dropped,
    };
  }
  state = s;
  for (const fn of subscribers) fn();
}

function enqueue(b: Batch) {
  pending.push(b);
  if (!frame) frame = requestAnimationFrame(flush);
}

let started = false;
/** Start listening once; returns an unlisten function. */
export async function startEventBridge(): Promise<() => void> {
  if (started) return () => {};
  started = true;
  if (!inTauri) return startMockPump(enqueue);
  return listen<Batch>(BATCH_EVENT, (e) => enqueue(e.payload));
}

function subscribe(fn: () => void) {
  subscribers.add(fn);
  return () => subscribers.delete(fn);
}

/** Subscribe a component to a slice of live state. Keep selectors cheap and referentially stable. */
export function useLive<T>(select: (s: LiveState) => T): T {
  return useSyncExternalStore(subscribe, () => select(state));
}
