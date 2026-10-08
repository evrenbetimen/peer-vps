import { useCallback, useEffect, useState } from "react";

import { commands } from "../bridge/commands";
import { useLive } from "../bridge/events";
import type { HostAllocation, HostSnapshot } from "../bridge/types";
import { Button, Card, ErrorNote, Slider, Sparkline, Stat, Toggle } from "../components/ui";
import { bytesPerSec, credits, mib, perHour } from "../lib/format";

export function HostDashboard() {
  const [snap, setSnap] = useState<HostSnapshot | null>(null);
  const [draft, setDraft] = useState<HostAllocation | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const metrics = useLive((s) => s.metrics);
  const cpuHistory = useLive((s) => s.cpuHistory);
  const netHistory = useLive((s) => s.netHistory);
  const liveEarnings = useLive((s) => (snap ? s.balances[snap.collateral.provider] : undefined));

  const refresh = useCallback(async () => {
    try {
      const s = await commands.getHostSnapshot();
      setSnap(s);
      setDraft((d) => d ?? s.allocation);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    const id = setInterval(() => void refresh(), 3000);
    return () => clearInterval(id);
  }, [refresh]);

  const apply = async () => {
    if (!draft) return;
    setSaving(true);
    setError(null);
    try {
      await commands.setHostAllocation(draft);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  if (!snap || !draft) return <p className="text-slate-400">Loading host…</p>;
  const temp = metrics?.cpuTempC ?? 0;
  const sla = metrics?.slaPct ?? 100;

  return (
    <div className="grid gap-4 xl:grid-cols-[360px_1fr]">
      <Card title="Resources offered to the network">
        <div className="space-y-5">
          <Slider label="Max cores" value={draft.maxCores} min={1} max={Math.max(snap.hostCores, draft.maxCores)} format={(v) => `${v} / ${snap.hostCores}`} onChange={(maxCores) => setDraft({ ...draft, maxCores })} />
          <Slider label="Max RAM" value={draft.maxMemMib} min={512} max={Math.max(snap.hostMemMib, draft.maxMemMib)} step={512} format={mib} onChange={(maxMemMib) => setDraft({ ...draft, maxMemMib })} />
          <Slider label="Max storage" value={draft.maxDiskGib} min={10} max={2000} step={10} format={(v) => `${v} GiB`} onChange={(maxDiskGib) => setDraft({ ...draft, maxDiskGib })} />
          <Slider
            label="Price per core"
            value={draft.pricePerCoreSec}
            min={10}
            max={2000}
            step={10}
            format={(v) => perHour(v)}
            onChange={(pricePerCoreSec) => setDraft({ ...draft, pricePerCoreSec })}
          />
          <Toggle label="Offer GPU slices" checked={draft.gpuEnabled} onChange={(gpuEnabled) => setDraft({ ...draft, gpuEnabled })} />
          <div className="flex items-center justify-between pt-1">
            <span className="text-xs text-slate-500">Hypervisor: {snap.hypervisor}</span>
            <Button onClick={() => void apply()} disabled={saving}>
              {saving ? "Applying…" : "Apply"}
            </Button>
          </div>
          <ErrorNote error={error} />
        </div>
      </Card>

      <div className="space-y-4">
        <div className="grid gap-4 sm:grid-cols-2 2xl:grid-cols-4">
          <Stat label="CPU temperature" value={temp ? `${temp.toFixed(1)} °C` : "n/a"} tone={temp > 85 ? "bad" : temp > 70 ? "warn" : "default"} />
          <Stat label="SLA (30 d)" value={`${sla.toFixed(2)} %`} tone={sla >= 99.9 ? "good" : sla >= 99 ? "warn" : "bad"} />
          <Stat
            label="Collateral"
            value={`${credits(snap.collateral.locked, 0)} cr`}
            hint={snap.collateral.eligible ? `eligible (min ${credits(snap.collateral.minimum, 0)})` : `below minimum ${credits(snap.collateral.minimum, 0)}`}
            tone={snap.collateral.eligible ? "good" : "bad"}
          />
          <Stat label="Earnings" value={`${credits(liveEarnings ?? snap.earnings)} cr`} hint="provider balance, settled per second" />
        </div>

        <div className="grid gap-4 lg:grid-cols-2">
          <Card title={`CPU load ${metrics ? `· ${metrics.cpuLoadPct.toFixed(0)} %` : ""}`}>
            <Sparkline values={cpuHistory} max={100} className="h-24 w-full" />
          </Card>
          <Card title={`Network ${metrics ? `· ${bytesPerSec(metrics.netRxBps + metrics.netTxBps)}` : ""}`}>
            <Sparkline values={netHistory} className="h-24 w-full" />
          </Card>
        </div>

        <Card title={`Guest MicroVMs · ${snap.vms.length}`}>
          {snap.vms.length === 0 ? (
            <p className="text-sm text-slate-500">No guests yet. Deploy one from the Renter console against “this-machine”.</p>
          ) : (
            <table className="w-full text-left text-sm">
              <thead className="text-xs uppercase text-slate-500">
                <tr>
                  <th className="py-1">VM</th>
                  <th>Image</th>
                  <th>Pinned cores</th>
                  <th>RAM</th>
                  <th>Accel</th>
                  <th>State</th>
                </tr>
              </thead>
              <tbody className="font-mono">
                {snap.vms.map((vm) => (
                  <tr key={vm.id} className="border-t border-slate-800">
                    <td className="py-1.5">{vm.id.slice(0, 8)}</td>
                    <td>{vm.spec.image}</td>
                    <td>{vm.placement.pinnedCores.join(",")}</td>
                    <td>{mib(vm.placement.memMib)}</td>
                    <td>{vm.placement.accelerator ? `${vm.placement.accelerator.slices}× ${vm.placement.accelerator.deviceId}` : "—"}</td>
                    <td>{vm.state}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          <p className="mt-3 text-xs text-slate-500">
            Free: {snap.freeCores} cores · {mib(snap.freeMemMib)} · {snap.freeDiskGib} GiB
          </p>
        </Card>
      </div>
    </div>
  );
}
