import { useCallback, useEffect, useState } from "react";

import { commands } from "../bridge/commands";
import type { PeerInfo, PeerOverview, PeerStatus } from "../bridge/types";
import { Button, Card, ErrorNote } from "./ui";
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

  const copyInvite = () => {
    if (!view?.invite) return;
    void navigator.clipboard?.writeText(view.invite).then(() => {
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
          <div className="text-xs text-slate-400">Give this to another PeerVPS so it can add this machine</div>
          {view.invite ? (
            <button type="button" onClick={copyInvite} className="font-mono text-cyan-300 hover:underline" title="Copy">
              {view.invite}
            </button>
          ) : (
            <span className="text-slate-500">not accepting peers</span>
          )}
          {copied && <span className="ml-2 text-xs text-emerald-400">copied</span>}
        </div>

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
