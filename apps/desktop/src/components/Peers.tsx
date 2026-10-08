import { useCallback, useEffect, useState } from "react";

import { commands } from "../bridge/commands";
import type { InternetStatus, PeerInfo, PeerOverview, PeerStatus } from "../bridge/types";
import { Button, Card, ErrorNote, Toggle } from "./ui";
import { cx } from "../lib/format";

const STATUS: Record<PeerStatus, { label: string; tone: string }> = {
  online: { label: "online", tone: "text-emerald-400" },
  waitingForApproval: { label: "waiting for their approval", tone: "text-amber-300" },
  unreachable: { label: "unreachable", tone: "text-rose-400" },
  pending: { label: "wants to rent from you", tone: "text-cyan-300" },
  inbound: { label: "can rent from you", tone: "text-slate-300" },
};

/** Other PeerVPS machines this one rents from and to. */
export function Peers() {
  const [view, setView] = useState<PeerOverview | null>(null);
  const [address, setAddress] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setView(await commands.getPeers());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    const id = setInterval(() => void refresh(), 3000);
    return () => clearInterval(id);
  }, [refresh]);

  const run = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const add = () =>
    void run(async () => {
      await commands.addPeer(address);
      setAddress("");
    });

  const copy = (text: string | null) => {
    if (!text) return;
    void navigator.clipboard?.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    });
  };

  if (!view) return <Card title="Peers">{error ? <ErrorNote error={error} /> : <p className="text-sm text-slate-500">Loading peers…</p>}</Card>;
  const pending = view.peers.filter((p) => p.status === "pending");
  const others = view.peers.filter((p) => p.status !== "pending");

  return (
    <Card title={`Peers · ${others.length}`}>
      <div className="space-y-3 text-sm">
        <div>
          <div className="text-xs text-slate-400">Give this to another PeerVPS on this network so it can add this machine</div>
          {view.invite ? (
            <button type="button" onClick={() => copy(view.invite)} className="font-mono text-cyan-300 hover:underline" title="Copy">
              {view.invite}
            </button>
          ) : (
            <span className="text-slate-500">not accepting peers</span>
          )}
          {copied && <span className="ml-2 text-xs text-emerald-400">copied</span>}
        </div>

        <div className="space-y-1">
          <Toggle
            label="Reachable from other networks"
            checked={view.internet.state !== "off"}
            onChange={(on) => void run(() => commands.setInternet(on))}
          />
          <InternetLine status={view.internet} invite={view.internetInvite} onCopy={() => copy(view.internetInvite)} />
        </div>

        {view.nearby.length > 0 && (
          <div>
            <div className="text-xs text-slate-400">On this network</div>
            <ul>
              {view.nearby.map((n) => (
                <li key={n.id} className="flex items-center justify-between gap-2 py-1" data-testid={`nearby-${n.id}`}>
                  <span className="font-mono">{n.id}</span>
                  <Button variant="ghost" disabled={busy} onClick={() => void run(() => commands.addPeer(n.invite))}>
                    Add
                  </Button>
                </li>
              ))}
            </ul>
          </div>
        )}

        <form
          className="flex gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            add();
          }}
        >
          <input
            aria-label="Peer address"
            value={address}
            onChange={(e) => setAddress(e.target.value)}
            placeholder="pv-…@192.168.1.20:7071"
            className="min-w-0 flex-1 rounded-lg border border-slate-700 bg-slate-950 px-2 py-1.5 font-mono text-xs text-slate-100 placeholder:text-slate-600"
          />
          <Button type="submit" disabled={busy || !address.trim()}>
            Add peer
          </Button>
        </form>

        {pending.map((p) => (
          <div key={p.id} className="flex items-center justify-between gap-2 rounded-lg border border-cyan-500/30 bg-cyan-500/5 px-3 py-2" data-testid={`peer-${p.id}`}>
            <div>
              <div className="font-mono">{p.id}</div>
              <div className="text-xs text-slate-400">wants to rent from you{p.address ? ` · ${p.address}` : ""}</div>
            </div>
            <div className="flex gap-1.5">
              <Button onClick={() => void run(() => commands.approvePeer(p.id))} disabled={busy}>
                Approve
              </Button>
              <Button variant="ghost" onClick={() => void run(() => commands.removePeer(p.id))} disabled={busy}>
                Ignore
              </Button>
            </div>
          </div>
        ))}

        <ul className="divide-y divide-slate-800">
          {others.map((p) => (
            <PeerRow key={p.id} peer={p} busy={busy} onRemove={() => void run(() => commands.removePeer(p.id))} />
          ))}
          {others.length === 0 && <li className="py-2 text-slate-500">No peers yet. Add another machine by its invite; its offers then show up in the Console.</li>}
        </ul>
        <ErrorNote error={error} />
      </div>
    </Card>
  );
}

function InternetLine({ status, invite, onCopy }: { status: InternetStatus; invite: string | null; onCopy: () => void }) {
  switch (status.state) {
    case "off":
      return <p className="text-xs text-slate-500">Off: only machines on this network can add this one. Turning it on asks the router (UPnP) to forward a port here.</p>;
    case "checking":
      return <p className="text-xs text-slate-400">Asking the router…</p>;
    case "open":
      return (
        <div className="text-xs">
          <span className="text-slate-400">Give this to machines on other networks: </span>
          <button type="button" onClick={onCopy} className="font-mono text-cyan-300 hover:underline" title="Copy">
            {invite}
          </button>
        </div>
      );
    default:
      return <p className="text-xs text-amber-300">{status.detail ?? "Not reachable from other networks."}</p>;
  }
}

function PeerRow({ peer, busy, onRemove }: { peer: PeerInfo; busy: boolean; onRemove: () => void }) {
  const s = STATUS[peer.status];
  return (
    <li className="flex items-center justify-between gap-2 py-2" data-testid={`peer-${peer.id}`}>
      <div className="min-w-0">
        <div className="font-mono">{peer.id}</div>
        <div className="text-xs text-slate-400">
          <span className={cx(s.tone)}>{s.label}</span>
          {peer.address && ` · ${peer.address}`}
          {peer.status === "online" && ` · ${peer.offers.length} offer${peer.offers.length === 1 ? "" : "s"} in the Console`}
        </div>
        {peer.error && peer.status === "unreachable" && <div className="truncate text-xs text-rose-400/80">{peer.error}</div>}
      </div>
      <Button variant="ghost" onClick={onRemove} disabled={busy}>
        Remove
      </Button>
    </li>
  );
}
