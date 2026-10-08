import { useEffect, useState } from "react";

import { inTauri } from "./bridge/commands";
import { startEventBridge, useLive } from "./bridge/events";
import { cx } from "./lib/format";
import { Failover } from "./views/Failover";
import { HostDashboard } from "./views/HostDashboard";
import { RenterConsole } from "./views/RenterConsole";
import { Wallet } from "./views/Wallet";

const VIEWS = [
  { id: "host", label: "Host", sub: "Provider mode", el: <HostDashboard /> },
  { id: "renter", label: "Console", sub: "Renter & agents", el: <RenterConsole /> },
  { id: "wallet", label: "Wallet", sub: "Credits & billing", el: <Wallet /> },
  { id: "failover", label: "Failover", sub: "Topology & HA", el: <Failover /> },
] as const;

type ViewId = (typeof VIEWS)[number]["id"];

export function App() {
  const [view, setView] = useState<ViewId>("host");
  const dropped = useLive((s) => s.dropped);

  useEffect(() => {
    let stop: (() => void) | undefined;
    void startEventBridge().then((fn) => (stop = fn));
    return () => stop?.();
  }, []);

  const current = VIEWS.find((v) => v.id === view) ?? VIEWS[0];
  return (
    <div className="flex h-full">
      <nav className="flex w-56 shrink-0 flex-col border-r border-slate-800 bg-slate-950 p-3">
        <div className="mb-6 px-2 pt-2">
          <div className="bg-gradient-to-r from-cyan-400 to-indigo-400 bg-clip-text text-lg font-bold text-transparent">PeerVPS</div>
          <div className="text-xs text-slate-500">{inTauri ? "desktop node" : "browser preview (mock data)"}</div>
        </div>
        {VIEWS.map((v) => (
          <button
            key={v.id}
            type="button"
            aria-current={v.id === view ? "page" : undefined}
            onClick={() => setView(v.id)}
            className={cx("mb-1 rounded-lg px-3 py-2 text-left transition", v.id === view ? "bg-slate-800 text-white" : "text-slate-400 hover:bg-slate-900")}
          >
            <div className="text-sm font-medium">{v.label}</div>
            <div className="text-xs text-slate-500">{v.sub}</div>
          </button>
        ))}
        {dropped > 0 && <div className="mt-auto px-2 text-xs text-amber-400">{dropped} events dropped</div>}
      </nav>
      <main className="min-w-0 flex-1 overflow-y-auto p-6">
        <h1 className="mb-4 text-xl font-semibold">{current.sub}</h1>
        {current.el}
      </main>
    </div>
  );
}
