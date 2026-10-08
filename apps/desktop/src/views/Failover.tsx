import { useEffect, useMemo, useState } from "react";

import { commands } from "../bridge/commands";
import { useLive } from "../bridge/events";
import type { FailoverPhase, Topology, TopologyNode } from "../bridge/types";
import { Button, Card, ErrorNote } from "../components/ui";
import { cx } from "../lib/format";

const POS: Record<string, { x: number; y: number }> = {
  client: { x: 120, y: 200 },
  "host-b": { x: 560, y: 90 },
  "host-c": { x: 560, y: 310 },
};

const PHASE_STYLE: Record<FailoverPhase | "healthy", { ring: string; label: string }> = {
  healthy: { ring: "stroke-emerald-400", label: "healthy" },
  recovered: { ring: "stroke-emerald-400", label: "recovered" },
  suspect: { ring: "stroke-amber-400", label: "suspect" },
  down: { ring: "stroke-rose-500", label: "down" },
  rerouted: { ring: "stroke-rose-500", label: "down · rerouted" },
  stranded: { ring: "stroke-rose-500", label: "down · no standby" },
};

export function Failover() {
  const [topo, setTopo] = useState<Topology | null>(null);
  const [error, setError] = useState<string | null>(null);
  const failovers = useLive((s) => s.failovers);
  const routes = useLive((s) => s.routes);

  useEffect(() => {
    commands.getTopology().then(setTopo, (e) => setError(String(e)));
  }, []);

  const phaseOf = useMemo(() => {
    const m: Record<string, FailoverPhase> = {};
    for (const t of failovers) m[t.peer] = t.phase;
    return m;
  }, [failovers]);

  if (!topo) return <p className="text-slate-400">Loading topology…</p>;
  const active = routes[topo.virtualIp]?.peer ?? topo.activePeer;
  const hosts = topo.nodes.filter((n) => n.role !== "client");

  const act = async (fn: () => Promise<Topology>) => {
    setError(null);
    try {
      setTopo(await fn());
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="grid gap-4 xl:grid-cols-[1fr_360px]">
      <Card title={`Overlay path to ${topo.virtualIp}`}>
        <svg viewBox="0 0 700 400" className="h-[420px] w-full">
          {/* async block replication primary → standby */}
          <line x1={POS["host-b"]!.x} y1={POS["host-b"]!.y} x2={POS["host-c"]!.x} y2={POS["host-c"]!.y} className="stroke-violet-500/50" strokeWidth={2} strokeDasharray="4 6" />
          <text x={POS["host-b"]!.x + 14} y={200} className="fill-violet-300 text-[11px]">
            block replication
          </text>
          {hosts.map((h) => {
            const isActive = h.id === active;
            const p = POS[h.id]!;
            return (
              <line
                key={`link-${h.id}`}
                x1={POS.client!.x}
                y1={POS.client!.y}
                x2={p.x}
                y2={p.y}
                strokeWidth={isActive ? 3 : 1.5}
                strokeDasharray={isActive ? "10 8" : "2 8"}
                className={cx(isActive ? "stroke-cyan-400 [animation:flow_0.8s_linear_infinite]" : "stroke-slate-700", "transition-all duration-500")}
              />
            );
          })}
          {topo.nodes.map((n) => (
            <NodeGlyph key={n.id} node={n} phase={phaseOf[n.id]} active={n.id === active} />
          ))}
        </svg>
        <div className="mt-2 flex flex-wrap gap-2">
          {hosts.map((h) => (
            <Button key={h.id} variant={h.alive ? "danger" : "ghost"} onClick={() => void act(() => (h.alive ? commands.killPeer(h.id) : commands.restorePeer(h.id)))}>
              {h.alive ? `Kill ${h.label}` : `Restore ${h.label}`}
            </Button>
          ))}
        </div>
        <p className="mt-2 text-xs text-slate-500">
          Heartbeats every {topo.heartbeatMs} ms; three misses mark a host down and the route flips to the standby.
        </p>
        <ErrorNote error={error} />
      </Card>

      <Card title="Transitions">
        <ol className="space-y-1.5 font-mono text-xs">
          {[...failovers].reverse().map((t, i) => (
            <li key={`${t.atMs}-${i}`} className="flex justify-between gap-2">
              <span className="text-slate-500">{new Date(t.atMs).toLocaleTimeString()}</span>
              <span className="flex-1">{t.peer}</span>
              <span className={cx(t.phase === "suspect" ? "text-amber-400" : ["down", "rerouted", "stranded"].includes(t.phase) ? "text-rose-400" : "text-emerald-400")}>
                {t.phase}
                {t.phase === "suspect" ? ` ×${t.missedHeartbeats}` : ""}
                {t.movedVips.length ? ` → ${t.movedVips.join(",")}` : ""}
              </span>
            </li>
          ))}
          {failovers.length === 0 && <li className="text-slate-500">All peers healthy. Kill one to watch the failover.</li>}
        </ol>
      </Card>
    </div>
  );
}

function NodeGlyph({ node, phase, active }: { node: TopologyNode; phase: FailoverPhase | undefined; active: boolean }) {
  const p = POS[node.id] ?? { x: 0, y: 0 };
  const style = PHASE_STYLE[node.role === "client" ? "healthy" : (phase ?? (node.alive ? "healthy" : "down"))];
  return (
    <g transform={`translate(${p.x} ${p.y})`} className="transition-transform duration-500">
      <circle r={42} className={cx("fill-slate-900", style.ring, phase === "suspect" && "animate-pulse")} strokeWidth={active ? 5 : 3} />
      <text textAnchor="middle" y={-4} className="fill-slate-100 text-[13px] font-semibold">
        {node.label}
      </text>
      <text textAnchor="middle" y={13} className="fill-slate-400 text-[10px]">
        {node.role === "client" ? node.endpoint : node.role}
      </text>
      {node.role !== "client" && (
        <text textAnchor="middle" y={62} className="fill-slate-300 text-[11px]">
          {active ? "serving · " : ""}
          {style.label}
        </text>
      )}
    </g>
  );
}
